use crate::artwork::ArtworkCache;
use crate::config::AppConfig;
use crate::fuzzy::FuzzySearch;
use crate::metadata;
use crate::models::{Album, Track};
use anyhow::Result;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LibrarySnapshot {
    pub folders: Vec<PathBuf>,
    pub albums: Vec<Album>,
    pub filtered_albums: Arc<[Album]>,
    pub all_tracks: Arc<[Track]>,
    pub filtered_tracks: Arc<[Track]>,
    pub artists: Vec<(String, Vec<Album>)>,
    pub scanning: bool,
    pub query: String,
    pub version: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LibraryCache {
    folders: Vec<PathBuf>,
    albums: Vec<Album>,
    all_tracks: Vec<Track>,
    artists: Vec<(String, Vec<Album>)>,
    version: u64,
    timestamp: u64,
}

type SharedSnapshot = Arc<RwLock<Arc<LibrarySnapshot>>>;

pub struct Library {
    state: SharedSnapshot,
    covers: ArtworkCache,
    watcher: Mutex<Option<RecommendedWatcher>>,
    rescan_tx: Sender<()>,
}

fn update(state: &SharedSnapshot, change: impl FnOnce(&mut LibrarySnapshot)) {
    let mut guard = state.write();
    let snapshot = Arc::make_mut(&mut guard);
    change(snapshot);
    snapshot.version += 1;
}

fn rank<T: Clone>(items: &[T], limit: usize, score: impl Fn(&T) -> i32) -> Vec<T> {
    let mut scored: Vec<(i32, &T)> = items
        .iter()
        .map(|item| (score(item), item))
        .filter(|(score, _)| *score > 0)
        .collect();
    scored.sort_by_key(|(score, _)| Reverse(*score));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, item)| item.clone())
        .collect()
}

impl Library {
    pub fn new() -> Result<Self> {
        let folders = AppConfig::load().folders;
        let (rescan_tx, rx) = mpsc::channel::<()>();
        let watcher = Self::make_watcher(&folders, &rescan_tx)?;

        // Try to load cache for instant startup
        let temp_state = Arc::new(RwLock::new(Arc::new(LibrarySnapshot {
            folders: folders.clone(),
            ..Default::default()
        })));
        let covers = ArtworkCache::default();
        let mut lib = Self {
            state: temp_state,
            covers: covers.clone(),
            watcher: Mutex::new(None),
            rescan_tx: rescan_tx.clone(),
        };

        if let Some(cache) = lib.load_cache() {
            if cache.folders == folders {
                // Cache is valid for current folders, load it
                let mut snapshot = LibrarySnapshot {
                    folders: cache.folders,
                    albums: cache.albums,
                    all_tracks: cache.all_tracks.into(),
                    filtered_tracks: Arc::new([]),
                    filtered_albums: Arc::new([]),
                    artists: cache.artists,
                    scanning: false,
                    query: String::new(),
                    version: cache.version,
                };
                Self::apply_search(&mut snapshot);
                let state = Arc::new(RwLock::new(Arc::new(snapshot)));
                lib.state = state;
            }
        }

        let state = lib.state.clone();
        let covers = lib.covers.clone();
        let rescan_tx_clone = lib.rescan_tx.clone();

        std::thread::spawn(move || {
            Self::do_scan(&state, &covers);
            while rx.recv().is_ok() {
                std::thread::sleep(Duration::from_millis(800));
                while rx.try_recv().is_ok() {}
                Self::do_scan(&state, &covers);
            }
        });

        Ok(Self {
            state: lib.state,
            covers: lib.covers,
            watcher: Mutex::new(watcher),
            rescan_tx: rescan_tx_clone,
        })
    }

    fn cache_path() -> Option<PathBuf> {
        dirs::cache_dir().map(|d| d.join("omamusic").join("library_cache.json"))
    }

