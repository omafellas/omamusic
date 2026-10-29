//! SQLite storage for the music library.
//!
//! The database is a disposable index: it can always be rebuilt from the audio
//! files, so an unreadable or outdated file is simply dropped and recreated.
//! `tracks` is the source of truth; `albums` is a materialized roll-up used for
//! album-level search; the two FTS5 tables back BM25 search.

use crate::models::{Album, AlbumSort, AlbumSummary, ArtistEntry, Track};
use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Bump when the schema changes. Older databases are discarded and rebuilt.
pub const SCHEMA_VERSION: i64 = 4;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tracks (
    id           INTEGER PRIMARY KEY,
    path         TEXT NOT NULL UNIQUE,
    dir          TEXT NOT NULL,
    title        TEXT NOT NULL,
    artist       TEXT NOT NULL,
    album        TEXT NOT NULL,
    album_key    TEXT NOT NULL,
    genre        TEXT NOT NULL DEFAULT '',
    track_number INTEGER NOT NULL DEFAULT 0,
    year         INTEGER NOT NULL DEFAULT 0,
    duration     REAL NOT NULL DEFAULT 0,
    mtime_ns     INTEGER NOT NULL,
    size         INTEGER NOT NULL,
    tags_missing     INTEGER NOT NULL DEFAULT 0,
    mb_release_group TEXT NOT NULL DEFAULT '',
    mb_release       TEXT NOT NULL DEFAULT '',
    mb_recording     TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS tracks_dir ON tracks(dir);
CREATE INDEX IF NOT EXISTS tracks_album_key ON tracks(album_key);
CREATE INDEX IF NOT EXISTS tracks_artist ON tracks(artist);

CREATE TABLE IF NOT EXISTS albums (
    id           INTEGER PRIMARY KEY,
    key          TEXT NOT NULL UNIQUE,
    title        TEXT NOT NULL,
    artist       TEXT NOT NULL,
    year         INTEGER NOT NULL DEFAULT 0,
    dir          TEXT NOT NULL,
    track_count  INTEGER NOT NULL DEFAULT 0,
    track_titles TEXT NOT NULL DEFAULT '',
    genres       TEXT NOT NULL DEFAULT '',
    tags_missing     INTEGER NOT NULL DEFAULT 0,
    mb_release_group TEXT NOT NULL DEFAULT '',
    added_ns     INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS artwork (
    album_key TEXT PRIMARY KEY,
    data      BLOB,
    src_path  TEXT,
    src_mtime INTEGER
);

CREATE TABLE IF NOT EXISTS lookup (
    album_key     TEXT PRIMARY KEY,
    status        TEXT NOT NULL,
    release       TEXT,
    release_group TEXT,
    score         REAL,
    candidate     TEXT,
    attempted_at  INTEGER NOT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS tracks_fts USING fts5(
    title, artist, album, genre,
    content='tracks', content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'
);
CREATE VIRTUAL TABLE IF NOT EXISTS albums_fts USING fts5(
    title, artist, track_titles, genres,
    content='albums', content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER IF NOT EXISTS tracks_ai AFTER INSERT ON tracks BEGIN
    INSERT INTO tracks_fts(rowid, title, artist, album, genre)
    VALUES (new.id, new.title, new.artist, new.album, new.genre);
END;
CREATE TRIGGER IF NOT EXISTS tracks_ad AFTER DELETE ON tracks BEGIN
    INSERT INTO tracks_fts(tracks_fts, rowid, title, artist, album, genre)
    VALUES ('delete', old.id, old.title, old.artist, old.album, old.genre);
END;
CREATE TRIGGER IF NOT EXISTS tracks_au AFTER UPDATE ON tracks BEGIN
    INSERT INTO tracks_fts(tracks_fts, rowid, title, artist, album, genre)
    VALUES ('delete', old.id, old.title, old.artist, old.album, old.genre);
    INSERT INTO tracks_fts(rowid, title, artist, album, genre)
    VALUES (new.id, new.title, new.artist, new.album, new.genre);
END;

CREATE TRIGGER IF NOT EXISTS albums_ai AFTER INSERT ON albums BEGIN
    INSERT INTO albums_fts(rowid, title, artist, track_titles, genres)
    VALUES (new.id, new.title, new.artist, new.track_titles, new.genres);
END;
CREATE TRIGGER IF NOT EXISTS albums_ad AFTER DELETE ON albums BEGIN
    INSERT INTO albums_fts(albums_fts, rowid, title, artist, track_titles, genres)
    VALUES ('delete', old.id, old.title, old.artist, old.track_titles, old.genres);
END;
CREATE TRIGGER IF NOT EXISTS albums_au AFTER UPDATE ON albums BEGIN
    INSERT INTO albums_fts(albums_fts, rowid, title, artist, track_titles, genres)
    VALUES ('delete', old.id, old.title, old.artist, old.track_titles, old.genres);
    INSERT INTO albums_fts(rowid, title, artist, track_titles, genres)
    VALUES (new.id, new.title, new.artist, new.track_titles, new.genres);
END;
"#;

const DROP: &str = r#"
DROP TABLE IF EXISTS tracks_fts;
DROP TABLE IF EXISTS albums_fts;
DROP TABLE IF EXISTS tracks;
DROP TABLE IF EXISTS albums;
DROP TABLE IF EXISTS artwork;
DROP TABLE IF EXISTS lookup;
"#;

/// Columns of `tracks`, aliased as `t`, in the order [`track_from_row`] reads.
const TRACK_COLS: &str =
    "t.path, t.title, t.artist, t.album, t.album_key, t.track_number, t.year, t.duration";

/// Order of tracks inside an album. Numbered tracks sort by number first, the
/// rest by path, matching the ordering the player used before SQLite.
const TRACK_ORDER: &str = "(t.track_number = 0), t.track_number, t.path";

/// Album ordering used everywhere the album list is shown.
const ALBUM_ORDER: &str = "a.artist COLLATE NOCASE, a.year, a.title COLLATE NOCASE";

fn album_order(sort: AlbumSort) -> &'static str {
    match sort {
        AlbumSort::Artist => ALBUM_ORDER,
        AlbumSort::Title => "a.title COLLATE NOCASE, a.artist COLLATE NOCASE, a.year",
        AlbumSort::Year => {
            "(a.year = 0), a.year DESC, a.artist COLLATE NOCASE, a.title COLLATE NOCASE"
        }
        AlbumSort::RecentlyAdded => {
            "a.added_ns DESC, a.artist COLLATE NOCASE, a.title COLLATE NOCASE"
        }
    }
}

const ALBUM_COLS: &str = "a.key, a.title, a.artist, a.year, a.track_count, \
     EXISTS(SELECT 1 FROM lookup l WHERE l.album_key = a.key \
     AND l.status IN ('unmatched', 'error')) AS unmatched";

pub const ERROR_RETRY_SECS: i64 = 60 * 60;

/// A SQLite file shared by the UI (reader) and the indexer (writer).
pub struct Db {
    pub path: PathBuf,
    reader: Mutex<Connection>,
}

impl Db {
    /// Open the reader connection, creating and validating the schema.
    ///
    /// The index is disposable, so an unreadable file is deleted and rebuilt
    /// rather than preventing the player from starting.
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match Self::try_open(&path) {
            Ok(db) => Ok(db),
            Err(err) => {
                eprintln!("library database unusable, rebuilding: {err:#}");
                remove_db_files(&path);
                Self::try_open(&path)
            }
        }
    }

    fn try_open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        configure(&conn)?;
        ensure_schema(&conn)?;
        Ok(Self {
            path: path.to_path_buf(),
            reader: Mutex::new(conn),
        })
    }

    /// An empty, in-memory database for view tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        configure(&conn)?;
        ensure_schema(&conn)?;
        Ok(Self {
            path: PathBuf::new(),
            reader: Mutex::new(conn),
        })
    }

    /// Open the connection used by the background indexer.
    pub fn open_writer(path: &Path) -> Result<Connection> {
        let conn = Connection::open(path)?;
        configure(&conn)?;
        ensure_schema(&conn)?;
        Ok(conn)
    }

    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        f(&self.reader.lock())
    }
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn remove_db_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(sidecar(path, "-wal"));
    let _ = std::fs::remove_file(sidecar(path, "-shm"));
}

