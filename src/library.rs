//! Library state: SQLite-backed list paging, BM25 search and background
//! reconciliation with the music folders.

use crate::artwork::ArtworkCache;
use crate::config::AppConfig;
use crate::db::{self, Db};
use crate::index;
use crate::lookup::http::UreqHttp;
use crate::lookup::worker::{Wake, Worker, WorkerDeps};
use crate::models::{Album, AlbumSort, AlbumSummary, ArtistEntry, GenreEntry, SongSort, Track};
use crate::search;
use anyhow::{Context, Result};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::{Mutex, RwLock};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::time::Duration;

/// Search results for the current query. Ids are small, so they are kept in
/// memory; the rows themselves are paged from SQLite on demand.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub track_ids: Arc<[i64]>,
    pub album_keys: Arc<[String]>,
    pub artists: Arc<[ArtistEntry]>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SongSource {
    Search,
    Library(SongSort),
    Genre(String, SongSort),
    Paths(Arc<[PathBuf]>),
}

#[derive(Debug, Clone, Default)]
pub struct LibrarySnapshot {
    pub folders: Vec<PathBuf>,
    pub album_count: usize,
    pub track_count: usize,
    pub filter: Option<Filter>,
    pub scanning: bool,
    pub query: String,
    pub version: u64,
    pub lookup_pending: usize,
    pub lookup_total: usize,
    pub lookup_unmatched: Vec<AlbumSummary>,
}

pub(crate) type SharedSnapshot = Arc<RwLock<Arc<LibrarySnapshot>>>;

pub struct Library {
    state: SharedSnapshot,
    db: Arc<Db>,
    covers: ArtworkCache,
    watcher: Mutex<Option<RecommendedWatcher>>,
    rescan_tx: Sender<()>,
    lookup_tx: Option<Sender<Wake>>,
}

pub(crate) fn update(state: &SharedSnapshot, change: impl FnOnce(&mut LibrarySnapshot)) {
    let mut guard = state.write();
    let snapshot = Arc::make_mut(&mut guard);
    change(snapshot);
    snapshot.version += 1;
}

impl Library {
    pub fn new() -> Result<Self> {
        let config = AppConfig::load();
        let folders = config.folders;
        let db_path = Self::db_path()?;
        let db = Arc::new(Db::open(db_path)?);

        let (rescan_tx, rx) = mpsc::channel::<()>();
        let watcher = Self::make_watcher(&folders, &rescan_tx)?;

        let covers = ArtworkCache::default();
        {
            let db = db.clone();
            covers.set_provider(Arc::new(move |key: &str| {
                db.read(|conn| db::artwork(conn, key)).ok().flatten()
            }));
        }

        let state: SharedSnapshot = Arc::new(RwLock::new(Arc::new(Self::load_snapshot(
            &db,
            folders.clone(),
        )?)));
        update(&state, |s| s.scanning = true);

        let cache_dir = Self::cache_dir()?;
        let lookup_tx = Worker::spawn(
            WorkerDeps {
                http: Arc::new(UreqHttp::new()),
                db_path: db.path.clone(),
                cache_dir: cache_dir.clone(),
                covers: covers.clone(),
                state: state.clone(),
                acoustid_key: config.acoustid_key.clone(),
            },
            config.online_lookup,
        );

        let thread_state = state.clone();
        let thread_db = db.clone();
        let thread_covers = covers.clone();
        let thread_lookup = lookup_tx.clone();
        std::thread::spawn(move || {
            let mut writer = match Db::open_writer(&thread_db.path) {
                Ok(writer) => writer,
                Err(err) => {
                    eprintln!("open library database: {err:#}");
                    return;
                }
            };
            loop {
                let folders = thread_state.read().folders.clone();
                update(&thread_state, |s| s.scanning = true);
                match index::reconcile(&mut writer, &folders, &cache_dir) {
                    Ok(stats) => {
                        for key in &stats.changed_albums {
                            thread_covers.remove(key);
                        }
                        if stats.removed > 0 {
                            thread_covers.clear();
                        }
                        Self::reload(&thread_state, &thread_db);
                    }
                    Err(err) => eprintln!("scan library: {err:#}"),
                }
                update(&thread_state, |s| s.scanning = false);
                let _ = thread_lookup.send(Wake::ScanFinished);

                if rx.recv().is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(800));
                while rx.try_recv().is_ok() {}
            }
        });

