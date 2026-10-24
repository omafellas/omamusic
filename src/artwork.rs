use async_channel::{Receiver, Sender, TrySendError};
use gpui_kit::{Image, ImageFormat};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

type Key = (String, u32);
type Provider = Arc<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync + 'static>;

#[derive(Default)]
struct Covers {
    raw: HashMap<String, Arc<[u8]>>,
    rendered: HashMap<Key, Arc<Image>>,
    pending: HashSet<Key>,
    /// Albums the store reported as having no cover, so the store is not
    /// queried again until the album is invalidated.
    missing: HashSet<String>,
    generation: u64,
}

/// Raw covers and rendered thumbnails. Raw bytes are loaded lazily from the
/// store (SQLite) the first time an album cover is requested, so startup does
/// not have to read every cover.
#[derive(Clone)]
pub struct ArtworkCache {
    inner: Arc<Mutex<Covers>>,
    requests: Sender<Key>,
    listeners: Arc<Mutex<Vec<Sender<()>>>>,
    provider: Arc<Mutex<Option<Provider>>>,
}

impl Default for ArtworkCache {
    fn default() -> Self {
        let inner = Arc::new(Mutex::new(Covers::default()));
        let (requests, jobs) = async_channel::unbounded::<Key>();
        let listeners: Arc<Mutex<Vec<Sender<()>>>> = Arc::default();
        let workers = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(1, 4));
        for _ in 0..workers {
            let (inner, jobs, listeners) = (inner.clone(), jobs.clone(), listeners.clone());
            std::thread::spawn(move || {
                while let Ok(key) = jobs.recv_blocking() {
                    render_into(&inner, key);
                    listeners.lock().retain(|listener| {
                        !matches!(listener.try_send(()), Err(TrySendError::Closed(_)))
                    });
                }
            });
        }
        Self {
            inner,
            requests,
            listeners,
            provider: Arc::new(Mutex::new(None)),
        }
    }
}

fn render(raw: &[u8], size: u32) -> Option<Arc<Image>> {
    let mut png = Vec::new();
    image::load_from_memory(raw)
        .ok()?
        .thumbnail(size, size)
        .to_rgba8()
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(Arc::new(Image::from_bytes(ImageFormat::Png, png)))
}

fn render_into(inner: &Mutex<Covers>, key: Key) {
    let (raw, generation) = {
        let covers = inner.lock();
        if covers.rendered.contains_key(&key) {
            return;
        }
        (covers.raw.get(&key.0).cloned(), covers.generation)
    };
    let image = raw.and_then(|raw| render(&raw, key.1));
    let mut covers = inner.lock();
    if let Some(image) = image.filter(|_| covers.generation == generation) {
        covers.pending.remove(&key);
        covers.rendered.insert(key, image);
    }
}

impl ArtworkCache {
    /// Install the store that raw covers are loaded from on demand.
    pub fn set_provider(&self, provider: Provider) {
        *self.provider.lock() = Some(provider);
    }

    /// Forget one album's cover (its file or tags changed).
    pub fn remove(&self, album_id: &str) {
        let mut covers = self.inner.lock();
        covers.raw.remove(album_id);
        covers.missing.remove(album_id);
        covers.rendered.retain(|(id, _), _| id != album_id);
        covers.pending.retain(|(id, _)| id != album_id);
        covers.generation += 1;
    }

    /// Forget every cover (folders or albums changed wholesale).
    pub fn clear(&self) {
        let mut covers = self.inner.lock();
        covers.raw.clear();
        covers.rendered.clear();
        covers.pending.clear();
        covers.missing.clear();
        covers.generation += 1;
    }

    fn load(&self, album_id: &str) -> Option<Arc<[u8]>> {
        {
            let covers = self.inner.lock();
            if covers.missing.contains(album_id) {
                return None;
            }
            if let Some(raw) = covers.raw.get(album_id) {
                return Some(raw.clone());
            }
        }
        let provider = self.provider.lock();
        let raw = provider.as_ref().and_then(|p| p(album_id));
        drop(provider);
        let mut covers = self.inner.lock();
        match raw {
            Some(raw) => Some(
                covers
                    .raw
                    .entry(album_id.to_string())
                    .or_insert_with(|| raw.into())
                    .clone(),
            ),
            None => {
                covers.missing.insert(album_id.to_string());
                None
            }
        }
    }

    pub fn cached(&self, album_id: &str, size: u32) -> Option<Arc<Image>> {
        let key = (album_id.to_string(), size);
        {
            let mut covers = self.inner.lock();
            if let Some(image) = covers.rendered.get(&key) {
                return Some(image.clone());
            }
            if covers.missing.contains(album_id) {
                return None;
            }
            if covers.raw.contains_key(album_id) {
                if covers.pending.insert(key.clone()) {
                    let _ = self.requests.try_send(key);
                }
                return None;
            }
        }
        self.load(album_id)?;
        let mut covers = self.inner.lock();
        if let Some(image) = covers.rendered.get(&key) {
            return Some(image.clone());
        }
        if covers.pending.insert(key.clone()) {
            let _ = self.requests.try_send(key);
        }
        None
    }

    pub fn thumbnail(&self, album_id: &str, size: u32) -> Option<Arc<Image>> {
        let key = (album_id.to_string(), size);
        let raw = {
            let covers = self.inner.lock();
            if let Some(image) = covers.rendered.get(&key) {
                return Some(image.clone());
            }
            if covers.missing.contains(album_id) {
                return None;
            }
            covers.raw.get(album_id).cloned()
        };
        let raw = match raw {
            Some(raw) => raw,
            None => self.load(album_id)?,
        };
        let generation = self.inner.lock().generation;
        let image = render(&raw, size)?;
        let mut covers = self.inner.lock();
        if covers.generation == generation {
            covers.pending.remove(&key);
            covers.rendered.insert(key, image.clone());
        }
        Some(image)
    }

    pub fn subscribe(&self) -> Receiver<()> {
        let (sender, receiver) = async_channel::bounded(1);
        self.listeners.lock().push(sender);
        receiver
    }
}