pub fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    // `journal_mode` returns the resulting mode, so it cannot go through
    // `pragma_update` (which rejects statements that yield rows).
    let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(())
}

pub fn ensure_schema(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 0 && version != SCHEMA_VERSION {
        conn.execute_batch(DROP)?;
    }
    conn.execute_batch(SCHEMA)?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

fn track_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Track> {
    Ok(Track {
        path: PathBuf::from(row.get::<_, String>("path")?),
        title: row.get("title")?,
        artist: row.get("artist")?,
        album: row.get("album")?,
        track_number: row.get::<_, i64>("track_number")?.max(0) as u32,
        year: row.get::<_, i64>("year")?.max(0) as u32,
        duration: row.get("duration")?,
        album_id: row.get("album_key")?,
    })
}

fn album_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AlbumSummary> {
    Ok(AlbumSummary {
        id: row.get("key")?,
        title: row.get("title")?,
        artist: row.get("artist")?,
        year: row.get::<_, i64>("year")?.max(0) as u32,
        track_count: row.get::<_, i64>("track_count")?.max(0) as usize,
        unmatched: row.get::<_, i64>("unmatched")? != 0,
    })
}

fn placeholders(count: usize) -> String {
    let mut s = String::with_capacity(count * 2);
    for i in 0..count {
        if i > 0 {
            s.push(',');
        }
        s.push('?');
    }
    s
}

pub fn track_count(conn: &Connection) -> Result<usize> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM tracks", [], |row| row.get(0))?;
    Ok(n.max(0) as usize)
}

