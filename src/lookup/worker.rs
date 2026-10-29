use crate::artwork::ArtworkCache;
use crate::db::{self, Db, LookupCandidate, LookupRow, LookupStatus};
use crate::index::{MISSING_ALBUM, MISSING_ARTIST, MISSING_YEAR};
use crate::library::{self, SharedSnapshot};
use crate::lookup::covers::fetch_cover;
use crate::lookup::fingerprint;
use crate::lookup::http::{Http, Response, Throttle};
use crate::lookup::musicbrainz::{self, Release};
use crate::lookup::score::{self, LocalAlbum, LocalTrack, Match};
use crate::lookup::text::normalize;
use crate::lookup::write::{self, TrackFill, WriteOutcome};
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const RETRY_AFTER_SECS: i64 = 30 * 24 * 60 * 60;
const MUSICBRAINZ_GAP: Duration = Duration::from_millis(1000);
const ACOUSTID_GAP: Duration = Duration::from_millis(340);
const MAX_RELEASE_GROUPS_FROM_FINGERPRINTS: usize = 2;
const BACKOFFS: [Duration; 2] = [Duration::from_secs(5), Duration::from_secs(30)];
const SCAN_POLL: Duration = Duration::from_millis(500);
const UNKNOWN_ARTIST: &str = "Unknown Artist";

#[derive(Debug)]
pub struct RateLimited;

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MusicBrainz rate limit")
    }
}

impl std::error::Error for RateLimited {}

#[derive(Debug)]
pub struct Offline;

impl std::fmt::Display for Offline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("network unreachable")
    }
}

impl std::error::Error for Offline {}

pub enum Wake {
    ScanFinished,
    SettingChanged(bool),
    Retry,
}

pub struct WorkerDeps {
    pub http: Arc<dyn Http>,
    pub db_path: PathBuf,
    pub cache_dir: PathBuf,
    pub covers: ArtworkCache,
    pub state: SharedSnapshot,
    pub acoustid_key: String,
}

pub struct Limits {
    pub musicbrainz: Throttle,
    pub acoustid: Throttle,
}

