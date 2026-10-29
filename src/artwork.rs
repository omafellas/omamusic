use gpui_kit::RenderImage;
use image::{Frame, RgbaImage};
use parking_lot::{Condvar, Mutex};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

type Key = (String, u32);
type Provider = Arc<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync + 'static>;

/// How long a freshly rendered thumbnail reports `fresh`, so the view can
/// fade it in instead of popping it over the placeholder.
const FRESH_FOR: Duration = Duration::from_millis(400);

#[derive(Clone)]
pub struct Thumbnail {
    pub image: Arc<RenderImage>,
    pub fresh: bool,
}

#[derive(Default)]
struct Covers {
    raw: HashMap<String, Arc<[u8]>>,
    rendered: HashMap<Key, (Arc<RenderImage>, Instant)>,
    pending: HashSet<Key>,
    working: HashSet<Key>,
    /// Albums the store reported as having no cover, so the store is not
    /// queried again until the album is invalidated.
    missing: HashSet<String>,
    generation: u64,
}

#[derive(Default)]
struct Queue {
    jobs: Mutex<Vec<Key>>,
    wake: Condvar,
}

/// Raw covers and rendered thumbnails. Everything slow happens on the worker
/// threads: reading the raw bytes from the store (SQLite), decoding and
/// scaling. The UI thread only looks the finished thumbnail up. Requests are
/// served newest first, so while scrolling the rows on screen win over the
/// rows that already scrolled past.
#[derive(Clone)]
pub struct ArtworkCache {
    inner: Arc<Mutex<Covers>>,
    queue: Arc<Queue>,
    provider: Arc<Mutex<Option<Provider>>>,
}

impl Default for ArtworkCache {
    fn default() -> Self {
        Self::with_workers(std::thread::available_parallelism().map_or(2, |n| n.get().clamp(1, 4)))
    }
}

fn decode(raw: &[u8], size: u32) -> Option<RgbaImage> {
    let reader = image::ImageReader::new(std::io::Cursor::new(raw))
        .with_guessed_format()
        .ok()?;
    Some(reader.decode().ok()?.thumbnail(size, size).into_rgba8())
}

fn render(raw: &[u8], size: u32) -> Option<Arc<RenderImage>> {
    let mut data = decode(raw, size)?;
    for pixel in data.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new(vec![Frame::new(data)])))
}