        Ok(Self {
            state,
            db,
            covers,
            watcher: Mutex::new(watcher),
            rescan_tx,
            lookup_tx: Some(lookup_tx),
        })
    }

    pub fn set_online_lookup(&self, on: bool) {
        AppConfig::update(|c| c.online_lookup = on);
        if let Some(tx) = &self.lookup_tx {
            let _ = tx.send(Wake::SettingChanged(on));
        }
    }

    pub fn retry_unmatched(&self) {
        if let Err(err) = self.db.read(db::clear_unmatched) {
            eprintln!("online lookup: clear unmatched: {err:#}");
        }
        update(&self.state, |s| s.lookup_unmatched.clear());
        if let Some(tx) = &self.lookup_tx {
            let _ = tx.send(Wake::Retry);
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn insert_album(&self, summary: &AlbumSummary) {
        let path = format!("/music/{}/01.mp3", summary.title);
        let dir = format!("/music/{}", summary.title);
        self.db
            .read(|conn| {
                conn.execute(
                    "INSERT INTO tracks(path, dir, title, artist, album, album_key, track_number, \
                     year, duration, mtime_ns, size) VALUES (?1, ?2, 'Song', ?3, ?4, ?5, 1, ?6, \
                     200.0, 1, 1)",
                    rusqlite::params![
                        path,
                        dir,
                        summary.artist,
                        summary.title,
                        summary.id,
                        summary.year
                    ],
                )?;
                conn.execute_batch(index::REBUILD_ALBUMS)?;
                if summary.unmatched {
                    db::record_lookup(
                        conn,
                        &db::LookupRow {
                            album_key: summary.id.clone(),
                            status: db::LookupStatus::Unmatched,
                            release: None,
                            release_group: None,
                            score: Some(0.5),
                            candidate: None,
                            attempted_at: 1,
                        },
                    )?;
                }
                Ok(())
            })
            .expect("insert album");
        Self::reload(&self.state, &self.db);
        update(&self.state, |s| {
            if s.folders.is_empty() {
                s.folders.push(PathBuf::from("/music"));
            }
            if summary.unmatched {
                s.lookup_unmatched.push(summary.clone());
            }
        });
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn insert_cover(&self, album_id: &str, png: &[u8]) {
        self.db
            .read(|conn| db::set_artwork(conn, album_id, png))
            .expect("insert cover");
        self.covers.remove(album_id);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn insert_song(&self, track: &Track, added_ns: i64) {
        let dir = track
            .path
            .parent()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.db
            .read(|conn| {
                conn.execute(
                    "INSERT INTO tracks(path, dir, title, artist, album, album_key, genre, \
                     track_number, year, duration, mtime_ns, size) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1)",
                    rusqlite::params![
                        track.path.to_string_lossy(),
                        dir,
                        track.title,
                        track.artist,
                        track.album,
                        track.album_id,
                        track.genre,
                        track.track_number,
                        track.year,
                        track.duration,
                        added_ns
                    ],
                )?;
                conn.execute_batch(index::REBUILD_ALBUMS)?;
                Ok(())
            })
            .expect("insert song");
        Self::reload(&self.state, &self.db);
        update(&self.state, |s| {
            if s.folders.is_empty() {
                s.folders.push(PathBuf::from("/music"));
            }
        });
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_lookup(dir: &Path, http: Arc<dyn crate::lookup::http::Http>) -> Result<Self> {
        let db = Arc::new(Db::open(dir.join("library.db"))?);
        let covers = ArtworkCache::default();
        {
            let db = db.clone();
            covers.set_provider(Arc::new(move |key: &str| {
                db.read(|conn| db::artwork(conn, key)).ok().flatten()
            }));
        }
        let state: SharedSnapshot =
            Arc::new(RwLock::new(Arc::new(Self::load_snapshot(&db, Vec::new())?)));
        let lookup_tx = Worker::spawn(
            WorkerDeps {
                http,
                db_path: db.path.clone(),
                cache_dir: dir.join("cache"),
                covers: covers.clone(),
                state: state.clone(),
                acoustid_key: AppConfig::load().acoustid_key,
            },
            AppConfig::load().online_lookup,
        );
        let (rescan_tx, _rx) = mpsc::channel::<()>();
        Ok(Self {
            state,
            db,
            covers,
            watcher: Mutex::new(None),
            rescan_tx,
            lookup_tx: Some(lookup_tx),
        })
    }

    fn cache_dir() -> Result<PathBuf> {
        let dir = dirs::cache_dir().context("no cache directory")?;
        Ok(dir.join("omamusic"))
    }

    fn db_path() -> Result<PathBuf> {
        Ok(Self::cache_dir()?.join("library.db"))
    }

    fn load_snapshot(db: &Db, folders: Vec<PathBuf>) -> Result<LibrarySnapshot> {
        let (track_count, album_count) =
            db.read(|conn| Ok((db::track_count(conn)?, db::album_count(conn)?)))?;
        Ok(LibrarySnapshot {
            folders,
            album_count,
            track_count,
            ..Default::default()
        })
    }

    fn reload(state: &SharedSnapshot, db: &Db) {
        let counts = db.read(|conn| Ok((db::track_count(conn)?, db::album_count(conn)?)));
        let Ok((track_count, album_count)) = counts else {
            return;
        };
        // Recompute until the query we searched for is still the current one,
        // so a query typed while the scan was running is not left stale.
        loop {
            let query = state.read().query.clone();
            let filter = Self::compute_filter(db, &query);
            let mut installed = false;
            update(state, |s| {
                s.track_count = track_count;
                s.album_count = album_count;
                if s.query == query {
                    s.filter = filter;
                    installed = true;
                }
            });
            if installed {
                break;
            }
        }
    }

    fn compute_filter(db: &Db, query: &str) -> Option<Filter> {
        if query.trim().is_empty() {
            return None;
        }
        db.read(|conn| {
            let Some(found) = search::search(conn, query)? else {
                return Ok(None);
            };
            let artists = search::artists_from_albums(conn, &found.album_keys)?;
            Ok(Some(Filter {
                track_ids: found.track_ids.into(),
                album_keys: found.album_keys.into(),
                artists: artists.into(),
            }))
        })
        .ok()
        .flatten()
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

    #[cfg(any(test, feature = "test-support"))]
    /// A library with no folders, watcher, or scanner thread; for view tests.
    pub fn empty() -> Self {
        let (rescan_tx, _rx) = mpsc::channel::<()>();
        let db = Arc::new(Db::in_memory().expect("in-memory library database"));
        let covers = ArtworkCache::default();
        {
            let db = db.clone();
            covers.set_provider(Arc::new(move |key: &str| {
                db.read(|conn| db::artwork(conn, key)).ok().flatten()
            }));
        }
        Self {
            state: Arc::new(RwLock::new(Arc::new(LibrarySnapshot::default()))),
            db,
            covers,
            watcher: Mutex::new(None),
            rescan_tx,
            lookup_tx: None,
        }
    }

    pub fn set_query(&self, q: String) {
        if self.state.read().query == q {
            return;
        }
        let filter = Self::compute_filter(&self.db, &q);
        update(&self.state, |s| {
            s.query = q;
            s.filter = filter;
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

    // -- Paged views ---------------------------------------------------------

    pub fn track_total(&self) -> usize {
        let s = self.snapshot();
        s.filter
            .as_ref()
            .map_or(s.track_count, |f| f.track_ids.len())
    }

    pub fn track_page(&self, offset: usize, limit: usize) -> Vec<Track> {
        if limit == 0 {
            return Vec::new();
        }
        let s = self.snapshot();
        let result = match &s.filter {
            Some(f) => {
                let end = (offset + limit).min(f.track_ids.len());
                if offset >= end {
                    return Vec::new();
                }
                self.db
                    .read(|conn| db::tracks_by_ids(conn, &f.track_ids[offset..end]))
            }
            None => self.db.read(|conn| db::tracks_page(conn, offset, limit)),
        };
        result.unwrap_or_default()
    }

    /// A single track of the current song view, for menu actions.
    pub fn track_at(&self, index: usize) -> Option<Track> {
        self.track_page(index, 1).into_iter().next()
    }

    /// Every track of the current song view, used to seed playback.
    pub fn tracks_for_play(&self) -> Vec<Track> {
        let s = self.snapshot();
        let result = match &s.filter {
            Some(f) => self.db.read(|conn| db::tracks_by_ids(conn, &f.track_ids)),
            None => self.db.read(db::all_tracks),
        };
        result.unwrap_or_default()
    }

    pub fn song_total(&self, source: &SongSource) -> usize {
        match source {
            SongSource::Search => self.track_total(),
            SongSource::Library(_) => self.snapshot().track_count,
            SongSource::Genre(..) => self.song_stats(source).0,
            SongSource::Paths(_) => self.songs(source).len(),
        }
    }

    pub fn song_stats(&self, source: &SongSource) -> (usize, f64) {
        let genre = match source {
            SongSource::Library(_) => None,
            SongSource::Genre(name, _) => Some(name.as_str()),
            SongSource::Search | SongSource::Paths(_) => {
                let songs = self.songs(source);
                return (songs.len(), songs.iter().map(|t| t.duration).sum());
            }
        };
        self.db
            .read(|conn| db::songs_stats(conn, genre))
            .unwrap_or_default()
    }

    pub fn song_page(&self, source: &SongSource, offset: usize, limit: usize) -> Vec<Track> {
        if limit == 0 {
            return Vec::new();
        }
        match source {
            SongSource::Search => self.track_page(offset, limit),
            SongSource::Library(sort) => self
                .db
                .read(|conn| db::songs_page(conn, *sort, None, offset, limit))
                .unwrap_or_default(),
            SongSource::Genre(name, sort) => self
                .db
                .read(|conn| db::songs_page(conn, *sort, Some(name), offset, limit))
                .unwrap_or_default(),
            SongSource::Paths(_) => self
                .songs(source)
                .into_iter()
                .skip(offset)
                .take(limit)
                .collect(),
        }
    }

    pub fn songs(&self, source: &SongSource) -> Vec<Track> {
        match source {
            SongSource::Search => self.tracks_for_play(),
            SongSource::Paths(paths) => self
                .db
                .read(|conn| db::tracks_by_paths(conn, paths))
                .unwrap_or_default(),
            SongSource::Library(_) | SongSource::Genre(..) => {
                self.song_page(source, 0, i64::MAX as usize)
            }
        }
    }

    pub fn genres(&self) -> Vec<GenreEntry> {
        self.db.read(db::genres).unwrap_or_default()
    }

    pub fn album_total(&self) -> usize {
        let s = self.snapshot();
        s.filter
            .as_ref()
            .map_or(s.album_count, |f| f.album_keys.len())
    }

    pub fn album_page(&self, offset: usize, limit: usize, sort: AlbumSort) -> Vec<AlbumSummary> {
        if limit == 0 {
            return Vec::new();
        }
        let s = self.snapshot();
        let result = match &s.filter {
            Some(f) => {
                let end = (offset + limit).min(f.album_keys.len());
                if offset >= end {
                    return Vec::new();
                }
                self.db
                    .read(|conn| db::albums_by_keys(conn, &f.album_keys[offset..end]))
            }
            None => self
                .db
                .read(|conn| db::albums_page(conn, offset, limit, sort)),
        };
        result.unwrap_or_default()
    }

    pub fn artist_total(&self) -> usize {
        let s = self.snapshot();
        match &s.filter {
            Some(f) => f.artists.len(),
            None => self.db.read(db::artist_count).unwrap_or(0),
        }
    }

    pub fn artist_page(&self, offset: usize, limit: usize) -> Vec<ArtistEntry> {
        if limit == 0 {
            return Vec::new();
        }
        let s = self.snapshot();
        match &s.filter {
            Some(f) => {
                let end = (offset + limit).min(f.artists.len());
                if offset >= end {
                    return Vec::new();
                }
                f.artists[offset..end].to_vec()
            }
            None => self
                .db
                .read(|conn| db::artists_page(conn, offset, limit))
                .unwrap_or_default(),
        }
    }

    pub fn album_detail(&self, key: &str) -> Option<Album> {
        self.db
            .read(|conn| db::album_detail(conn, key))
            .ok()
            .flatten()
    }

    pub fn album_tracks(&self, key: &str) -> Vec<Track> {
        self.db
            .read(|conn| db::album_tracks(conn, key))
            .unwrap_or_default()
    }

    pub fn artist_albums(&self, name: &str) -> Vec<AlbumSummary> {
        self.db
            .read(|conn| db::artist_albums(conn, name))
            .unwrap_or_default()
    }

    pub fn artist_tracks(&self, name: &str) -> Vec<Track> {
        self.db
            .read(|conn| db::artist_tracks(conn, name))
            .unwrap_or_default()
    }

    pub fn first_album_key(&self) -> Option<String> {
        self.db.read(db::first_album_key).ok().flatten()
    }
}

#[cfg(test)]
impl Library {
    /// Build a library from an existing database without starting the
    /// background reconciler, for tests.
    fn open_for_test(db_path: PathBuf, folders: Vec<PathBuf>) -> Result<Self> {
        let db = Arc::new(Db::open(db_path)?);
        let covers = ArtworkCache::default();
        let (rescan_tx, _rx) = mpsc::channel();
        let state: SharedSnapshot =
            Arc::new(RwLock::new(Arc::new(Self::load_snapshot(&db, folders)?)));
        Ok(Self {
            state,
            db,
            covers,
            watcher: Mutex::new(None),
            rescan_tx,
            lookup_tx: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn insert(
        conn: &rusqlite::Connection,
        path: &str,
        title: &str,
        artist: &str,
        album: &str,
        n: u32,
    ) {
        let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
        let key = index::album_key(dir, album);
        conn.execute(
            "INSERT INTO tracks(path, dir, title, artist, album, album_key, track_number, year, \
             duration, mtime_ns, size) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 0.0, 1, 1)",
            params![path, dir, title, artist, album, key, n],
        )
        .unwrap();
    }

    #[test]
    fn paging_and_bm25_read_from_the_database() {
        let dir = std::env::temp_dir().join(format!("omamusic-lib-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("library.db");
        {
            let conn = Db::open_writer(&db_path).unwrap();
            insert(
                &conn,
                "/m/1.mp3",
                "Neon Lights",
                "Kraftwerk",
                "Trans Europe",
                1,
            );
            insert(
                &conn,
                "/m/2.mp3",
                "Autobahn",
                "Kraftwerk",
                "Trans Europe",
                2,
            );
            insert(&conn, "/m/3.mp3", "Moon River", "Andy", "Some Album", 1);
            conn.execute_batch(index::REBUILD_ALBUMS).unwrap();
        }
        let library = Library::open_for_test(db_path, Vec::new()).unwrap();

        assert_eq!(library.track_total(), 3);
        assert_eq!(library.album_total(), 2);
        assert_eq!(library.artist_total(), 2);

        // Album order is by artist, so "Andy" comes first.
        assert_eq!(library.track_at(0).unwrap().title, "Moon River");
        assert_eq!(library.track_page(0, 2).len(), 2);
        assert_eq!(library.track_page(2, 2).len(), 1);
        assert_eq!(library.album_page(0, 10, AlbumSort::Artist).len(), 2);
        assert_eq!(library.artist_page(0, 10)[0].name, "Andy");

        // BM25 filtering narrows all three views.
        library.set_query("neon".into());
        assert_eq!(library.track_total(), 1);
        assert_eq!(library.track_page(0, 10)[0].title, "Neon Lights");
        assert_eq!(library.album_total(), 1);
        assert_eq!(library.artist_total(), 1);
        assert_eq!(library.artist_page(0, 10)[0].name, "Kraftwerk");

        // Album and artist detail pages read their tracks from SQLite.
        let key = library.album_page(0, 1, AlbumSort::Artist)[0].id.clone();
        let album = library.album_detail(&key).unwrap();
        assert_eq!(album.tracks.len(), 2);
        assert_eq!(library.album_tracks(&key).len(), 2);
        assert_eq!(library.artist_albums("Kraftwerk").len(), 1);
        assert_eq!(library.artist_tracks("Kraftwerk").len(), 2);

        // Clearing the query restores the unfiltered paging.
        library.set_query(String::new());
        assert_eq!(library.track_total(), 3);
        assert_eq!(library.album_total(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