pub fn album_count(conn: &Connection) -> Result<usize> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM albums", [], |row| row.get(0))?;
    Ok(n.max(0) as usize)
}

pub fn artist_count(conn: &Connection) -> Result<usize> {
    let n: i64 = conn.query_row("SELECT COUNT(DISTINCT artist) FROM tracks", [], |row| {
        row.get(0)
    })?;
    Ok(n.max(0) as usize)
}

/// A page of all tracks in album order.
pub fn tracks_page(conn: &Connection, offset: usize, limit: usize) -> Result<Vec<Track>> {
    let sql = format!(
        "SELECT {TRACK_COLS} FROM tracks t JOIN albums a ON a.key = t.album_key \
         ORDER BY {ALBUM_ORDER}, {TRACK_ORDER} LIMIT ?1 OFFSET ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![limit as i64, offset as i64], track_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn all_tracks(conn: &Connection) -> Result<Vec<Track>> {
    let sql = format!(
        "SELECT {TRACK_COLS} FROM tracks t JOIN albums a ON a.key = t.album_key \
         ORDER BY {ALBUM_ORDER}, {TRACK_ORDER}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], track_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Tracks for the given row ids, returned in the order of `ids`.
pub fn tracks_by_ids(conn: &Connection, ids: &[i64]) -> Result<Vec<Track>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT t.id, {TRACK_COLS} FROM tracks t WHERE t.id IN ({})",
        placeholders(ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let params = rusqlite::params_from_iter(ids.iter());
    let rows = stmt.query_map(params, |row| {
        let id: i64 = row.get(0)?;
        Ok((id, track_from_row(row)?))
    })?;
    let mut found: std::collections::HashMap<i64, Track> = std::collections::HashMap::new();
    for row in rows {
        let (id, track) = row?;
        found.insert(id, track);
    }
    Ok(ids.iter().filter_map(|id| found.remove(id)).collect())
}

pub fn albums_page(
    conn: &Connection,
    offset: usize,
    limit: usize,
    sort: AlbumSort,
) -> Result<Vec<AlbumSummary>> {
    let sql = format!(
        "SELECT {ALBUM_COLS} FROM albums a \
         ORDER BY {} LIMIT ?1 OFFSET ?2",
        album_order(sort)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![limit as i64, offset as i64], album_summary_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Album summaries for the given keys, returned in the order of `keys`.
pub fn albums_by_keys(conn: &Connection, keys: &[String]) -> Result<Vec<AlbumSummary>> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {ALBUM_COLS} FROM albums a \
         WHERE a.key IN ({})",
        placeholders(keys.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let params = rusqlite::params_from_iter(keys.iter());
    let rows = stmt.query_map(params, |row| {
        let summary = album_summary_from_row(row)?;
        Ok((summary.id.clone(), summary))
    })?;
    let mut found: std::collections::HashMap<String, AlbumSummary> =
        std::collections::HashMap::new();
    for row in rows {
        let (key, summary) = row?;
        found.insert(key, summary);
    }
    Ok(keys.iter().filter_map(|key| found.remove(key)).collect())
}

pub fn first_album_key(conn: &Connection) -> Result<Option<String>> {
    let sql = format!("SELECT a.key FROM albums a ORDER BY {ALBUM_ORDER} LIMIT 1");
    Ok(conn
        .query_row(&sql, [], |row| row.get::<_, String>(0))
        .optional()?)
}

pub fn artists_page(conn: &Connection, offset: usize, limit: usize) -> Result<Vec<ArtistEntry>> {
    let mut stmt = conn.prepare(
        "SELECT artist, COUNT(DISTINCT album_key) AS albums, COUNT(*) AS songs, MIN(album_key) \
         FROM tracks GROUP BY artist ORDER BY artist COLLATE NOCASE LIMIT ?1 OFFSET ?2",
    )?;
    let rows = stmt.query_map(params![limit as i64, offset as i64], |row| {
        Ok(ArtistEntry {
            name: row.get(0)?,
            album_count: row.get::<_, i64>(1)?.max(0) as usize,
            song_count: row.get::<_, i64>(2)?.max(0) as usize,
            cover_album_id: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Full album, with tracks in play order.
pub fn album_detail(conn: &Connection, key: &str) -> Result<Option<Album>> {
    let album = conn
        .query_row(
            &format!("SELECT {ALBUM_COLS}, a.genres FROM albums a WHERE a.key = ?1"),
            params![key],
            |row| {
                Ok(Album {
                    id: row.get("key")?,
                    title: row.get("title")?,
                    artist: row.get("artist")?,
                    year: row.get::<_, i64>("year")?.max(0) as u32,
                    tracks: Vec::new().into(),
                    unmatched: row.get::<_, i64>("unmatched")? != 0,
                    genre: row
                        .get::<_, String>("genres")?
                        .split(',')
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                })
            },
        )
        .optional()?;
    let Some(mut album) = album else {
        return Ok(None);
    };
    let sql =
        format!("SELECT {TRACK_COLS} FROM tracks t WHERE t.album_key = ?1 ORDER BY {TRACK_ORDER}");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![key], track_from_row)?;
    let tracks: Vec<Track> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    album.tracks = tracks.into();
    Ok(Some(album))
}

pub fn album_tracks(conn: &Connection, key: &str) -> Result<Vec<Track>> {
    let sql =
        format!("SELECT {TRACK_COLS} FROM tracks t WHERE t.album_key = ?1 ORDER BY {TRACK_ORDER}");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![key], track_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn artist_albums(conn: &Connection, name: &str) -> Result<Vec<AlbumSummary>> {
    let sql = format!(
        "SELECT {ALBUM_COLS} FROM albums a \
         WHERE EXISTS (SELECT 1 FROM tracks t WHERE t.album_key = a.key AND t.artist = ?1) \
         ORDER BY {ALBUM_ORDER}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![name], album_summary_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn artist_tracks(conn: &Connection, name: &str) -> Result<Vec<Track>> {
    let sql = format!(
        "SELECT {TRACK_COLS} FROM tracks t JOIN albums a ON a.key = t.album_key \
         WHERE t.artist = ?1 ORDER BY {ALBUM_ORDER}, {TRACK_ORDER}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![name], track_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Track row ids matching a BM25 query, best match first.
pub fn search_tracks(conn: &Connection, query: &str, limit: usize) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT t.id FROM tracks_fts JOIN tracks t ON t.id = tracks_fts.rowid \
         WHERE tracks_fts MATCH ?1 ORDER BY bm25(tracks_fts, 5.0, 3.0, 1.0, 2.0) LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![query, limit as i64], |row| row.get::<_, i64>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Album keys matching a BM25 query, best match first.
pub fn search_albums(conn: &Connection, query: &str, limit: usize) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT a.key FROM albums_fts JOIN albums a ON a.id = albums_fts.rowid \
         WHERE albums_fts MATCH ?1 ORDER BY bm25(albums_fts, 5.0, 3.0, 1.0, 2.0) LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![query, limit as i64], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Stored cover art for an album, or `None` when it has none (or was never
/// checked).
pub fn artwork(conn: &Connection, key: &str) -> Result<Option<Vec<u8>>> {
    let data: Option<Option<Vec<u8>>> = conn
        .query_row(
            "SELECT data FROM artwork WHERE album_key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?;
    Ok(data.flatten())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupStatus {
    Matched,
    Unmatched,
    Error,
}

impl LookupStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            LookupStatus::Matched => "matched",
            LookupStatus::Unmatched => "unmatched",
            LookupStatus::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "matched" => Some(LookupStatus::Matched),
            "unmatched" => Some(LookupStatus::Unmatched),
            "error" => Some(LookupStatus::Error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LookupRow {
    pub album_key: String,
    pub status: LookupStatus,
    pub release: Option<String>,
    pub release_group: Option<String>,
    pub score: Option<f64>,
    pub candidate: Option<String>,
    pub attempted_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LookupCandidate {
    pub album_key: String,
    pub title: String,
    pub artist: String,
    pub year: u32,
    pub tags_missing: u32,
    pub mb_release_group: String,
    pub needs_cover: bool,
    pub tracks: Vec<Track>,
    pub recordings: Vec<String>,
}

pub fn record_lookup(conn: &Connection, row: &LookupRow) -> Result<()> {
    conn.execute(
        "INSERT INTO lookup(album_key, status, release, release_group, score, candidate, attempted_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(album_key) DO UPDATE SET status = excluded.status, release = excluded.release, \
         release_group = excluded.release_group, score = excluded.score, \
         candidate = excluded.candidate, attempted_at = excluded.attempted_at",
        params![
            row.album_key,
            row.status.as_str(),
            row.release,
            row.release_group,
            row.score,
            row.candidate,
            row.attempted_at,
        ],
    )?;
    Ok(())
}

pub fn unmatched_albums(conn: &Connection) -> Result<Vec<AlbumSummary>> {
    let sql = format!(
        "SELECT {ALBUM_COLS} FROM albums a \
         JOIN lookup l ON l.album_key = a.key WHERE l.status IN ('unmatched', 'error') \
         ORDER BY {ALBUM_ORDER}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], album_summary_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn clear_unmatched(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM lookup WHERE status IN ('unmatched', 'error')",
        [],
    )?)
}

pub fn set_artwork(conn: &Connection, key: &str, data: &[u8]) -> Result<()> {
    conn.execute(
        "INSERT INTO artwork(album_key, data) VALUES (?1, ?2) \
         ON CONFLICT(album_key) DO UPDATE SET data = excluded.data",
        params![key, data],
    )?;
    Ok(())
}

pub fn albums_needing_lookup(
    conn: &Connection,
    now: i64,
    retry_after: i64,
) -> Result<Vec<LookupCandidate>> {
    let sql = format!(
        "SELECT a.key, a.title, a.artist, a.year, a.tags_missing, a.mb_release_group, \
         (ar.data IS NULL) AS needs_cover \
         FROM albums a \
         LEFT JOIN artwork ar ON ar.album_key = a.key \
         LEFT JOIN lookup l ON l.album_key = a.key \
         WHERE (ar.data IS NULL OR a.tags_missing != 0) \
         AND (l.album_key IS NULL OR l.attempted_at <= ?1 \
              OR (l.status = 'error' AND l.attempted_at <= ?2)) \
         ORDER BY l.attempted_at IS NOT NULL, l.attempted_at, {ALBUM_ORDER}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let albums: Vec<LookupCandidate> = stmt
        .query_map(params![now - retry_after, now - ERROR_RETRY_SECS], |row| {
            Ok(LookupCandidate {
                album_key: row.get(0)?,
                title: row.get(1)?,
                artist: row.get(2)?,
                year: row.get::<_, i64>(3)?.max(0) as u32,
                tags_missing: row.get::<_, i64>(4)?.max(0) as u32,
                mb_release_group: row.get(5)?,
                needs_cover: row.get::<_, i64>(6)? != 0,
                tracks: Vec::new(),
                recordings: Vec::new(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let sql = format!(
        "SELECT {TRACK_COLS}, t.mb_recording FROM tracks t WHERE t.album_key = ?1 ORDER BY {TRACK_ORDER}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut out = Vec::with_capacity(albums.len());
    for mut album in albums {
        let rows = stmt.query_map(params![album.album_key], |row| {
            Ok((track_from_row(row)?, row.get::<_, String>("mb_recording")?))
        })?;
        for row in rows {
            let (track, recording) = row?;
            album.tracks.push(track);
            album.recordings.push(recording);
        }
        out.push(album);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        conn
    }

    fn insert_track(
        conn: &Connection,
        path: &str,
        title: &str,
        artist: &str,
        album: &str,
        number: u32,
    ) {
        let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
        let key = crate::index::album_key(dir, album);
        conn.execute(
            "INSERT INTO tracks(path, dir, title, artist, album, album_key, track_number, year, \
             duration, mtime_ns, size) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 0.0, 1, 1)",
            params![path, dir, title, artist, album, key, number],
        )
        .unwrap();
    }

    fn rebuild_albums(conn: &Connection) {
        conn.execute_batch(crate::index::REBUILD_ALBUMS).unwrap();
    }

    fn lookup(key: &str, status: LookupStatus, attempted_at: i64) -> LookupRow {
        LookupRow {
            album_key: key.to_string(),
            status,
            release: None,
            release_group: None,
            score: Some(0.4),
            candidate: Some("Someone – Something (1999)".to_string()),
            attempted_at,
        }
    }

    #[test]
    fn lookup_rows_round_trip_and_mark_summaries() {
        let conn = memory();
        insert_track(&conn, "/m/a/1.mp3", "One", "Band", "Record", 1);
        rebuild_albums(&conn);
        let key = albums_page(&conn, 0, 1, AlbumSort::Artist).unwrap()[0]
            .id
            .clone();
        assert!(!albums_page(&conn, 0, 1, AlbumSort::Artist).unwrap()[0].unmatched);

        record_lookup(&conn, &lookup(&key, LookupStatus::Unmatched, 100)).unwrap();
        let unmatched = unmatched_albums(&conn).unwrap();
        assert_eq!(unmatched.len(), 1);
        assert!(unmatched[0].unmatched);
        assert!(albums_page(&conn, 0, 1, AlbumSort::Artist).unwrap()[0].unmatched);
        assert!(albums_by_keys(&conn, std::slice::from_ref(&key)).unwrap()[0].unmatched);
        assert!(artist_albums(&conn, "Band").unwrap()[0].unmatched);

        record_lookup(&conn, &lookup(&key, LookupStatus::Matched, 200)).unwrap();
        assert!(unmatched_albums(&conn).unwrap().is_empty());
        assert!(!albums_page(&conn, 0, 1, AlbumSort::Artist).unwrap()[0].unmatched);
        assert_eq!(clear_unmatched(&conn).unwrap(), 0);

        record_lookup(&conn, &lookup(&key, LookupStatus::Unmatched, 300)).unwrap();
        assert_eq!(clear_unmatched(&conn).unwrap(), 1);
        assert!(unmatched_albums(&conn).unwrap().is_empty());
    }

    #[test]
    fn error_rows_count_as_unmatched_and_retry_sooner() {
        let conn = memory();
        insert_track(&conn, "/m/a/1.mp3", "One", "Band", "Record", 1);
        rebuild_albums(&conn);
        let key = albums_page(&conn, 0, 1, AlbumSort::Artist).unwrap()[0]
            .id
            .clone();
        let now = 10_000_000;
        let month = 2_592_000;

        record_lookup(&conn, &lookup(&key, LookupStatus::Error, now - 10)).unwrap();
        assert!(albums_needing_lookup(&conn, now, month).unwrap().is_empty());
        assert_eq!(
            unmatched_albums(&conn).unwrap().len(),
            1,
            "errors are shown too"
        );
        assert!(albums_page(&conn, 0, 1, AlbumSort::Artist).unwrap()[0].unmatched);

        record_lookup(
            &conn,
            &lookup(&key, LookupStatus::Error, now - ERROR_RETRY_SECS - 1),
        )
        .unwrap();
        assert_eq!(
            albums_needing_lookup(&conn, now, month).unwrap().len(),
            1,
            "errors are retried after an hour, not a month"
        );
        assert_eq!(
            clear_unmatched(&conn).unwrap(),
            1,
            "retry all covers errors"
        );
        assert!(unmatched_albums(&conn).unwrap().is_empty());
    }

    #[test]
    fn set_artwork_stores_a_cover() {
        let conn = memory();
        insert_track(&conn, "/m/a/1.mp3", "One", "Band", "Record", 1);
        rebuild_albums(&conn);
        let key = albums_page(&conn, 0, 1, AlbumSort::Artist).unwrap()[0]
            .id
            .clone();
        assert_eq!(artwork(&conn, &key).unwrap(), None);
        set_artwork(&conn, &key, b"img").unwrap();
        assert_eq!(artwork(&conn, &key).unwrap(), Some(b"img".to_vec()));
        set_artwork(&conn, &key, b"new").unwrap();
        assert_eq!(artwork(&conn, &key).unwrap(), Some(b"new".to_vec()));
    }

    #[test]
    fn albums_needing_lookup_respects_retry_window() {
        let conn = memory();
        insert_track(&conn, "/m/a/1.mp3", "One", "Band", "Record", 1);
        insert_track(&conn, "/m/a/2.mp3", "Two", "Band", "Record", 2);
        conn.execute(
            "UPDATE tracks SET duration = 200.5, mb_recording = 'rec-2' WHERE path = '/m/a/2.mp3'",
            [],
        )
        .unwrap();
        rebuild_albums(&conn);
        let now = 10_000_000;
        let month = 2_592_000;

        let need = albums_needing_lookup(&conn, now, month).unwrap();
        assert_eq!(need.len(), 1, "no artwork row means a cover is needed");
        let album = &need[0];
        assert_eq!(album.title, "Record");
        assert_eq!(album.artist, "Band");
        assert!(album.needs_cover);
        assert_eq!(album.tags_missing, 0);
        assert_eq!(album.tracks.len(), 2);
        assert_eq!(album.tracks[1].duration, 200.5);
        assert_eq!(album.recordings, ["", "rec-2"]);
        let key = album.album_key.clone();

        record_lookup(&conn, &lookup(&key, LookupStatus::Unmatched, now - 10)).unwrap();
        assert!(albums_needing_lookup(&conn, now, month).unwrap().is_empty());

        record_lookup(
            &conn,
            &lookup(&key, LookupStatus::Unmatched, now - month - 1),
        )
        .unwrap();
        assert_eq!(albums_needing_lookup(&conn, now, month).unwrap().len(), 1);

        record_lookup(&conn, &lookup(&key, LookupStatus::Matched, now - 10)).unwrap();
        assert!(
            albums_needing_lookup(&conn, now, month).unwrap().is_empty(),
            "a fresh match without a cover is not retried until the window passes"
        );
        record_lookup(&conn, &lookup(&key, LookupStatus::Matched, now - month - 1)).unwrap();
        assert_eq!(albums_needing_lookup(&conn, now, month).unwrap().len(), 1);
        conn.execute(
            "INSERT INTO artwork(album_key, data) VALUES (?1, ?2)",
            params![key, vec![1u8]],
        )
        .unwrap();
        assert!(albums_needing_lookup(&conn, now, month).unwrap().is_empty());

        conn.execute("UPDATE albums SET tags_missing = 1", [])
            .unwrap();
        let need = albums_needing_lookup(&conn, now, month).unwrap();
        assert_eq!(need.len(), 1, "missing tags need work even with a cover");
        assert!(!need[0].needs_cover);
    }

    #[test]
    fn schema_creation_is_idempotent_and_has_fts5() {
        let conn = memory();
        ensure_schema(&conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        // A working FTS5 build is required for BM25 search.
        conn.execute_batch("CREATE VIRTUAL TABLE fts_probe USING fts5(x)")
            .unwrap();
    }

    #[test]
    fn out_of_date_schema_is_rebuilt() {
        let conn = memory();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        ensure_schema(&conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn corrupt_database_is_rebuilt() {
        let dir = std::env::temp_dir().join(format!("omamusic-db-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("library.db");
        std::fs::write(&path, b"this is not a sqlite database").unwrap();

        let db = Db::open(path).unwrap();
        assert_eq!(db.read(track_count).unwrap(), 0);
        let version: i64 = db
            .read(|conn| Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn triggers_keep_the_search_index_in_sync() {
        let conn = memory();
        insert_track(
            &conn,
            "/m/a.mp3",
            "Neon Lights",
            "Kraftwerk",
            "Trans Europe",
            1,
        );
        assert_eq!(
            search_tracks(&conn, "\"neon\"", 10).unwrap().len(),
            1,
            "insert should be searchable"
        );
        conn.execute(
            "UPDATE tracks SET title = 'Autobahn' WHERE path = '/m/a.mp3'",
            [],
        )
        .unwrap();
        assert!(search_tracks(&conn, "\"neon\"", 10).unwrap().is_empty());
        assert_eq!(search_tracks(&conn, "\"autobahn\"", 10).unwrap().len(), 1);
        conn.execute("DELETE FROM tracks WHERE path = '/m/a.mp3'", [])
            .unwrap();
        assert!(search_tracks(&conn, "\"autobahn\"", 10).unwrap().is_empty());
    }

    #[test]
    fn bm25_ranks_a_title_match_above_an_album_only_match() {
        let conn = memory();
        // "Moon" is in the title of one track and only in the album name of another.
        insert_track(&conn, "/m/1.mp3", "Moon River", "Andy", "Some Album", 1);
        insert_track(&conn, "/m/2.mp3", "Something Else", "Andy", "Moon", 2);
        let ids = search_tracks(&conn, "\"moon\"", 10).unwrap();
        assert_eq!(ids.len(), 2);
        let titles: Vec<String> = tracks_by_ids(&conn, &ids)
            .unwrap()
            .into_iter()
            .map(|t| t.title)
            .collect();
        assert_eq!(titles, ["Moon River", "Something Else"]);
    }

    #[test]
    fn album_search_uses_track_titles() {
        let conn = memory();
        insert_track(
            &conn,
            "/m/1.mp3",
            "Neon Lights",
            "Kraftwerk",
            "Trans Europe",
            1,
        );
        insert_track(
            &conn,
            "/m/2.mp3",
            "Autobahn",
            "Kraftwerk",
            "Trans Europe",
            2,
        );
        rebuild_albums(&conn);
        assert_eq!(search_albums(&conn, "\"autobahn\"", 10).unwrap().len(), 1);
        assert_eq!(search_albums(&conn, "\"kraftwerk\"", 10).unwrap().len(), 1);
        assert!(search_albums(&conn, "\"missing\"", 10).unwrap().is_empty());
    }

    #[test]
    fn bm25_matches_partial_words_as_prefixes() {
        let conn = memory();
        insert_track(
            &conn,
            "/m/1.mp3",
            "Neon Lights",
            "Kraftwerk",
            "Trans Europe",
            1,
        );
        assert_eq!(search_tracks(&conn, "\"neon\"*", 10).unwrap().len(), 1);
        assert_eq!(search_tracks(&conn, "\"neo\"*", 10).unwrap().len(), 1);
        // Prefix, not infix: "eon" is not the start of "neon".
        assert!(search_tracks(&conn, "\"eon\"*", 10).unwrap().is_empty());
    }

    #[test]
    fn genre_is_searchable_for_tracks_and_albums() {
        let conn = memory();
        insert_track(
            &conn,
            "/m/1.mp3",
            "Blazing Corpse",
            "Exhumed",
            "Gore Metal",
            1,
        );
        conn.execute(
            "UPDATE tracks SET genre = 'Death Metal' WHERE path = '/m/1.mp3'",
            [],
        )
        .unwrap();
        rebuild_albums(&conn);
        assert_eq!(search_tracks(&conn, "\"death\"*", 10).unwrap().len(), 1);
        assert_eq!(search_albums(&conn, "\"death\"*", 10).unwrap().len(), 1);
        assert!(search_tracks(&conn, "\"black\"*", 10).unwrap().is_empty());
    }

    #[test]
    fn multi_word_query_requires_every_word() {
        let conn = memory();
        insert_track(
            &conn,
            "/m/1.mp3",
            "Neon Lights",
            "Kraftwerk",
            "Trans Europe",
            1,
        );
        let both = crate::search::match_query("neon lights").unwrap();
        assert_eq!(search_tracks(&conn, &both, 10).unwrap().len(), 1);
        // Partial words are matched as prefixes end-to-end.
        let partial = crate::search::match_query("neo lig").unwrap();
        assert_eq!(search_tracks(&conn, &partial, 10).unwrap().len(), 1);
        let unrelated = crate::search::match_query("neon autobahn").unwrap();
        assert!(search_tracks(&conn, &unrelated, 10).unwrap().is_empty());
    }

    #[test]
    fn album_rollup_marks_mixed_artists_as_various() {
        let conn = memory();
        insert_track(&conn, "/m/1.mp3", "One", "A", "Mixed", 1);
        insert_track(&conn, "/m/2.mp3", "Two", "B", "Mixed", 2);
        insert_track(&conn, "/m/3.mp3", "Three", "C", "Solid", 1);
        rebuild_albums(&conn);
        let albums = albums_page(&conn, 0, 10, AlbumSort::Artist).unwrap();
        let mixed = albums.iter().find(|a| a.title == "Mixed").unwrap();
        assert_eq!(mixed.artist, "Various Artists");
        assert_eq!(mixed.track_count, 2);
        let solid = albums.iter().find(|a| a.title == "Solid").unwrap();
        assert_eq!(solid.artist, "C");
    }

    #[test]
    fn tracks_page_follows_album_order() {
        let conn = memory();
        insert_track(&conn, "/m/b.mp3", "B2", "Zeta", "Zed", 2);
        insert_track(&conn, "/m/a.mp3", "B1", "Zeta", "Zed", 1);
        insert_track(&conn, "/m/c.mp3", "A1", "Alpha", "Ay", 1);
        rebuild_albums(&conn);
        let titles: Vec<String> = tracks_page(&conn, 0, 10)
            .unwrap()
            .into_iter()
            .map(|t| t.title)
            .collect();
        assert_eq!(titles, ["A1", "B1", "B2"]);
    }

    #[test]
    fn albums_page_follows_the_chosen_sort() {
        let conn = memory();
        insert_track(&conn, "/m/a/1.mp3", "One", "Zeta", "Beta", 1);
        insert_track(&conn, "/m/b/1.mp3", "One", "Alpha", "Gamma", 1);
        insert_track(&conn, "/m/c/1.mp3", "One", "Mid", "Alef", 1);
        conn.execute_batch(
            "UPDATE tracks SET year = 1999, mtime_ns = 30 WHERE album = 'Beta';
             UPDATE tracks SET year = 2010, mtime_ns = 10 WHERE album = 'Gamma';
             UPDATE tracks SET year = 0, mtime_ns = 20 WHERE album = 'Alef';",
        )
        .unwrap();
        rebuild_albums(&conn);
        let titles = |sort| -> Vec<String> {
            albums_page(&conn, 0, 10, sort)
                .unwrap()
                .into_iter()
                .map(|a| a.title)
                .collect()
        };
        assert_eq!(titles(AlbumSort::Artist), ["Gamma", "Alef", "Beta"]);
        assert_eq!(titles(AlbumSort::Title), ["Alef", "Beta", "Gamma"]);
        assert_eq!(titles(AlbumSort::Year), ["Gamma", "Beta", "Alef"]);
        assert_eq!(titles(AlbumSort::RecentlyAdded), ["Beta", "Alef", "Gamma"]);
    }

    #[test]
    fn album_detail_carries_the_first_genre() {
        let conn = memory();
        insert_track(&conn, "/m/1.mp3", "One", "A", "Disc", 1);
        insert_track(&conn, "/m/2.mp3", "Two", "A", "Disc", 2);
        conn.execute_batch("UPDATE tracks SET genre = 'Synth-pop' WHERE title = 'One';")
            .unwrap();
        rebuild_albums(&conn);
        let key = crate::index::album_key("/m", "Disc");
        assert_eq!(
            album_detail(&conn, &key).unwrap().unwrap().genre,
            "Synth-pop"
        );
    }

    #[test]
    fn album_detail_orders_tracks_by_number() {
        let conn = memory();
        insert_track(&conn, "/m/z.mp3", "Second", "A", "Disc", 2);
        insert_track(&conn, "/m/a.mp3", "First", "A", "Disc", 1);
        rebuild_albums(&conn);
        let key = crate::index::album_key("/m", "Disc");
        let album = album_detail(&conn, &key).unwrap().unwrap();
        let titles: Vec<&str> = album.tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["First", "Second"]);
    }

    #[test]
    fn artwork_round_trips_and_distinguishes_none() {
        let conn = memory();
        insert_track(&conn, "/m/1.mp3", "One", "A", "Disc", 1);
        rebuild_albums(&conn);
        let key = crate::index::album_key("/m", "Disc");
        assert_eq!(artwork(&conn, &key).unwrap(), None);
        conn.execute(
            "INSERT INTO artwork(album_key, data) VALUES (?1, ?2)",
            params![key, vec![1u8, 2, 3]],
        )
        .unwrap();
        assert_eq!(artwork(&conn, &key).unwrap(), Some(vec![1, 2, 3]));
    }
}