    fn load_cache(&self) -> Option<LibraryCache> {
        let path = Self::cache_path()?;
        let data = std::fs::read(path).ok()?;
        serde_json::from_slice(&data).ok()
    }

    fn save_cache(snapshot: &LibrarySnapshot) {
        if let Some(path) = Library::cache_path() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let cache = LibraryCache {
                folders: snapshot.folders.clone(),
                albums: snapshot.albums.clone(),
                all_tracks: snapshot.all_tracks.iter().cloned().collect(),
                artists: snapshot.artists.clone(),
                version: snapshot.version,
                timestamp: match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                {
                    Ok(d) => d.as_secs(),
                    Err(_) => 0,
                },
            };
            if let Ok(data) = serde_json::to_vec(&cache) {
                let _ = std::fs::write(path, data);
            }
        }
    }

    pub fn snapshot(&self) -> Arc<LibrarySnapshot> {
        self.state.read().clone()
    }

    pub fn version(&self) -> u64 {
        self.state.read().version
    }

    pub fn query(&self) -> String {
        self.state.read().query.clone()
    }

    pub fn covers(&self) -> &ArtworkCache {
        &self.covers
    }

    pub fn request_rescan(&self) {
        let _ = self.rescan_tx.send(());
    }

    #[cfg(test)]
    /// A library with no folders, watcher, or scanner thread; for view tests.
    pub fn empty() -> Self {
        let (rescan_tx, _rx) = mpsc::channel::<()>();
        Self {
            state: Arc::new(RwLock::new(Arc::new(LibrarySnapshot::default()))),
            covers: ArtworkCache::default(),
            watcher: Mutex::new(None),
            rescan_tx,
        }
    }

    pub fn set_query(&self, q: String) {
        if self.state.read().query == q {
            return;
        }
        update(&self.state, |s| {
            s.query = q;
            Self::apply_search(s);
        });
    }

    pub fn add_folder(&self, path: PathBuf) {
        if !path.is_dir() {
            eprintln!("music folder does not exist: {}", path.display());
            return;
        }
        {
            let s = self.state.read();
            if s.folders.iter().any(|f| path.starts_with(f)) {
                return;
            }
        }
        update(&self.state, |s| {
            s.folders.retain(|f| !f.starts_with(&path));
            s.folders.push(path);
        });
        self.folders_changed();
    }

    pub fn remove_folder(&self, path: &Path) {
        update(&self.state, |s| s.folders.retain(|f| f != path));
        self.folders_changed();
    }

    fn folders_changed(&self) {
        let folders = self.state.read().folders.clone();
        AppConfig::update(|c| c.folders = folders.clone());
        match Self::make_watcher(&folders, &self.rescan_tx) {
            Ok(watcher) => *self.watcher.lock() = watcher,
            Err(err) => eprintln!("watch music folders: {err:#}"),
        }
        self.request_rescan();
    }

    fn make_watcher(folders: &[PathBuf], tx: &Sender<()>) -> Result<Option<RecommendedWatcher>> {
        if folders.is_empty() {
            return Ok(None);
        }
        let tx = tx.clone();
        let mut watcher =
            notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    if matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_)
                    ) {
                        let _ = tx.send(());
                    }
                }
            })?;
        for folder in folders {
            if folder.is_dir() {
                watcher.watch(folder, RecursiveMode::Recursive)?;
            }
        }
        Ok(Some(watcher))
    }

    fn apply_search(s: &mut LibrarySnapshot) {
        if s.query.trim().is_empty() {
            s.filtered_albums = s.albums.as_slice().into();
            s.filtered_tracks = s.all_tracks.clone();
        } else {
            let search = FuzzySearch::new(&s.query);
            s.filtered_albums = rank(&s.albums, 300, |a| {
                search
                    .score(&a.title)
                    .max(search.score(&a.artist))
                    .max(search.score(&a.track_titles))
            })
            .into();
            s.filtered_tracks = rank(&s.all_tracks, 500, |t| {
                search
                    .score(&t.title)
                    .max(search.score(&t.artist))
                    .max(search.score(&t.album))
            })
            .into();
        }

        let mut map: HashMap<String, Vec<Album>> = HashMap::new();
        for a in s.filtered_albums.iter() {
            map.entry(a.artist.clone()).or_default().push(a.clone());
        }
        let mut artists: Vec<_> = map
            .into_iter()
            .map(|(name, mut albums)| {
                albums.sort_by(|a, b| a.year.cmp(&b.year).then_with(|| a.title.cmp(&b.title)));
                (name, albums)
            })
            .collect();
        artists.sort_by_key(|a| a.0.to_lowercase());
        s.artists = artists;
    }

    fn do_scan(state: &SharedSnapshot, covers: &ArtworkCache) {
        update(state, |s| s.scanning = true);
        let folders = state.read().folders.clone();

        let mut album_map: HashMap<String, Vec<Track>> = HashMap::new();
        let mut cover_map: HashMap<String, Vec<u8>> = HashMap::new();

        let paths = folders.iter().flat_map(|folder| {
            walkdir::WalkDir::new(folder)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file() && metadata::is_audio(e.path()))
                .map(walkdir::DirEntry::into_path)
        });
        for p in paths {
            let md = metadata::read(&p);
            let dir = p
                .parent()
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or_default();
            let album_name = if md.album.is_empty() {
                p.parent()
                    .and_then(|d| d.file_name())
                    .and_then(|s| s.to_str())
                    .unwrap_or("Unknown Album")
                    .to_string()
            } else {
                md.album
            };
            let key = format!("{dir}\n{album_name}");
            if !cover_map.contains_key(&key) {
                if let Some(art) = md.artwork.or_else(|| metadata::folder_artwork(&p)) {
                    cover_map.insert(key.clone(), art);
                }
            }
            let title = if md.title.is_empty() {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Unknown")
                    .to_string()
            } else {
                md.title
            };
            let artist = if md.artist.is_empty() {
                "Unknown Artist".to_string()
            } else {
                md.artist
            };
            album_map.entry(key.clone()).or_default().push(Track {
                path: p,
                title,
                artist,
                album: album_name,
                track_number: md.track_number,
                year: md.year,
                duration: md.duration.max(0.0),
                album_id: key,
            });
        }

        let mut albums: Vec<Album> = Vec::with_capacity(album_map.len());
        for (id, mut tracks) in album_map {
            tracks.sort_by(|a, b| {
                if a.track_number > 0 && b.track_number > 0 && a.track_number != b.track_number {
                    a.track_number.cmp(&b.track_number)
                } else {
                    a.path.cmp(&b.path)
                }
            });
            let artist = if tracks.iter().all(|t| t.artist == tracks[0].artist) {
                tracks[0].artist.clone()
            } else {
                "Various Artists".to_string()
            };
            let year = tracks.iter().map(|t| t.year).max().unwrap_or(0);
            let title = tracks[0].album.clone();
            let track_titles = tracks
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            albums.push(Album {
                id,
                title,
                artist,
                year,
                tracks: tracks.into(),
                track_titles,
            });
        }
        albums.sort_by(|a, b| {
            a.artist
                .to_lowercase()
                .cmp(&b.artist.to_lowercase())
                .then_with(|| a.year.cmp(&b.year))
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        });

        covers.replace_all(cover_map);

        // Song order follows album order, like the reference player.
        let all_tracks: Vec<Track> = albums
            .iter()
            .flat_map(|a| a.tracks.iter().cloned())
            .collect();
        update(state, |s| {
            s.albums = albums;
            s.all_tracks = all_tracks.into();
            s.scanning = false;
            Self::apply_search(s);
        });

        // Save cache for fast startup next time
        let snapshot = state.read().clone();
        Self::save_cache(&snapshot);
        drop(snapshot);
    }
}