impl Limits {
    pub fn new() -> Self {
        Self {
            musicbrainz: Throttle::new(MUSICBRAINZ_GAP),
            acoustid: Throttle::new(ACOUSTID_GAP),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn unlimited() -> Self {
        Self {
            musicbrainz: Throttle::new(Duration::ZERO),
            acoustid: Throttle::new(Duration::ZERO),
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Worker;

impl Worker {
    pub fn spawn(deps: WorkerDeps, enabled: bool) -> Sender<Wake> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || run_thread(deps, rx, enabled));
        tx
    }
}

fn run_thread(deps: WorkerDeps, rx: Receiver<Wake>, mut enabled: bool) {
    let conn = match Db::open_writer(&deps.db_path) {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("online lookup: open database: {err:#}");
            return;
        }
    };
    let limits = Limits::new();
    publish_unmatched(&deps, &conn);
    loop {
        let wake = match rx.recv() {
            Ok(wake) => wake,
            Err(_) => return,
        };
        match wake {
            Wake::SettingChanged(on) => enabled = on,
            Wake::Retry => {
                if let Err(err) = db::clear_unmatched(&conn) {
                    eprintln!("online lookup: clear unmatched: {err:#}");
                }
                publish_unmatched(&deps, &conn);
            }
            Wake::ScanFinished => {}
        }
        if enabled {
            enabled = run_pass(&deps, &conn, &limits, &rx);
        }
    }
}

fn run_pass(
    deps: &WorkerDeps,
    conn: &rusqlite::Connection,
    limits: &Limits,
    rx: &Receiver<Wake>,
) -> bool {
    loop {
        let (enabled, rerun) = run_once(deps, conn, limits, rx);
        if !enabled || !rerun {
            return enabled;
        }
    }
}

fn run_once(
    deps: &WorkerDeps,
    conn: &rusqlite::Connection,
    limits: &Limits,
    rx: &Receiver<Wake>,
) -> (bool, bool) {
    let mut enabled = true;
    let mut rerun = false;
    let albums = match db::albums_needing_lookup(conn, unix_now(), RETRY_AFTER_SECS) {
        Ok(albums) => albums,
        Err(err) => {
            eprintln!("online lookup: list albums: {err:#}");
            return (enabled, false);
        }
    };
    let total = albums.len();
    library::update(&deps.state, |s| {
        s.lookup_total = total;
        s.lookup_pending = total;
    });
    for (done, album) in albums.into_iter().enumerate() {
        loop {
            let drained = drain(rx);
            if drained.disconnected {
                return (false, false);
            }
            if let Some(on) = drained.enabled {
                enabled = on;
            }
            rerun |= drained.rerun;
            if !enabled || !deps.state.read().scanning {
                break;
            }
            std::thread::sleep(SCAN_POLL);
        }
        if !enabled {
            break;
        }
        let key = (!deps.acoustid_key.is_empty()).then_some(deps.acoustid_key.as_str());
        let attempt = || {
            process_album(
                &*deps.http,
                limits,
                &deps.cache_dir,
                key,
                &album,
                unix_now(),
            )
        };
        let row = match with_backoff(attempt, std::thread::sleep) {
            Ok(Some(row)) => row,
            Ok(None) => {
                eprintln!("online lookup: MusicBrainz keeps rate limiting, stopping for now");
                break;
            }
            Err(err) => {
                eprintln!("online lookup: {err:#}, stopping for now");
                break;
            }
        };
        if let Err(err) = db::record_lookup(conn, &row) {
            eprintln!("online lookup: record: {err:#}");
        }
        if let Err(err) = store_cover(conn, &deps.cache_dir, &album.album_key) {
            eprintln!("online lookup: store cover: {err:#}");
        }
        deps.covers.remove(&album.album_key);
        library::update(&deps.state, |s| s.lookup_pending = total - done - 1);
        publish_unmatched(deps, conn);
    }
    library::update(&deps.state, |s| {
        s.lookup_total = 0;
        s.lookup_pending = 0;
    });
    (enabled, rerun)
}

struct Drained {
    enabled: Option<bool>,
    rerun: bool,
    disconnected: bool,
}

fn drain(rx: &Receiver<Wake>) -> Drained {
    let mut drained = Drained {
        enabled: None,
        rerun: false,
        disconnected: false,
    };
    loop {
        match rx.try_recv() {
            Ok(Wake::SettingChanged(on)) => drained.enabled = Some(on),
            Ok(Wake::ScanFinished | Wake::Retry) => drained.rerun = true,
            Err(TryRecvError::Empty) => return drained,
            Err(TryRecvError::Disconnected) => {
                drained.disconnected = true;
                return drained;
            }
        }
    }
}

fn with_backoff(
    mut attempt: impl FnMut() -> Result<LookupRow>,
    mut sleep: impl FnMut(Duration),
) -> Result<Option<LookupRow>> {
    let mut backoffs = BACKOFFS.iter();
    loop {
        match attempt() {
            Ok(row) => return Ok(Some(row)),
            Err(err) if err.downcast_ref::<RateLimited>().is_some() => match backoffs.next() {
                Some(wait) => sleep(*wait),
                None => return Ok(None),
            },
            Err(err) => return Err(err),
        }
    }
}

fn store_cover(conn: &rusqlite::Connection, cache_dir: &Path, album_key: &str) -> Result<bool> {
    let bytes = match std::fs::read(write::cache_path(cache_dir, album_key)) {
        Ok(bytes) if !bytes.is_empty() => bytes,
        _ => return Ok(false),
    };
    db::set_artwork(conn, album_key, &bytes)?;
    Ok(true)
}

fn publish_unmatched(deps: &WorkerDeps, conn: &rusqlite::Connection) {
    match db::unmatched_albums(conn) {
        Ok(list) => library::update(&deps.state, |s| s.lookup_unmatched = list),
        Err(err) => eprintln!("online lookup: list unmatched: {err:#}"),
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn error_row(album_key: &str, now: i64) -> LookupRow {
    LookupRow {
        album_key: album_key.to_string(),
        status: LookupStatus::Error,
        release: None,
        release_group: None,
        score: None,
        candidate: None,
        attempted_at: now,
    }
}

pub fn process_album(
    http: &dyn Http,
    limits: &Limits,
    cache_dir: &Path,
    acoustid_key: Option<&str>,
    album: &LookupCandidate,
    now: i64,
) -> Result<LookupRow> {
    match identify_and_write(http, limits, cache_dir, acoustid_key, album, now) {
        Ok(row) => Ok(row),
        Err(err)
            if err.downcast_ref::<RateLimited>().is_some()
                || err.downcast_ref::<Offline>().is_some() =>
        {
            Err(err)
        }
        Err(err) => {
            eprintln!("online lookup: {}: {err:#}", album.title);
            Ok(error_row(&album.album_key, now))
        }
    }
}

fn identify_and_write(
    http: &dyn Http,
    limits: &Limits,
    cache_dir: &Path,
    acoustid_key: Option<&str>,
    album: &LookupCandidate,
    now: i64,
) -> Result<LookupRow> {
    let mut local = local_album(album);
    let mut row = LookupRow {
        album_key: album.album_key.clone(),
        status: LookupStatus::Unmatched,
        release: None,
        release_group: None,
        score: None,
        candidate: None,
        attempted_at: now,
    };
    let searchable = !(local.title_from_folder && local.artist.is_none());
    let mut best = if searchable {
        let search = if local.has_ids {
            musicbrainz::release_group_search_url(&album.mb_release_group)
        } else {
            musicbrainz::search_url(&local.title, local.artist.as_deref(), local.tracks.len())
        };
        best_candidate(http, limits, &local, &search)?
    } else {
        None
    };
    let accepted = best.as_ref().is_some_and(|(_, m)| score::accept(m, &local));
    if !accepted {
        if let Some(key) = acoustid_key {
            if let Some((by_sound, found)) = identify_by_sound(http, limits, key, album)? {
                if found
                    .as_ref()
                    .is_some_and(|(_, m)| score::accept(m, &by_sound))
                {
                    local = by_sound;
                    best = found;
                } else if best.is_none() {
                    best = found;
                }
            }
        }
    }
    if let Some((release, m)) = &best {
        row.score = Some(m.distance);
        row.candidate = Some(describe(release));
    }
    if !searchable && best.is_none() {
        return Ok(row);
    }

    match best {
        Some((release, m)) if score::accept(&m, &local) => {
            let cover = album
                .needs_cover
                .then(|| {
                    fetch_cover(
                        http,
                        &limits.musicbrainz,
                        Some(&release.id),
                        Some(&release.release_group),
                        &release.artist,
                        &release.title,
                    )
                })
                .flatten();
            if let Some(cover) = &cover {
                write::save_cache_cover(cache_dir, &album.album_key, cover)?;
            }
            write_tracks(album, &local, &release, &m, cover.as_deref());
            row.status = LookupStatus::Matched;
            row.release = Some(release.id);
            row.release_group = Some(release.release_group);
        }
        _ => {
            if album.needs_cover && album.tags_missing == 0 {
                if let Some(artist) = &local.artist {
                    if let Some(cover) =
                        fetch_cover(http, &limits.musicbrainz, None, None, artist, &local.title)
                    {
                        write::save_cache_cover(cache_dir, &album.album_key, &cover)?;
                    }
                }
            }
        }
    }
    Ok(row)
}

fn local_album(album: &LookupCandidate) -> LocalAlbum {
    let title_from_folder = album.tags_missing & MISSING_ALBUM != 0;
    let artist_known = album.tags_missing & MISSING_ARTIST == 0
        && album.artist != UNKNOWN_ARTIST
        && !album.artist.is_empty()
        && !(title_from_folder && normalize(&album.artist) == "various artists");
    LocalAlbum {
        title: album.title.clone(),
        title_from_folder,
        artist: artist_known.then(|| album.artist.clone()),
        year: album.year,
        has_ids: !album.mb_release_group.is_empty(),
        tracks: album
            .tracks
            .iter()
            .map(|track| {
                let stem = track
                    .path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();
                LocalTrack {
                    number: track.track_number,
                    title: (track.title != stem && !track.title.is_empty())
                        .then(|| track.title.clone()),
                    duration: track.duration,
                    recording: None,
                }
            })
            .collect(),
    }
}

/// Fingerprint every track, ask AcoustID, and score the release groups the
/// recordings belong to. Returns the local album annotated with recording
/// ids and the best candidate, or `None` when nothing was recognised.
type Candidate = Option<(Release, Match)>;

fn identify_by_sound(
    http: &dyn Http,
    limits: &Limits,
    key: &str,
    album: &LookupCandidate,
) -> Result<Option<(LocalAlbum, Candidate)>> {
    let mut local = local_album(album);
    let mut hits = Vec::with_capacity(album.tracks.len());
    for track in &album.tracks {
        let hit = match fingerprint::fingerprint(&track.path) {
            Ok(print) => {
                limits.acoustid.wait();
                let url = fingerprint::lookup_url(key, print.duration, &print.compressed);
                let Response { status, body } = http.get(&url).map_err(|err| {
                    eprintln!("online lookup: AcoustID: {err:#}");
                    anyhow::Error::from(Offline)
                })?;
                match status {
                    200 => fingerprint::parse_lookup(&body).unwrap_or_else(|err| {
                        eprintln!("online lookup: {}: {err:#}", track.path.display());
                        fingerprint::Hit::default()
                    }),
                    other => {
                        eprintln!("online lookup: AcoustID answered {other}");
                        fingerprint::Hit::default()
                    }
                }
            }
            Err(err) => {
                eprintln!("online lookup: {err:#}");
                fingerprint::Hit::default()
            }
        };
        hits.push(hit);
    }
    let groups = fingerprint::vote_release_groups(&hits);
    if groups.is_empty() {
        return Ok(None);
    }
    for (track, hit) in local.tracks.iter_mut().zip(&hits) {
        track.recording = hit.recording.clone();
    }
    local.has_ids = true;
    let mut best: Option<(Release, Match)> = None;
    for group in groups.iter().take(MAX_RELEASE_GROUPS_FROM_FINGERPRINTS) {
        let found = best_candidate(
            http,
            limits,
            &local,
            &musicbrainz::release_group_search_url(group),
        )?;
        if let Some((release, m)) = found {
            if best.as_ref().is_none_or(|(_, b)| m.distance < b.distance) {
                best = Some((release, m));
            }
        }
    }
    Ok(Some((local, best)))
}

fn best_candidate(
    http: &dyn Http,
    limits: &Limits,
    local: &LocalAlbum,
    search: &str,
) -> Result<Option<(Release, Match)>> {
    let ids = musicbrainz::parse_search(&musicbrainz_get(http, limits, search)?)?;
    let mut best: Option<(Release, Match)> = None;
    for id in ids {
        let body = match musicbrainz_get(http, limits, &musicbrainz::release_url(&id)) {
            Ok(body) => body,
            Err(err)
                if err.downcast_ref::<RateLimited>().is_some()
                    || err.downcast_ref::<Offline>().is_some() =>
            {
                return Err(err)
            }
            Err(err) => {
                eprintln!("online lookup: skipping candidate {id}: {err:#}");
                continue;
            }
        };
        let Ok(release) = musicbrainz::parse_release(&body) else {
            continue;
        };
        let m = score::distance(local, &release);
        if best.as_ref().is_none_or(|(_, b)| m.distance < b.distance) {
            best = Some((release, m));
        }
    }
    Ok(best)
}

fn musicbrainz_get(http: &dyn Http, limits: &Limits, url: &str) -> Result<Vec<u8>> {
    limits.musicbrainz.wait();
    let Response { status, body } = http.get(url).map_err(|err| {
        eprintln!("online lookup: {url}: {err:#}");
        anyhow::Error::from(Offline)
    })?;
    match status {
        200 => Ok(body),
        503 => Err(RateLimited.into()),
        other => Err(anyhow!("MusicBrainz answered {other} for {url}")),
    }
}

fn describe(release: &Release) -> String {
    if release.year > 0 {
        format!("{} – {} ({})", release.artist, release.title, release.year)
    } else {
        format!("{} – {}", release.artist, release.title)
    }
}

fn write_tracks(
    album: &LookupCandidate,
    local: &LocalAlbum,
    release: &Release,
    m: &Match,
    cover: Option<&[u8]>,
) {
    for ((track, local_track), pair) in album.tracks.iter().zip(&local.tracks).zip(&m.pairing) {
        let Some(other) = pair.map(|i| &release.tracks[i]) else {
            continue;
        };
        let fill = TrackFill {
            title: local_track.title.is_none().then_some(other.title.as_str()),
            artist: local.artist.is_none().then_some(other.artist.as_str()),
            album: local.title_from_folder.then_some(release.title.as_str()),
            year: (album.tags_missing & MISSING_YEAR != 0).then_some(release.year),
            track_number: (local_track.number == 0 && release.media == 1).then_some(other.number),
            release_group: Some(release.release_group.as_str()),
            release: Some(release.id.as_str()),
            recording: Some(other.recording.as_str()),
        };
        write_one(&track.path, &fill, cover);
    }
}

fn write_one(path: &Path, fill: &TrackFill, cover: Option<&[u8]>) {
    match write::fill_file(path, fill, cover) {
        Ok(WriteOutcome::Written | WriteOutcome::Unchanged) => {}
        Ok(WriteOutcome::ReadOnly) => {
            eprintln!("online lookup: read-only, skipped {}", path.display())
        }
        Ok(WriteOutcome::Linked) => {
            eprintln!("online lookup: linked file, skipped {}", path.display())
        }
        Err(err) => eprintln!("online lookup: write {}: {err:#}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{self, Db, LookupStatus};
    use crate::index;
    use crate::lookup::http::FakeHttp;
    use crate::lookup::score::ACCEPT_THRESHOLD;
    use crate::metadata::{self, fixtures};
    use lofty::config::WriteOptions;
    use lofty::prelude::*;
    use lofty::tag::{Tag, TagType};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        root: PathBuf,
        music: PathBuf,
        cache: PathBuf,
        conn: rusqlite::Connection,
    }

    impl Fixture {
        fn new(folder: &str, artist: Option<&str>) -> Self {
            Self::build(folder, artist, false)
        }

        fn complete(folder: &str, artist: &str) -> Self {
            Self::build(folder, Some(artist), true)
        }

        fn tones(folder: &str) -> Self {
            let mut fx = Self::build(folder, None, false);
            for (i, (seconds, hz)) in [(3u32, 300.0), (4, 600.0), (5, 900.0)]
                .into_iter()
                .enumerate()
            {
                std::fs::write(
                    fx.music.join(format!("track{}.wav", i + 1)),
                    fixtures::wav_tone(seconds, hz),
                )
                .unwrap();
            }
            index::reconcile(&mut fx.conn, std::slice::from_ref(&fx.music), &fx.cache).unwrap();
            fx
        }

        fn loose(folder: &str) -> Self {
            let mut fx = Self::build(folder, None, false);
            for (i, artist) in ["A", "B", "C"].into_iter().enumerate() {
                let path = fx.music.join(format!("track{}.wav", i + 1));
                let mut tag = Tag::new(TagType::Id3v2);
                tag.set_artist(artist.into());
                tag.save_to_path(&path, WriteOptions::default()).unwrap();
            }
            index::reconcile(&mut fx.conn, std::slice::from_ref(&fx.music), &fx.cache).unwrap();
            fx
        }

        fn build(folder: &str, artist: Option<&str>, full: bool) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("omamusic-worker-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let music = root.join(folder);
            let cache = root.join("cache");
            std::fs::create_dir_all(&music).unwrap();
            std::fs::create_dir_all(&cache).unwrap();
            for (i, seconds) in [1u32, 2, 3].into_iter().enumerate() {
                let path = music.join(format!("track{}.wav", i + 1));
                std::fs::write(&path, fixtures::wav_bytes(seconds)).unwrap();
                if let Some(artist) = artist {
                    let mut tag = Tag::new(TagType::Id3v2);
                    tag.set_artist(artist.into());
                    tag.set_album(folder.into());
                    if full {
                        tag.set_title(format!("Song {}", i + 1));
                        tag.set_year(2002);
                    }
                    tag.save_to_path(&path, WriteOptions::default()).unwrap();
                }
            }
            let mut conn = Db::open_writer(&root.join("library.db")).unwrap();
            index::reconcile(&mut conn, std::slice::from_ref(&music), &cache).unwrap();
            Self {
                root,
                music,
                cache,
                conn,
            }
        }

        fn candidate(&self) -> db::LookupCandidate {
            let mut all = db::albums_needing_lookup(&self.conn, NOW, RETRY_AFTER_SECS).unwrap();
            assert_eq!(all.len(), 1);
            all.remove(0)
        }

        fn tracks(&self) -> Vec<metadata::AudioMetadata> {
            (1..=3)
                .map(|i| metadata::read(&self.music.join(format!("track{i}.wav"))))
                .collect()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.music, std::fs::Permissions::from_mode(0o755));
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    const NOW: i64 = 1_800_000_000;

    fn png(side: u32) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_pixel(side, side, image::Rgb([0, 0, 200]))
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    const SEARCH: &str = r#"{"releases":[{"id":"r1"}]}"#;
    const RELEASE: &str = r#"{
        "id":"r1","title":"Estranhos no Ninho","date":"2002-06-01",
        "artist-credit":[{"name":"Exxótica","joinphrase":""}],
        "release-group":{"id":"g1","first-release-date":"2002"},
        "media":[{"tracks":[
            {"position":1,"title":"No Palco","length":1000,"recording":{"id":"rec-1"}},
            {"position":2,"title":"Fruto Proibido","length":2000,"recording":{"id":"rec-2"}},
            {"position":3,"title":"Gato Preto","length":3000,"recording":{"id":"rec-3"}}
        ]}]}"#;

    fn matching_http() -> FakeHttp {
        FakeHttp::new()
            .on(
                "https://musicbrainz.org/ws/2/release/?",
                200,
                SEARCH.as_bytes().to_vec(),
            )
            .on(
                "https://musicbrainz.org/ws/2/release/r1",
                200,
                RELEASE.as_bytes().to_vec(),
            )
            .on("https://coverartarchive.org/release/r1", 200, png(400))
    }

    fn throttle() -> Limits {
        Limits::unlimited()
    }

    #[test]
    fn matched_album_gets_tags_cover_and_cache_file() {
        let mut fx = Fixture::new("Estranhos no Ninho", Some("Exxótica"));
        let candidate = fx.candidate();
        assert_eq!(
            candidate.tags_missing,
            index::MISSING_TITLE | index::MISSING_YEAR
        );
        let http = matching_http();

        let row = process_album(&http, &throttle(), &fx.cache, None, &candidate, NOW).unwrap();
        assert_eq!(row.status, LookupStatus::Matched);
        assert_eq!(row.release.as_deref(), Some("r1"));
        assert_eq!(row.release_group.as_deref(), Some("g1"));
        assert_eq!(row.attempted_at, NOW);
        assert!(row.score.unwrap() < 0.05);

        let tracks = fx.tracks();
        assert_eq!(tracks[0].title, "No Palco");
        assert_eq!(tracks[1].title, "Fruto Proibido");
        assert_eq!(tracks[2].title, "Gato Preto");
        for track in &tracks {
            assert_eq!(track.artist, "Exxótica");
            assert_eq!(track.album, "Estranhos no Ninho");
            assert_eq!(track.year, 2002);
            assert_eq!(track.mb_release, "r1");
            assert_eq!(track.mb_release_group, "g1");
            assert!(track.artwork.is_some(), "cover embedded");
        }
        assert_eq!(tracks[0].track_number, 1);
        assert_eq!(tracks[2].track_number, 3);
        assert!(write::cache_path(&fx.cache, &candidate.album_key).exists());

        let requests = http.requests();
        assert!(requests[0].starts_with("https://musicbrainz.org/ws/2/release/?"));
        assert!(requests[0].contains("Exx%C3%B3tica"));
        assert_eq!(requests[1], "https://musicbrainz.org/ws/2/release/r1?fmt=json&inc=recordings+artist-credits+release-groups");
        assert_eq!(
            requests[2],
            "https://coverartarchive.org/release/r1/front-500"
        );
        assert_eq!(requests.len(), 3);

        db::record_lookup(&fx.conn, &row).unwrap();
        let stats = index::reconcile(&mut fx.conn, &[fx.music.clone()], &fx.cache).unwrap();
        assert_eq!(stats.updated, 3, "rewritten files are re-read");
        assert!(db::artwork(&fx.conn, &candidate.album_key)
            .unwrap()
            .is_some());
        let bits: i64 = fx
            .conn
            .query_row("SELECT tags_missing FROM albums", [], |row| row.get(0))
            .unwrap();
        assert_eq!(bits, 0);
        assert!(db::albums_needing_lookup(&fx.conn, NOW, RETRY_AFTER_SECS)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn no_candidates_marks_unmatched_without_touching_files() {
        let fx = Fixture::new("Estranhos no Ninho", Some("Exxótica"));
        let candidate = fx.candidate();
        let before = fx.tracks();
        let http = FakeHttp::new().on(
            "https://musicbrainz.org/ws/2/release/?",
            200,
            br#"{"releases":[]}"#.to_vec(),
        );
        let row = process_album(&http, &throttle(), &fx.cache, None, &candidate, NOW).unwrap();
        assert_eq!(row.status, LookupStatus::Unmatched);
        assert_eq!(row.candidate, None);
        assert_eq!(fx.tracks(), before);
        assert!(!write::cache_path(&fx.cache, &candidate.album_key).exists());
    }

    #[test]
    fn poor_candidate_is_unmatched_and_named() {
        let fx = Fixture::new("Estranhos no Ninho", Some("Exxótica"));
        let candidate = fx.candidate();
        let release = r#"{"id":"r1","title":"Kid A","date":"2000-10-02",
            "artist-credit":[{"name":"Radiohead","joinphrase":""}],
            "release-group":{"id":"g1"},
            "media":[{"tracks":[
                {"position":1,"title":"Everything","length":298000,"recording":{"id":"a"}},
                {"position":2,"title":"Kid A","length":284000,"recording":{"id":"b"}},
                {"position":3,"title":"Idioteque","length":309000,"recording":{"id":"c"}}
            ]}]}"#;
        let http = FakeHttp::new()
            .on(
                "https://musicbrainz.org/ws/2/release/?",
                200,
                SEARCH.as_bytes().to_vec(),
            )
            .on(
                "https://musicbrainz.org/ws/2/release/r1",
                200,
                release.as_bytes().to_vec(),
            );
        let row = process_album(&http, &throttle(), &fx.cache, None, &candidate, NOW).unwrap();
        assert_eq!(row.status, LookupStatus::Unmatched);
        assert_eq!(row.candidate.as_deref(), Some("Radiohead – Kid A (2000)"));
        assert!(row.score.unwrap() > ACCEPT_THRESHOLD);
        assert_eq!(fx.tracks()[0].title, "");
    }

    #[test]
    fn folder_named_album_without_artist_makes_no_request() {
        let fx = Fixture::new("Downloads", None);
        let candidate = fx.candidate();
        let http = matching_http();
        let row = process_album(&http, &throttle(), &fx.cache, None, &candidate, NOW).unwrap();
        assert_eq!(row.status, LookupStatus::Unmatched);
        assert!(http.requests().is_empty());
    }

    #[test]
    fn read_only_files_still_match_with_cache_cover() {
        let fx = Fixture::new("Estranhos no Ninho", Some("Exxótica"));
        let candidate = fx.candidate();
        for i in 1..=3 {
            std::fs::set_permissions(
                fx.music.join(format!("track{i}.wav")),
                std::fs::Permissions::from_mode(0o444),
            )
            .unwrap();
        }
        std::fs::set_permissions(&fx.music, std::fs::Permissions::from_mode(0o555)).unwrap();
        let row = process_album(
            &matching_http(),
            &throttle(),
            &fx.cache,
            None,
            &candidate,
            NOW,
        )
        .unwrap();
        assert_eq!(row.status, LookupStatus::Matched);
        assert!(write::cache_path(&fx.cache, &candidate.album_key).exists());
        assert_eq!(fx.tracks()[0].title, "", "file left alone");
    }

    #[test]
    fn transport_error_aborts_without_a_row() {
        struct Broken;
        impl Http for Broken {
            fn get(&self, _url: &str) -> anyhow::Result<Response> {
                anyhow::bail!("no network")
            }
        }
        let fx = Fixture::new("Estranhos no Ninho", Some("Exxótica"));
        let candidate = fx.candidate();
        let err =
            process_album(&Broken, &throttle(), &fx.cache, None, &candidate, NOW).unwrap_err();
        assert!(err.downcast_ref::<Offline>().is_some());
    }

    #[test]
    fn bad_server_answer_is_recorded_as_error() {
        let fx = Fixture::new("Estranhos no Ninho", Some("Exxótica"));
        let candidate = fx.candidate();
        let http = FakeHttp::new().on(
            "https://musicbrainz.org/ws/2/release/?",
            200,
            b"<html>".to_vec(),
        );
        let row = process_album(&http, &throttle(), &fx.cache, None, &candidate, NOW).unwrap();
        assert_eq!(row.status, LookupStatus::Error);
        assert_eq!(row.attempted_at, NOW);
    }

    #[test]
    fn a_failing_candidate_fetch_skips_that_candidate() {
        let fx = Fixture::new("Estranhos no Ninho", Some("Exxótica"));
        let candidate = fx.candidate();
        let http = FakeHttp::new()
            .on(
                "https://musicbrainz.org/ws/2/release/?",
                200,
                br#"{"releases":[{"id":"r0"},{"id":"r1"}]}"#.to_vec(),
            )
            .on("https://musicbrainz.org/ws/2/release/r0", 500, b"".to_vec())
            .on(
                "https://musicbrainz.org/ws/2/release/r1",
                200,
                RELEASE.as_bytes().to_vec(),
            )
            .on("https://coverartarchive.org/release/r1", 200, png(400));
        let row = process_album(&http, &throttle(), &fx.cache, None, &candidate, NOW).unwrap();
        assert_eq!(row.status, LookupStatus::Matched);
        assert_eq!(row.release.as_deref(), Some("r1"));
    }

    const TONES_RELEASE: &str = r#"{
        "id":"r1","title":"Estranhos no Ninho","date":"2002-06-01",
        "artist-credit":[{"name":"Exxótica","joinphrase":""}],
        "release-group":{"id":"g1","first-release-date":"2002"},
        "media":[{"tracks":[
            {"position":1,"title":"No Palco","length":3000,"recording":{"id":"rec-1"}},
            {"position":2,"title":"Fruto Proibido","length":4000,"recording":{"id":"rec-2"}},
            {"position":3,"title":"Gato Preto","length":5000,"recording":{"id":"rec-3"}}
        ]}]}"#;

    fn acoustid_routes(fx: &Fixture, http: FakeHttp) -> FakeHttp {
        let mut http = http;
        for i in 1..=3 {
            let fp =
                crate::lookup::fingerprint::fingerprint(&fx.music.join(format!("track{i}.wav")))
                    .unwrap();
            let body = format!(
                r#"{{"status":"ok","results":[{{"id":"x","score":0.95,"recordings":[{{"id":"rec-{i}","releasegroups":[{{"id":"g1"}}]}}]}}]}}"#
            );
            http = http.on(
                &crate::lookup::fingerprint::lookup_url("k3y", fp.duration, &fp.compressed),
                200,
                body.into_bytes(),
            );
        }
        http
    }

    #[test]
    fn untagged_folder_is_identified_by_fingerprint() {
        let fx = Fixture::tones("Downloads");
        let candidate = fx.candidate();
        let http = acoustid_routes(
            &fx,
            FakeHttp::new()
                .on(
                    "https://musicbrainz.org/ws/2/release/?",
                    200,
                    SEARCH.as_bytes().to_vec(),
                )
                .on(
                    "https://musicbrainz.org/ws/2/release/r1",
                    200,
                    TONES_RELEASE.as_bytes().to_vec(),
                )
                .on("https://coverartarchive.org/release/r1", 200, png(400)),
        );
        let row =
            process_album(&http, &throttle(), &fx.cache, Some("k3y"), &candidate, NOW).unwrap();
        assert_eq!(row.status, LookupStatus::Matched, "{row:?}");
        let tracks = fx.tracks();
        assert_eq!(tracks[0].title, "No Palco");
        assert_eq!(tracks[1].title, "Fruto Proibido");
        assert_eq!(tracks[2].title, "Gato Preto");
        assert_eq!(tracks[0].artist, "Exxótica");
        assert_eq!(tracks[0].album, "Estranhos no Ninho");
        assert_eq!(tracks[2].mb_recording, "rec-3");
        let requests = http.requests();
        assert!(
            requests
                .iter()
                .take(3)
                .all(|u| u.starts_with("https://api.acoustid.org/v2/lookup?client=k3y")),
            "{requests:?}"
        );
        assert!(requests[3].contains("rgid%3Ag1"), "{requests:?}");
    }

    #[test]
    fn without_a_key_untagged_folders_stay_unmatched_offline() {
        let fx = Fixture::tones("Downloads");
        let candidate = fx.candidate();
        let http = matching_http();
        let row = process_album(&http, &throttle(), &fx.cache, None, &candidate, NOW).unwrap();
        assert_eq!(row.status, LookupStatus::Unmatched);
        assert!(http.requests().is_empty());
    }

    #[test]
    fn loose_various_artists_folder_makes_no_request() {
        let fx = Fixture::loose("Downloads");
        let candidate = fx.candidate();
        assert_eq!(candidate.artist, "Various Artists");
        let http = matching_http();
        let row = process_album(&http, &throttle(), &fx.cache, None, &candidate, NOW).unwrap();
        assert_eq!(row.status, LookupStatus::Unmatched);
        assert!(http.requests().is_empty());
    }

    #[test]
    fn store_cover_fills_the_artwork_row_from_the_cache() {
        let fx = Fixture::new("Estranhos no Ninho", Some("Exxótica"));
        let candidate = fx.candidate();
        assert!(!store_cover(&fx.conn, &fx.cache, &candidate.album_key).unwrap());
        write::save_cache_cover(&fx.cache, &candidate.album_key, &png(400)).unwrap();
        assert!(store_cover(&fx.conn, &fx.cache, &candidate.album_key).unwrap());
        assert_eq!(
            db::artwork(&fx.conn, &candidate.album_key).unwrap(),
            Some(png(400))
        );
    }

    #[test]
    fn drain_collects_setting_changes_and_reruns() {
        let (tx, rx) = mpsc::channel();
        tx.send(Wake::ScanFinished).unwrap();
        tx.send(Wake::SettingChanged(false)).unwrap();
        tx.send(Wake::Retry).unwrap();
        let drained = drain(&rx);
        assert_eq!(drained.enabled, Some(false));
        assert!(drained.rerun);
        assert!(!drained.disconnected);
        assert!(!drain(&rx).rerun);
        drop(tx);
        assert!(drain(&rx).disconnected);
    }

    #[test]
    fn backoff_retries_the_same_album_then_gives_up() {
        let mut calls = 0;
        let mut sleeps = Vec::new();
        let row = with_backoff(
            || {
                calls += 1;
                if calls < 3 {
                    Err(RateLimited.into())
                } else {
                    Ok(error_row("k", NOW))
                }
            },
            |d| sleeps.push(d),
        );
        assert!(row.unwrap().is_some());
        assert_eq!(calls, 3);
        assert_eq!(sleeps, BACKOFFS);

        let mut sleeps = Vec::new();
        let row = with_backoff(|| Err(RateLimited.into()), |d| sleeps.push(d));
        assert!(row.unwrap().is_none(), "three rate limits abandon the pass");
        assert_eq!(sleeps, BACKOFFS);

        let row = with_backoff(|| Err(Offline.into()), |_| {});
        assert!(row.unwrap_err().downcast_ref::<Offline>().is_some());
    }

    #[test]
    fn multi_disc_release_leaves_track_numbers_alone() {
        let fx = Fixture::new("Estranhos no Ninho", Some("Exxótica"));
        let candidate = fx.candidate();
        let release = r#"{
            "id":"r1","title":"Estranhos no Ninho","date":"2002-06-01",
            "artist-credit":[{"name":"Exxótica","joinphrase":""}],
            "release-group":{"id":"g1"},
            "media":[
                {"tracks":[
                    {"position":1,"title":"No Palco","length":1000,"recording":{"id":"rec-1"}},
                    {"position":2,"title":"Fruto Proibido","length":2000,"recording":{"id":"rec-2"}}
                ]},
                {"tracks":[
                    {"position":1,"title":"Gato Preto","length":3000,"recording":{"id":"rec-3"}}
                ]}
            ]}"#;
        let http = FakeHttp::new()
            .on(
                "https://musicbrainz.org/ws/2/release/?",
                200,
                SEARCH.as_bytes().to_vec(),
            )
            .on(
                "https://musicbrainz.org/ws/2/release/r1",
                200,
                release.as_bytes().to_vec(),
            );
        let row = process_album(&http, &throttle(), &fx.cache, None, &candidate, NOW).unwrap();
        assert_eq!(row.status, LookupStatus::Matched);
        let tracks = fx.tracks();
        assert_eq!(tracks[2].title, "Gato Preto");
        assert_eq!(
            tracks[2].track_number, 0,
            "position 3 would be wrong for disc 2 track 1"
        );
    }

    #[test]
    fn rate_limit_is_reported_to_the_caller() {
        let fx = Fixture::new("Estranhos no Ninho", Some("Exxótica"));
        let candidate = fx.candidate();
        let http = FakeHttp::new().on("https://musicbrainz.org/ws/2/release/?", 503, b"".to_vec());
        let err = process_album(&http, &throttle(), &fx.cache, None, &candidate, NOW).unwrap_err();
        assert!(err.downcast_ref::<RateLimited>().is_some());
    }

    #[test]
    fn incomplete_tags_never_take_a_cover_by_name() {
        let fx = Fixture::new("Estranhos no Ninho", Some("Exxótica"));
        let candidate = fx.candidate();
        let itunes = r#"{"results":[{"artistName":"Exxótica","collectionName":"Estranhos no Ninho","artworkUrl100":"https://x/100x100bb.jpg"}]}"#;
        let http = FakeHttp::new()
            .on(
                "https://musicbrainz.org/ws/2/release/?",
                200,
                br#"{"releases":[]}"#.to_vec(),
            )
            .on(
                "https://itunes.apple.com/search",
                200,
                itunes.as_bytes().to_vec(),
            )
            .on("https://x/1200x1200bb.jpg", 200, png(600));
        let row = process_album(&http, &throttle(), &fx.cache, None, &candidate, NOW).unwrap();
        assert_eq!(row.status, LookupStatus::Unmatched);
        assert!(!write::cache_path(&fx.cache, &candidate.album_key).exists());
        assert_eq!(http.requests().len(), 1, "only MusicBrainz was asked");
    }

    #[test]
    fn complete_tags_without_cover_fetch_cover_by_name_when_musicbrainz_has_nothing() {
        let fx = Fixture::complete("Estranhos no Ninho", "Exxótica");
        let candidate = fx.candidate();
        assert_eq!(candidate.tags_missing, 0);
        let itunes = r#"{"results":[{"artistName":"Exxótica","collectionName":"Estranhos no Ninho","artworkUrl100":"https://x/100x100bb.jpg"}]}"#;
        let http = FakeHttp::new()
            .on(
                "https://musicbrainz.org/ws/2/release/?",
                200,
                br#"{"releases":[]}"#.to_vec(),
            )
            .on(
                "https://itunes.apple.com/search",
                200,
                itunes.as_bytes().to_vec(),
            )
            .on("https://x/1200x1200bb.jpg", 200, png(600));
        let row = process_album(&http, &throttle(), &fx.cache, None, &candidate, NOW).unwrap();
        assert_eq!(
            row.status,
            LookupStatus::Unmatched,
            "no MusicBrainz identity"
        );
        assert!(
            write::cache_path(&fx.cache, &candidate.album_key).exists(),
            "but the cover is cached"
        );
        assert!(
            fx.tracks()[0].artwork.is_none(),
            "a name-only cover is never written into files"
        );
        assert_eq!(fx.tracks()[0].title, "Song 1");
    }
}