fn load_raw(
    inner: &Mutex<Covers>,
    provider: &Mutex<Option<Provider>>,
    album_id: &str,
) -> Option<Arc<[u8]>> {
    {
        let covers = inner.lock();
        if covers.missing.contains(album_id) {
            return None;
        }
        if let Some(raw) = covers.raw.get(album_id) {
            return Some(raw.clone());
        }
    }
    let provider = provider.lock().clone();
    let raw = provider.as_ref().and_then(|p| p(album_id));
    let mut covers = inner.lock();
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

fn work(inner: &Mutex<Covers>, provider: &Mutex<Option<Provider>>, key: Key) {
    let generation = {
        let mut covers = inner.lock();
        if covers.rendered.contains_key(&key) || !covers.working.insert(key.clone()) {
            return;
        }
        covers.generation
    };
    let image = load_raw(inner, provider, &key.0).and_then(|raw| render(&raw, key.1));
    let mut covers = inner.lock();
    covers.working.remove(&key);
    if covers.generation != generation {
        return;
    }
    covers.pending.remove(&key);
    if let Some(image) = image {
        covers.rendered.insert(key, (image, Instant::now()));
    }
}

fn next_job(queue: &Queue, alive: &Weak<Mutex<Covers>>) -> Option<Key> {
    let mut jobs = queue.jobs.lock();
    loop {
        if let Some(key) = jobs.pop() {
            return Some(key);
        }
        if alive.strong_count() == 0 {
            return None;
        }
        queue.wake.wait_for(&mut jobs, Duration::from_secs(1));
    }
}

impl ArtworkCache {
    pub fn with_workers(workers: usize) -> Self {
        let inner = Arc::new(Mutex::new(Covers::default()));
        let queue = Arc::new(Queue::default());
        let provider: Arc<Mutex<Option<Provider>>> = Arc::default();
        for _ in 0..workers.max(1) {
            let alive = Arc::downgrade(&inner);
            let (queue, provider) = (queue.clone(), provider.clone());
            std::thread::spawn(move || {
                while let Some(key) = next_job(&queue, &alive) {
                    let Some(inner) = alive.upgrade() else { break };
                    work(&inner, &provider, key);
                }
            });
        }
        Self {
            inner,
            queue,
            provider,
        }
    }

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

    fn enqueue(&self, key: Key) {
        let mut jobs = self.queue.jobs.lock();
        jobs.retain(|job| *job != key);
        jobs.push(key);
        self.queue.wake.notify_one();
    }

    /// Thumbnails requested but not rendered yet. A view that got `None`
    /// from [`cached`](Self::cached) polls this to know when to draw again.
    pub fn pending_count(&self) -> usize {
        self.inner.lock().pending.len()
    }

    /// The thumbnail if it is ready. Otherwise the request moves to the front
    /// of the queue and `None` comes back at once.
    pub fn cached(&self, album_id: &str, size: u32) -> Option<Thumbnail> {
        let key = (album_id.to_string(), size);
        let mut covers = self.inner.lock();
        if let Some((image, since)) = covers.rendered.get(&key) {
            return Some(Thumbnail {
                image: image.clone(),
                fresh: since.elapsed() < FRESH_FOR,
            });
        }
        if covers.missing.contains(album_id) {
            return None;
        }
        if covers.working.contains(&key) {
            return None;
        }
        covers.pending.insert(key.clone());
        drop(covers);
        self.enqueue(key);
        None
    }

    /// Decode a thumbnail on the calling thread, for callers off the UI
    /// thread that need the pixels (tray icon, MPRIS art).
    pub fn thumbnail(&self, album_id: &str, size: u32) -> Option<RgbaImage> {
        let raw = load_raw(&self.inner, &self.provider, album_id)?;
        decode(&raw, size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::ThreadId;
    use std::time::{Duration, Instant};

    fn png(seed: u8) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbaImage::from_pixel(48, 48, image::Rgba([seed, 40, 200, 255]))
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    fn wait_for(cache: &ArtworkCache, id: &str, size: u32) -> Thumbnail {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(thumb) = cache.cached(id, size) {
                return thumb;
            }
            assert!(
                Instant::now() < deadline,
                "thumbnail for {id} never arrived"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn covers_load_and_decode_off_the_calling_thread() {
        let cache = ArtworkCache::with_workers(2);
        let seen: Arc<Mutex<Vec<ThreadId>>> = Arc::default();
        let log = seen.clone();
        cache.set_provider(Arc::new(move |_| {
            log.lock().push(std::thread::current().id());
            Some(png(1))
        }));
        assert!(cache.cached("a", 16).is_none(), "first ask only enqueues");
        let thumb = wait_for(&cache, "a", 16);
        assert!(thumb.fresh);
        assert_eq!(
            thumb.image.size(0),
            gpui_kit::size(gpui_kit::DevicePixels(16), gpui_kit::DevicePixels(16))
        );
        let me = std::thread::current().id();
        let seen = seen.lock();
        assert_eq!(seen.len(), 1, "raw cover is read once");
        assert!(
            seen.iter().all(|&t| t != me),
            "the store is never read on the UI thread"
        );
    }

    #[test]
    fn newest_request_is_rendered_first() {
        let cache = ArtworkCache::with_workers(1);
        let (gate_tx, gate_rx) = async_channel::bounded::<()>(1);
        let order: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = order.clone();
        cache.set_provider(Arc::new(move |id| {
            if id == "gate" {
                let _ = gate_rx.recv_blocking();
            }
            log.lock().push(id.to_string());
            Some(png(2))
        }));
        cache.cached("gate", 8);
        std::thread::sleep(Duration::from_millis(50));
        for id in ["a", "b", "c"] {
            cache.cached(id, 8);
        }
        gate_tx.send_blocking(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while order.lock().len() < 4 {
            assert!(Instant::now() < deadline, "workers never drained the queue");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*order.lock(), ["gate", "c", "b", "a"]);
    }

    #[test]
    fn missing_covers_are_asked_for_once() {
        let cache = ArtworkCache::with_workers(1);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n = calls.clone();
        cache.set_provider(Arc::new(move |_| {
            n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            None
        }));
        cache.cached("none", 8);
        std::thread::sleep(Duration::from_millis(100));
        cache.cached("none", 8);
        cache.cached("none", 16);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(cache.thumbnail("none", 8).is_none());
    }
}
