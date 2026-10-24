//! Background reconciler: walks the music folders, reads tags only for files
//! that are new or whose size/mtime changed, and writes the result to SQLite.

use crate::metadata;
use anyhow::Result;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Stable album identity, also the artwork-cache key. Kept byte-for-byte
/// compatible with the previous JSON cache so cached covers stay valid.
pub fn album_key(dir: &str, album: &str) -> String {
    format!("{dir}\n{album}")
}

/// Roll the `albums` table up from `tracks`. Album artists collapse to
/// "Various Artists" when the tracks disagree, and all track titles are
/// concatenated so album search can match on song names.
pub(crate) const REBUILD_ALBUMS: &str = r#"
DELETE FROM albums;
INSERT INTO albums(key, title, artist, year, dir, track_count, track_titles, genres)
SELECT album_key,
       MAX(album),
       CASE WHEN COUNT(DISTINCT artist) = 1 THEN MAX(artist) ELSE 'Various Artists' END,
       MAX(year),
       MAX(dir),
       COUNT(*),
       GROUP_CONCAT(title, ' '),
       COALESCE(GROUP_CONCAT(DISTINCT NULLIF(genre, '')), '')
FROM tracks
GROUP BY album_key;
"#;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanStats {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub skipped: usize,
    /// Albums whose album row or artwork changed, for cover-cache invalidation.
    pub changed_albums: Vec<String>,
}

impl ScanStats {
    pub fn changed(&self) -> bool {
        self.added > 0 || self.updated > 0 || self.removed > 0
    }
}

fn mtime_ns(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i64)
}

fn load_existing(conn: &Connection) -> Result<HashMap<String, (i64, i64)>> {
    let mut stmt = conn.prepare("SELECT path, mtime_ns, size FROM tracks")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            (row.get::<_, i64>(1)?, row.get::<_, i64>(2)?),
        ))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (path, stamp) = row?;
        map.insert(path, stamp);
    }
    Ok(map)
}

fn under(folder: &Path, path: &str) -> bool {
    let prefix = folder.to_string_lossy();
    path.strip_prefix(prefix.as_ref())
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(std::path::MAIN_SEPARATOR))
}

/// Reconcile the database with what is on disk. Returns what changed.
pub fn reconcile(conn: &mut Connection, folders: &[PathBuf]) -> Result<ScanStats> {
    let existing = load_existing(conn)?;
    let mut stats = ScanStats::default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut touched: HashSet<String> = HashSet::new();
    let mut embedded: HashMap<String, Vec<u8>> = HashMap::new();
    let mut album_path: HashMap<String, PathBuf> = HashMap::new();

    let tx = conn.transaction()?;
    for folder in folders {
        if !folder.is_dir() {
            continue;
        }
        for entry in WalkDir::new(folder)
            .into_iter()
            .filter_entry(|entry| !metadata::is_macos_junk(entry.path()))
            .filter_map(|e| e.ok())
        {
            let is_file = entry.file_type().is_file();
            let path = entry.into_path();
            if !is_file && !path.is_file() {
                continue;
            }
            if !metadata::is_audio(&path) {
                continue;
            }
            let path_str = path.to_string_lossy().into_owned();
            seen.insert(path_str.clone());
            let Ok(stat) = std::fs::metadata(&path) else {
                continue;
            };
            let (mtime, size) = (mtime_ns(&stat), stat.len() as i64);
            if let Some(&(old_mtime, old_size)) = existing.get(&path_str) {
                if old_mtime == mtime && old_size == size {
                    stats.skipped += 1;
                    continue;
                }
            }

            let md = metadata::read(&path);
            let dir = path
                .parent()
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_default();
            let album_name = if md.album.is_empty() {
                path.parent()
                    .and_then(|d| d.file_name())
                    .and_then(|s| s.to_str())
                    .unwrap_or("Unknown Album")
                    .to_string()
            } else {
                md.album
            };
            let key = album_key(&dir, &album_name);
            let title = if md.title.is_empty() {
                path.file_stem()
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
            tx.execute(
                "INSERT INTO tracks(path, dir, title, artist, album, album_key, genre, track_number, \
                 year, duration, mtime_ns, size) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
                 ON CONFLICT(path) DO UPDATE SET dir = excluded.dir, title = excluded.title, \
                 artist = excluded.artist, album = excluded.album, album_key = excluded.album_key, \
                 genre = excluded.genre, track_number = excluded.track_number, \
                 year = excluded.year, duration = excluded.duration, \
                 mtime_ns = excluded.mtime_ns, size = excluded.size",
                params![
                    path_str,
                    dir,
                    title,
                    artist,
                    album_name,
                    key,
                    md.genre,
                    md.track_number as i64,
                    md.year as i64,
                    md.duration.max(0.0),
                    mtime,
                    size,
                ],
            )?;
            if existing.contains_key(&path_str) {
                stats.updated += 1;
            } else {
                stats.added += 1;
            }
            touched.insert(key.clone());
            album_path
                .entry(key.clone())
                .or_insert_with(|| path.clone());
            if let Some(art) = md.artwork {
                if !art.is_empty() {
                    embedded.entry(key).or_insert(art);
                }
            }
        }
    }

    for path in existing.keys() {
        if seen.contains(path) {
            continue;
        }
        // Keep rows for a configured folder that is temporarily unavailable
        // (for example an unmounted drive) instead of dropping the whole index.
        let folder = folders.iter().find(|folder| under(folder, path));
        if folder.is_some_and(|folder| !folder.is_dir()) {
            continue;
        }
        tx.execute("DELETE FROM tracks WHERE path = ?1", params![path])?;
        stats.removed += 1;
    }

    if stats.changed() {
        tx.execute_batch(REBUILD_ALBUMS)?;
    }
    // Artwork is refreshed even when no audio file changed, so dropping a
    // `cover.jpg` next to the files is picked up by the watcher.
    update_artwork(&tx, &touched, &embedded, &album_path, &mut stats)?;
    tx.commit()?;
    Ok(stats)
}

fn update_artwork(
    conn: &Connection,
    touched: &HashSet<String>,
    embedded: &HashMap<String, Vec<u8>>,
    album_path: &HashMap<String, PathBuf>,
    stats: &mut ScanStats,
) -> Result<()> {
    let mut changed: Vec<String> = touched.iter().cloned().collect();
    for key in touched {
        let path = album_path.get(key);
        let data = embedded
            .get(key)
            .cloned()
            .or_else(|| path.and_then(|p| metadata::folder_artwork(p)));
        upsert_artwork(conn, key, data.as_deref(), path)?;
    }

    // Albums that were never checked, or that have no cover yet, get a folder
    // art check so dropping a `cover.jpg` next to the files is picked up.
    let mut stmt = conn.prepare(
        "SELECT a.key, (SELECT path FROM tracks WHERE album_key = a.key LIMIT 1) FROM albums a \
         LEFT JOIN artwork ar ON ar.album_key = a.key WHERE ar.album_key IS NULL OR ar.data IS NULL",
    )?;
    let rows: Vec<(String, Option<String>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    for (key, path) in rows {
        if touched.contains(&key) {
            continue;
        }
        let Some(path) = path.map(PathBuf::from) else {
            continue;
        };
        let data = metadata::folder_artwork(&path);
        if data.is_some() {
            changed.push(key.clone());
        }
        upsert_artwork(conn, &key, data.as_deref(), Some(&path))?;
    }

    conn.execute(
        "DELETE FROM artwork WHERE album_key NOT IN (SELECT key FROM albums)",
        [],
    )?;

    changed.sort();
    changed.dedup();
    stats.changed_albums = changed;
    Ok(())
}

fn upsert_artwork(
    conn: &Connection,
    key: &str,
    data: Option<&[u8]>,
    path: Option<&PathBuf>,
) -> Result<()> {
    let src = path.map(|p| p.to_string_lossy().into_owned());
    conn.execute(
        "INSERT INTO artwork(album_key, data, src_path) VALUES (?1, ?2, ?3) \
         ON CONFLICT(album_key) DO UPDATE SET data = excluded.data, src_path = excluded.src_path \
         WHERE artwork.data IS NOT excluded.data",
        params![key, data, src],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("omamusic-index-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn dir(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
    }

    fn memory() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        db::ensure_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn first_scan_indexes_and_later_scans_skip_unchanged_files() {
        let temp = TempDir::new();
        write(&temp.dir().join("one.mp3"), b"a");
        write(&temp.dir().join("two.mp3"), b"bb");
        let mut conn = memory();

        let first = reconcile(&mut conn, &[temp.dir().to_path_buf()]).unwrap();
        assert_eq!(first.added, 2);
        assert_eq!(first.updated, 0);
        assert_eq!(first.removed, 0);
        assert_eq!(db::track_count(&conn).unwrap(), 2);

        let second = reconcile(&mut conn, &[temp.dir().to_path_buf()]).unwrap();
        assert_eq!(second.added, 0);
        assert_eq!(second.skipped, 2);
        assert!(!second.changed());
    }

    #[test]
    fn changed_file_is_reindexed_and_deleted_file_is_removed() {
        let temp = TempDir::new();
        let file = temp.dir().join("one.mp3");
        write(&file, b"a");
        let mut conn = memory();
        reconcile(&mut conn, &[temp.dir().to_path_buf()]).unwrap();

        write(&file, b"changed-length");
        let updated = reconcile(&mut conn, &[temp.dir().to_path_buf()]).unwrap();
        assert_eq!(updated.updated, 1);

        std::fs::remove_file(&file).unwrap();
        let removed = reconcile(&mut conn, &[temp.dir().to_path_buf()]).unwrap();
        assert_eq!(removed.removed, 1);
        assert_eq!(db::track_count(&conn).unwrap(), 0);
    }

    #[test]
    fn embedded_title_becomes_searchable_and_falls_back_to_file_name() {
        let temp = TempDir::new();
        write(&temp.dir().join("mystery song.mp3"), b"a");
        let mut conn = memory();
        reconcile(&mut conn, &[temp.dir().to_path_buf()]).unwrap();
        let tracks = db::all_tracks(&conn).unwrap();
        assert_eq!(tracks.len(), 1);
        // No ID3 header: the file stem is used as the title.
        assert_eq!(tracks[0].title, "mystery song");
        assert_eq!(
            db::search_tracks(&conn, "\"mystery\"", 10).unwrap().len(),
            1
        );
        assert_eq!(
            db::search_tracks(&conn, "\"mystery\" AND \"song\"", 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn removing_a_folder_drops_its_tracks() {
        let temp = TempDir::new();
        let other = TempDir::new();
        write(&other.dir().join("keep.mp3"), b"a");
        write(&temp.dir().join("drop.mp3"), b"a");
        let mut conn = memory();
        reconcile(
            &mut conn,
            &[other.dir().to_path_buf(), temp.dir().to_path_buf()],
        )
        .unwrap();
        assert_eq!(db::track_count(&conn).unwrap(), 2);
        // Scanning only the remaining folder drops the tracks of the removed one.
        reconcile(&mut conn, &[other.dir().to_path_buf()]).unwrap();
        assert_eq!(db::track_count(&conn).unwrap(), 1);
    }

    #[test]
    fn offline_folder_keeps_its_tracks() {
        let temp = TempDir::new();
        write(&temp.dir().join("one.mp3"), b"a");
        let mut conn = memory();
        reconcile(&mut conn, &[temp.dir().to_path_buf()]).unwrap();
        assert_eq!(db::track_count(&conn).unwrap(), 1);
        // Simulate an unmounted drive: the configured folder is gone but the
        // index must not be wiped.
        std::fs::remove_dir_all(temp.dir()).unwrap();
        let stats = reconcile(&mut conn, &[temp.dir().to_path_buf()]).unwrap();
        assert_eq!(stats.removed, 0);
        assert_eq!(db::track_count(&conn).unwrap(), 1);
    }

    #[test]
    fn scanner_ignores_non_audio_files() {
        let temp = TempDir::new();
        write(&temp.dir().join("cover.jpg"), b"image");
        write(&temp.dir().join("notes.txt"), b"text");
        write(&temp.dir().join("song.flac"), b"flac");
        let mut conn = memory();
        let stats = reconcile(&mut conn, &[temp.dir().to_path_buf()]).unwrap();
        assert_eq!(stats.added, 1);
        assert_eq!(db::track_count(&conn).unwrap(), 1);
    }

    #[test]
    fn adding_folder_cover_is_picked_up_without_audio_changes() {
        let temp = TempDir::new();
        write(&temp.dir().join("song.mp3"), b"a");
        let mut conn = memory();
        reconcile(&mut conn, &[temp.dir().to_path_buf()]).unwrap();

        let dir = temp.dir().to_string_lossy().into_owned();
        let album = temp
            .dir()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let key = album_key(&dir, &album);
        assert_eq!(db::artwork(&conn, &key).unwrap(), None);

        // A cover dropped next to the files is not an audio change, but the
        // watcher still triggers a reconcile and the cover must appear.
        write(&temp.dir().join("cover.jpg"), b"image-bytes");
        let stats = reconcile(&mut conn, &[temp.dir().to_path_buf()]).unwrap();
        assert!(!stats.changed());
        assert!(stats.changed_albums.contains(&key));
        assert_eq!(
            db::artwork(&conn, &key).unwrap(),
            Some(b"image-bytes".to_vec())
        );
    }

    #[test]
    fn macos_archive_junk_is_not_indexed() {
        let temp = TempDir::new();
        std::fs::create_dir_all(temp.dir().join("Album")).unwrap();
        std::fs::create_dir_all(temp.dir().join("__MACOSX/Album")).unwrap();
        write(&temp.dir().join("Album/song.mp3"), b"a");
        write(&temp.dir().join("Album/._song.mp3"), b"appledouble");
        write(
            &temp.dir().join("__MACOSX/Album/._song.mp3"),
            b"appledouble",
        );
        write(&temp.dir().join("__MACOSX/Album/song.mp3"), b"appledouble");

        let mut conn = memory();
        let stats = reconcile(&mut conn, &[temp.dir().to_path_buf()]).unwrap();
        assert_eq!(stats.added, 1);
        let tracks = db::all_tracks(&conn).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "song");
    }
}
