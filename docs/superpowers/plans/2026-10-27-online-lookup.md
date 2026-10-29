# Online Lookup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An opt-in Settings switch that makes omamusic identify albums on MusicBrainz, fetch a front cover, and write the cover plus any empty tags into the audio files, with unmatched albums shown in the UI.

**Architecture:** A new `lookup` module owns everything network-related behind an `Http` trait: MusicBrainz search and scoring, cover sources, and tag writing through lofty. A worker thread owned by `Library` wakes after each scan, processes one album at a time with its own SQLite writer, records a `lookup` row per album, and reports progress through `LibrarySnapshot`. The scanner picks up the rewritten files through the existing mtime check, so covers and tags flow into the UI through code that already exists.

**Tech Stack:** Rust 2021, gpui-kit 0.7.1, rusqlite (bundled), lofty 0.22 (read and write), `ureq` 3 with `rustls` + `webpki-roots` + `gzip` (new), serde_json, image, sha2 (new direct dep, already in tree).

**Spec:** `docs/superpowers/specs/2026-10-27-online-lookup-design.md`

## Global Constraints

- Linux only. No new C dependencies. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` must pass at the end of every task.
- No test opens a socket. `ureq` is constructed only in `Library::new`. Tests use `FakeHttp`.
- Setting is `online_lookup`, default `false`. Nothing in `lookup` runs while it is off.
- MusicBrainz: ≥ 1000 ms between requests, `User-Agent: omamusic/<CARGO_PKG_VERSION> (+https://github.com/omafellas/omamusic)`, timeouts 10 s.
- Files: never overwrite an existing tag value or an existing picture. Write through temp file + rename in the same directory. Read-only → skip file, no UI error.
- Accept threshold: distance ≤ 0.15 and (durations matched for every track within 2 s, or an ID came from tags).
- Images: must decode, short side ≥ 300 px, aspect within 10 % of square.
- Retry window: 30 days (`2_592_000` s) for `unmatched`, `error`, and `matched`-without-cover.
- Repo rules: no code comments except documented workarounds; no AI attribution anywhere; every user-visible change gets a smoke test in `tests/ui.rs`; unit tests only for pure logic.

## Review Focus

1. Album tag fell back to folder name and the folder is "Music" or "Downloads": the search query would be nonsense. Expect: skip the MusicBrainz search when `tags_missing` has the album bit set and there is no artist either; go straight to `unmatched`. Test in Task 8.
2. Various Artists compilations: artist similarity must not punish; track artists differ per track. Expect: artist component skipped, titles and durations decide. Test in Task 5.
3. A 2-track single matched against a 20-track album with the same title: track-count penalty must reject. Test in Task 5.
4. Cover Art Archive answers 307 to archive.org and sometimes 404 for `release` but 200 for `release-group`. Expect: redirects followed, fallback chain continues. Test in Task 6 (fake returns 404 then 200).
5. The worker writes a file while the scanner is mid-reconcile of the same folder. Expect: no DB conflict (busy timeout) and the next reconcile picks up the new mtime. Test in Task 8 (worker test runs `reconcile` after the worker and asserts tags are in the DB).

---

### Task 1: Setting and switch

**Files:**
- Modify: `src/config.rs` (struct, Default, tests)
- Modify: `src/app.rs:1893-1920` (settings switches)
- Test: `tests/ui.rs`

**Interfaces:**
- Produces: `AppConfig.online_lookup: bool`; element id `pref-online-lookup`.

- [ ] **Step 1: Write the failing smoke test** in `tests/ui.rs`

```rust
#[gpui_kit::test]
async fn online_lookup_switch_persists_to_config(cx: &mut TestAppContext) {
    let mut app = open(cx);
    app.click("settings");
    app.click("pref-online-lookup");
    let text = std::fs::read_to_string(app.config_dir.join("config.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["online_lookup"], serde_json::Value::Bool(true));
    app.click("pref-online-lookup");
    let text = std::fs::read_to_string(app.config_dir.join("config.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["online_lookup"], serde_json::Value::Bool(false));
}
```

- [ ] **Step 2: Run** `cargo test --test ui online_lookup_switch` — expect FAIL (element `pref-online-lookup` not found).

- [ ] **Step 3: Add the field.** `pub online_lookup: bool` on `AppConfig`, `false` in `Default`. Unit test `online_lookup_defaults_off_and_round_trips` in `config.rs` tests: `{}` → false, `{"online_lookup": true}` → true.

- [ ] **Step 4: Add the switch row** after `pref-visual-effects` in `render_settings`: `self.pref_switch("pref-online-lookup", "Complete missing tags and covers online", self.prefs.online_lookup, |c, on| c.online_lookup = on, cx)` followed by a `div().text_size(rems(0.6875)).text_color(theme.secondary).px(rems(0.3125))` with the text "Sends album and artist names to MusicBrainz, Apple and Deezer. Writes covers and missing tags into your music files."

- [ ] **Step 5: Run** `cargo test --test ui online_lookup_switch` and `cargo test --lib config` — expect PASS.

- [ ] **Step 6: Commit** `feat: add online lookup setting`

---

### Task 2: Schema, MusicBrainz IDs and missing-tag bits

**Files:**
- Modify: `src/metadata.rs` (`AudioMetadata`, `read_tagged`, tests)
- Modify: `src/db.rs` (`SCHEMA_VERSION`, `SCHEMA`, `DROP`, `album_summary_from_row`, album queries, new functions, tests)
- Modify: `src/index.rs` (`REBUILD_ALBUMS`, `reconcile` insert, `update_artwork`, tests)
- Modify: `src/models.rs` (`AlbumSummary.unmatched`)

**Interfaces:**
- Produces in `metadata`: `AudioMetadata { mb_release_group: String, mb_release: String, mb_recording: String }`.
- Produces in `db`:
  - `pub const SCHEMA_VERSION: i64 = 3`
  - `pub struct LookupRow { pub album_key: String, pub status: LookupStatus, pub release: Option<String>, pub release_group: Option<String>, pub score: Option<f64>, pub candidate: Option<String>, pub attempted_at: i64 }`
  - `pub enum LookupStatus { Matched, Unmatched, Error }` with `as_str()` / `from_str` mapping to `matched` / `unmatched` / `error`.
  - `pub struct LookupCandidate { pub album_key: String, pub title: String, pub artist: String, pub year: u32, pub tags_missing: u32, pub mb_release_group: String, pub needs_cover: bool, pub tracks: Vec<Track> }` where `Track` gains nothing; durations come from `Track.duration`.
  - `pub fn albums_needing_lookup(conn, now: i64, retry_after: i64) -> Result<Vec<LookupCandidate>>` — albums where `artwork.data IS NULL` or `albums.tags_missing != 0`, excluding keys with a `lookup` row where `attempted_at > now - retry_after` unless status is matched and cover now exists; ordered by `lookup.attempted_at` NULLS FIRST, then `ALBUM_ORDER`.
  - `pub fn record_lookup(conn, row: &LookupRow) -> Result<()>` (upsert).
  - `pub fn unmatched_albums(conn) -> Result<Vec<AlbumSummary>>` (status `unmatched`, `ALBUM_ORDER`).
  - `pub fn clear_unmatched(conn) -> Result<usize>`.
  - `pub fn track_mb_ids(conn, album_key) -> Result<Vec<(String, String, String, String)>>` is not needed; `LookupCandidate.tracks` plus `tracks.mb_recording` read in `albums_needing_lookup` instead: add `pub mb_recording: String` to `Track`? No: keep `Track` unchanged; `LookupCandidate` carries `pub recordings: Vec<String>` parallel to `tracks`.
- Produces in `index`: `tags_missing` bits `TITLE = 1, ARTIST = 2, ALBUM = 4, YEAR = 8` as `pub const` in `index`; `update_artwork` third source `lookup::cache_path(key)` (defined in Task 7; until then a local `fn cache_cover(key) -> Option<Vec<u8>>` reading `dirs::cache_dir()/omamusic/covers/<sha256 hex>.jpg`).
- Produces in `models`: `AlbumSummary.unmatched: bool` (`#[serde(default)]`).

- [ ] **Step 1: Failing tests**

`metadata.rs` — extend `reads_tags_and_cover_written_by_a_tagger`: insert `ItemKey::MusicBrainzReleaseGroupId = "rg-1"`, `MusicBrainzReleaseId = "rel-1"`, `MusicBrainzRecordingId = "rec-1"`; assert the three fields.

`db.rs`:
```rust
#[test]
fn lookup_rows_round_trip_and_mark_summaries() {
    // insert one album via tracks + REBUILD_ALBUMS; record_lookup(Unmatched)
    // assert unmatched_albums().len() == 1 and albums_page(..)[0].unmatched
    // record_lookup(Matched) → unmatched_albums() empty; clear_unmatched() == 0
}
#[test]
fn albums_needing_lookup_respects_retry_window() {
    // album with no artwork row: returned
    // after record_lookup(Unmatched, attempted_at = now): not returned
    // with attempted_at = now - 31 days: returned
    // after inserting artwork data and tags_missing 0: not returned
}
```

`index.rs`:
```rust
#[test]
fn missing_tags_are_recorded_as_bits() {
    // untagged "song.mp3" (b"a") → tracks.tags_missing == TITLE|ARTIST|ALBUM|YEAR, albums.tags_missing same
}
#[test]
fn cached_cover_is_used_when_file_and_folder_have_none() {
    // set OMAMUSIC_CACHE_DIR? No: index takes cache dir from a `pub fn set_cache_dir` test hook? Keep simple:
    // `update_artwork` takes `cache_dir: &Path` param threaded from `reconcile(conn, folders, cache_dir)`.
    // write <cache_dir>/covers/<hash>.jpg = b"cached"; reconcile; db::artwork == Some(b"cached")
}
```

- [ ] **Step 2: Run** `cargo test --lib db::tests::lookup db::tests::albums_needing index::tests::missing index::tests::cached metadata` — expect FAIL to compile.

- [ ] **Step 3: Implement.** Schema v3 adds to `tracks`: `tags_missing INTEGER NOT NULL DEFAULT 0, mb_release_group TEXT NOT NULL DEFAULT '', mb_release TEXT NOT NULL DEFAULT '', mb_recording TEXT NOT NULL DEFAULT ''`; to `albums`: `tags_missing INTEGER NOT NULL DEFAULT 0, mb_release_group TEXT NOT NULL DEFAULT ''`; new `lookup` table as in spec; `DROP` drops it. `REBUILD_ALBUMS` adds `MAX(tags_missing)` (bitwise OR is fine via `MAX` only if all tracks agree; use a correlated subquery `(SELECT COALESCE(SUM(DISTINCT ...)))`? No — SQLite has no bit-or aggregate; use `MAX(tags_missing)`; builder's call, documented in spec as "any track missing" being good enough) and `MAX(mb_release_group)`. `reconcile` computes the bits where it applies fallbacks and inserts the three IDs. `reconcile` signature becomes `reconcile(conn, folders, cache_dir: &Path)`; `Library` passes `dirs::cache_dir().join("omamusic")`. Album SELECTs add `EXISTS(SELECT 1 FROM lookup l WHERE l.album_key = a.key AND l.status = 'unmatched') AS unmatched`.

- [ ] **Step 4: Run** the tests above — expect PASS. Run `cargo test` — all pass (existing index tests updated for the new `reconcile` parameter).

- [ ] **Step 5: Commit** `feat: track MusicBrainz ids, missing tags and lookup state in the index`

---

### Task 3: HTTP boundary

**Files:**
- Create: `src/lookup/mod.rs`, `src/lookup/http.rs`
- Modify: `Cargo.toml`, `src/lib.rs` (`pub mod lookup;`)

**Interfaces:**
- Produces:
  ```rust
  pub struct Response { pub status: u16, pub body: Vec<u8> }
  pub trait Http: Send + Sync + 'static {
      fn get(&self, url: &str) -> anyhow::Result<Response>;
  }
  pub struct UreqHttp { .. }      // new() builds Agent: timeout_global 10 s, user_agent USER_AGENT, follows redirects (ureq default 10), gzip
  pub const USER_AGENT: &str      // built with concat!/format at init: "omamusic/<version> (+https://github.com/omafellas/omamusic)"
  pub struct Throttle { .. }      // new(min_gap: Duration); fn wait(&self) blocks so successive calls are ≥ min_gap apart
  #[cfg(any(test, feature = "test-support"))]
  pub struct FakeHttp { .. }      // new(); on(prefix: &str, status: u16, body: impl Into<Vec<u8>>) -> Self (builder); fn requests(&self) -> Vec<String>
  ```
  `FakeHttp::get` matches the longest registered prefix; unmatched URL → `Ok(Response { status: 404, body: vec![] })`. Status 3xx is never produced by the fake; redirects are the real client's business.

- [ ] **Step 1: Add deps** to `Cargo.toml`: `ureq = { version = "3", default-features = false, features = ["rustls", "gzip"] }`, `sha2 = "0.10"`.

- [ ] **Step 2: Failing tests** in `http.rs`:
```rust
#[test] fn fake_http_matches_longest_prefix_and_records_requests()
#[test] fn throttle_spaces_calls()   // two wait() calls with min_gap 50 ms take ≥ 50 ms total
#[test] fn user_agent_names_version_and_contact()  // contains env!("CARGO_PKG_VERSION") and "github.com/omafellas/omamusic"
```

- [ ] **Step 3: Run** `cargo test --lib lookup::http` — expect FAIL.
- [ ] **Step 4: Implement** `http.rs`. `UreqHttp::get` reads status, `body_mut().read_to_vec()`; non-2xx is still `Ok(Response)` (callers decide), transport errors are `Err`.
- [ ] **Step 5: Run** `cargo test --lib lookup::http` — PASS. `cargo clippy --all-targets -- -D warnings` clean.
- [ ] **Step 6: Commit** `feat: add http boundary for online lookup`

---

### Task 4: Text normalization and MusicBrainz client

**Files:**
- Create: `src/lookup/text.rs`, `src/lookup/musicbrainz.rs`

**Interfaces:**
- `text.rs`:
  - `pub fn normalize(s: &str) -> String` — lowercase, strip bracketed suffixes `(...)`/`[...]`, strip leading "the ", drop punctuation, collapse whitespace, strip trailing "deluxe edition|remastered|remaster|bonus tracks|edition".
  - `pub fn similarity(a: &str, b: &str) -> f64` — `1 - levenshtein(normalize a, normalize b) / max_len`, 1.0 for two empties.
- `musicbrainz.rs`:
  ```rust
  pub struct Release { pub id: String, pub release_group: String, pub title: String, pub artist: String, pub year: u32, pub tracks: Vec<ReleaseTrack> }
  pub struct ReleaseTrack { pub number: u32, pub title: String, pub artist: String, pub recording: String, pub length_secs: Option<f64> }
  pub fn search_url(album: &str, artist: Option<&str>, track_count: usize) -> String
  pub fn release_url(id: &str) -> String      // .../ws/2/release/<id>?fmt=json&inc=recordings+artist-credits+release-groups
  pub fn parse_search(body: &[u8]) -> anyhow::Result<Vec<String>>   // release ids, ≤ 5
  pub fn parse_release(body: &[u8]) -> anyhow::Result<Release>      // flattens media[].tracks[]
  pub fn lucene_escape(s: &str) -> String
  ```
  `search_url` format: `https://musicbrainz.org/ws/2/release/?fmt=json&limit=5&query=` + percent-encoded `release:"<esc>"` [`+AND+artist:"<esc>"` when `artist` is `Some` and `normalize(artist) != "various artists"`] `+AND+tracks:<n>`. Year from `date` first 4 digits, `release-group.first-release-date` as fallback.

- [ ] **Step 1: Failing tests**
```rust
// text.rs
#[test] fn normalize_strips_articles_brackets_and_editions()  // "The Wall (Deluxe Edition)" → "wall"; "In Rainbows [Disc 1]" → "in rainbows"
#[test] fn similarity_is_one_for_equal_and_low_for_unrelated()  // ("OK Computer","Ok Computer!") == 1.0; ("OK Computer","Kid A") < 0.4
// musicbrainz.rs
#[test] fn search_url_quotes_escapes_and_skips_various_artists()
#[test] fn parse_search_returns_at_most_five_ids()   // inline JSON with 6 releases
#[test] fn parse_release_flattens_media_and_reads_lengths()   // inline JSON: 2 media, 3 tracks, "length": 243000 → 243.0, missing length → None, artist-credit joined with joinphrase
```
- [ ] **Step 2: Run** `cargo test --lib lookup::text lookup::musicbrainz` — FAIL.
- [ ] **Step 3: Implement.** Levenshtein in-house over `chars()`. JSON via `serde_json::Value` navigation (no typed structs needed).
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat: add MusicBrainz query building and parsing`

---

### Task 5: Scoring

**Files:**
- Create: `src/lookup/score.rs`

**Interfaces:**
```rust
pub struct LocalAlbum { pub title: String, pub title_from_folder: bool, pub artist: Option<String>, pub year: u32, pub tracks: Vec<LocalTrack>, pub has_ids: bool }
pub struct LocalTrack { pub number: u32, pub title: Option<String>, pub duration: f64 }   // title None when it came from the file name; duration 0.0 when unknown
pub struct Match { pub distance: f64, pub pairing: Vec<Option<usize>>, pub durations_matched: bool }   // pairing[i] = index into release.tracks
pub fn distance(local: &LocalAlbum, release: &Release) -> Match
pub fn accept(m: &Match, local: &LocalAlbum) -> bool   // distance ≤ 0.15 && (durations_matched || local.has_ids)
pub const ACCEPT_THRESHOLD: f64 = 0.15;
pub const DURATION_TOLERANCE_SECS: f64 = 2.0;
```
Distance = Σ weight·penalty / Σ weight over the components that applied: album 3·(1-sim), artist 3·(1-sim) when `artist` is Some and not Various Artists, track count 2·min(1, |diff|/3) (weight counted once), per track title 1·(1-sim) when `title` is Some, per track duration 2·min(1, max(0, |diff|-2)/30) when both known, year 1·min(1,|diff|/5) when both > 0. Pairing: by `number` when local numbers are unique and all > 0 and ≤ release track count, else greedy by smallest |duration diff| over remaining release tracks. `durations_matched` is true when every local track has duration > 0, is paired, and the paired diff ≤ 2 s.

- [ ] **Step 1: Failing tests** with hand-built `Release` values:
```rust
#[test] fn identical_album_scores_near_zero_and_is_accepted()
#[test] fn durations_alone_identify_an_untagged_folder()   // titles None, numbers 0, durations within 1 s → accept
#[test] fn single_vs_full_album_is_rejected()   // 2 local tracks vs 20 release tracks, same title → distance > 0.15
#[test] fn various_artists_skips_artist_component()   // artist Some("Various Artists"), per-track artists differ → accepted on titles+durations
#[test] fn weak_title_only_match_without_durations_is_not_accepted()   // durations 0.0, has_ids false, titles match → accept false
#[test] fn ids_from_tags_accept_without_durations()
```
- [ ] **Step 2: Run** `cargo test --lib lookup::score` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat: score MusicBrainz candidates against local albums`

---

### Task 6: Cover sources and image validation

**Files:**
- Create: `src/lookup/covers.rs`

**Interfaces:**
```rust
pub fn caa_release_url(release: &str) -> String        // https://coverartarchive.org/release/<id>/front-500
pub fn caa_release_group_url(rg: &str) -> String
pub fn itunes_search_url(artist: &str, album: &str) -> String   // https://itunes.apple.com/search?entity=album&limit=5&term=<pct(artist album)>
pub fn itunes_pick(body: &[u8], artist: &str, album: &str) -> Option<String>   // artworkUrl100 → 1200x1200bb, both sims ≥ 0.9
pub fn deezer_search_url(artist: &str, album: &str) -> String   // https://api.deezer.com/search/album?q=<pct(artist:"a" album:"b")>
pub fn deezer_pick(body: &[u8], artist: &str, album: &str) -> Option<String>   // cover_xl, both sims ≥ 0.9
pub fn validate_image(bytes: &[u8]) -> Option<Vec<u8>>   // decodes, short side ≥ 300, |w-h|/max ≤ 0.10 → returns bytes re-encoded as JPEG quality 90 if input is not JPEG, else original
pub fn fetch_cover(http: &dyn Http, throttle: &Throttle, release: Option<&str>, release_group: Option<&str>, artist: &str, album: &str) -> Option<Vec<u8>>
```
`fetch_cover` order: CAA release (if Some), CAA release group (if Some), iTunes, Deezer; each `status == 200` body goes through `validate_image`; first `Some` wins. CAA requests pass through `throttle`.

- [ ] **Step 1: Failing tests**
```rust
#[test] fn itunes_pick_rewrites_size_and_requires_close_match()
#[test] fn deezer_pick_reads_cover_xl()
#[test] fn validate_image_rejects_small_and_non_square()   // 100x100 PNG → None; 400x200 → None; 400x400 → Some
#[test] fn fetch_cover_falls_through_sources()   // FakeHttp: CAA release 404, CAA release-group 200 with a 400x400 PNG → returns it; requests() shows both URLs in order
#[test] fn fetch_cover_uses_itunes_when_caa_has_nothing()
```
Build PNGs in tests with `image::RgbaImage::new(w, h)` written to a buffer.
- [ ] **Step 2: Run** `cargo test --lib lookup::covers` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat: fetch covers from Cover Art Archive, iTunes and Deezer`

---

### Task 7: Writing tags and covers into files

**Files:**
- Create: `src/lookup/write.rs`
- Modify: `src/index.rs` (replace the Task 2 local `cache_cover` with `lookup::write::cache_path`)

**Interfaces:**
```rust
pub struct TrackFill<'a> { pub title: Option<&'a str>, pub artist: Option<&'a str>, pub album: Option<&'a str>, pub year: Option<u32>, pub track_number: Option<u32>, pub release_group: Option<&'a str>, pub release: Option<&'a str>, pub recording: Option<&'a str> }
pub enum WriteOutcome { Written, Unchanged, ReadOnly }
pub fn fill_file(path: &Path, fill: &TrackFill, cover: Option<&[u8]>) -> anyhow::Result<WriteOutcome>
pub fn cache_path(cache_dir: &Path, album_key: &str) -> PathBuf   // <cache_dir>/covers/<sha256 hex of album_key>.jpg
pub fn save_cache_cover(cache_dir: &Path, album_key: &str, bytes: &[u8]) -> anyhow::Result<PathBuf>
```
`fill_file`: open with lofty (`ParsingMode::Relaxed`); take `primary_tag_mut()` or insert a tag of `primary_tag_type()`; for each `Some` field whose current value is empty/absent, set it; if `cover` is Some and `pictures()` is empty, push `PictureType::CoverFront` with MIME sniffed from bytes (`image::guess_format`); if nothing changed → `Unchanged` without touching the file. Otherwise copy to `<path>.omamusic-tmp`, `save_to_path(tmp)`, `fsync`, copy permissions from original, `rename(tmp, path)`. Permission errors (`ErrorKind::PermissionDenied`, `ReadOnlyFilesystem`) → `ReadOnly`, tmp removed.

- [ ] **Step 1: Failing tests** (temp dir, WAV fixture from `metadata.rs` tests — move `wav_bytes()` to a `#[cfg(test)] pub(crate) mod fixtures` in `metadata.rs`):
```rust
#[test] fn fills_only_empty_fields_and_adds_cover_once()   // pre-set title "Keep"; fill title "New", artist "A", cover → metadata::read: title "Keep", artist "A", artwork Some; second call → Unchanged
#[test] fn read_only_directory_is_skipped()   // chmod dir 0o555 → ReadOnly, file bytes unchanged; restore perms in Drop
#[test] fn cache_path_is_stable_and_save_writes_it()
```
- [ ] **Step 2: Run** `cargo test --lib lookup::write` — FAIL.
- [ ] **Step 3: Implement.** Wire `index::update_artwork` to `lookup::write::cache_path`.
- [ ] **Step 4: Run** — PASS; `cargo test --lib index` PASS.
- [ ] **Step 5: Commit** `feat: write fetched covers and missing tags into audio files`

---

### Task 8: Worker and Library wiring

**Files:**
- Create: `src/lookup/worker.rs`
- Modify: `src/library.rs` (`LibrarySnapshot`, `Library::new`, scanner loop, `empty`, `open_for_test`), `src/lookup/mod.rs`

**Interfaces:**
- `LibrarySnapshot` gains `pub lookup_pending: usize, pub lookup_total: usize, pub lookup_unmatched: Vec<AlbumSummary>`.
- `Library` gains `pub fn set_online_lookup(&self, on: bool)` (writes config, wakes worker) and `pub fn retry_unmatched(&self)` (clears rows, wakes worker).
- `worker.rs`:
  ```rust
  pub struct Worker { .. }
  pub struct WorkerDeps { pub http: Arc<dyn Http>, pub db_path: PathBuf, pub cache_dir: PathBuf, pub covers: ArtworkCache, pub state: SharedSnapshot (made pub(crate) in library) }
  pub enum Wake { ScanFinished, SettingChanged(bool), Retry }
  impl Worker { pub fn spawn(deps: WorkerDeps, enabled: bool) -> Sender<Wake>; }
  pub fn process_album(http: &dyn Http, throttle: &Throttle, conn: &Connection, cache_dir: &Path, candidate: &LookupCandidate, now: i64) -> anyhow::Result<LookupRow>   // pure orchestration, used by the thread and by tests
  ```
  `process_album`: build `LocalAlbum` from the candidate (`title_from_folder = tags_missing & ALBUM != 0`, `artist = None` when `tags_missing & ARTIST != 0`, `has_ids = !mb_release_group.is_empty()`). If `title_from_folder && artist.is_none()` → `Unmatched` with `candidate: None`, no request. Else: ids → `release_url` directly; otherwise `search_url` → `parse_search` → `release_url` for each id (throttled). Score all, keep best. If `accept`: `fetch_cover` (only when `needs_cover`), `save_cache_cover`, then `fill_file` per track with the paired release track (fields only for bits set in `tags_missing`, plus the three ids), return `Matched`. Else `Unmatched` with `candidate = "<artist> – <title> (<year>)"` of the best and `score`. Any `Err` from http or JSON → `Error`.
  Thread loop: wait on channel; `SettingChanged(on)` stores flag; when flag on, open writer, loop `albums_needing_lookup(now, 2_592_000)`, set `lookup_total/pending`, process each, `record_lookup`, `covers.remove(key)`, refresh `lookup_unmatched` from `unmatched_albums`, stop when channel says `SettingChanged(false)` (checked between albums via `try_recv`). On 503 from MusicBrainz: sleep 5 s, retry once, then 30 s, then abandon the run.

- [ ] **Step 1: Failing tests** in `worker.rs` (in-memory DB file in temp dir, `FakeHttp`, three WAV files with empty tags and durations 1.0 s each — vary by writing different sample counts so durations are 1.0, 2.0, 3.0 s):
```rust
#[test] fn matched_album_gets_tags_cover_and_cache_file()
    // fake: search → ids ["r1"], release r1 with 3 tracks lengths 1000/2000/3000, CAA release 200 PNG 400x400
    // after process_album: status Matched; metadata::read on each file has title/artist/album/year + artwork; cache_path exists
    // then index::reconcile → db::artwork(key) Some, tracks.tags_missing == 0
#[test] fn no_candidates_marks_unmatched_without_touching_files()
#[test] fn folder_named_album_without_artist_makes_no_request()   // http.requests().is_empty()
#[test] fn read_only_files_still_match_with_cache_cover()
#[test] fn transport_error_is_recorded_as_error()   // Http impl returning Err
```
- [ ] **Step 2: Run** `cargo test --lib lookup::worker` — FAIL.
- [ ] **Step 3: Implement** `worker.rs`; wire in `Library::new`: `Worker::spawn(WorkerDeps { http: Arc::new(UreqHttp::new()), .. }, AppConfig::load().online_lookup)`; scanner loop sends `Wake::ScanFinished` after each reconcile; `Library::empty` and `open_for_test` keep `None` sender (no thread).
- [ ] **Step 4: Run** `cargo test` — PASS.
- [ ] **Step 5: Commit** `feat: run online lookup in the background after each scan`

---

### Task 9: Badge, Settings status and retry

**Files:**
- Modify: `src/app.rs` (`render_album_card` near line 1340, album header near 1584, `render_settings`, `pref_switch` call for `pref-online-lookup` to call `library.set_online_lookup`)
- Test: `tests/ui.rs`, `tests/rendering.rs`
- Modify: `src/library.rs` (`#[cfg(any(test, feature = "test-support"))] pub fn with_unmatched(self, albums: Vec<AlbumSummary>) -> Self` seeding `lookup_unmatched`, and a test-support `pub fn insert_unmatched_album(&self, summary: &AlbumSummary)` that inserts the album row and lookup row into the in-memory DB so `album_page` returns it with `unmatched: true`)

**Interfaces:**
- Element ids: `album-unmatched-<key>` badge (a `div().id(..).test_support()` wrapping `icon(IconName::CircleAlert)` with `with_tooltip(.., "Could not be identified online")`), `lookup-status` text, `lookup-retry` button.

- [ ] **Step 1: Failing smoke tests**
```rust
#[gpui_kit::test]
async fn unmatched_album_shows_badge_and_settings_count(cx) {
    // library = Library::empty(); library.insert_unmatched_album(&AlbumSummary{ id:"k", title:"T", artist:"A", .. })
    // open; try_find("album-unmatched-k").is_some()
    // click("settings"); label("lookup-status") == Some("1 album could not be identified")
    // click("lookup-retry"); render_frame; try_find("lookup-status").is_none(); try_find("album-unmatched-k").is_none()
}
#[gpui_kit::test]
async fn lookup_progress_is_shown_while_working(cx) {
    // library.with_lookup_progress(3, 12) test-support setter; settings label("lookup-status") == "Looking up albums… 3 of 12"
}
```
`open()` must accept a prebuilt `Library`: add `fn open_with(cx, library: Arc<Library>) -> Smoke`.

Pixel test in `tests/rendering.rs`: `unmatched_badge_paints_ink` — bounds of `album-unmatched-k` contain ink; same view without the row has no element.

- [ ] **Step 2: Run** `cargo test --test ui unmatched lookup_progress` — FAIL.
- [ ] **Step 3: Implement.** Badge: absolute-positioned `div` top-right over the cover, `size(rems(1.))`, `rounded_full()`, `bg(theme.background)`, icon `text_color(theme.warning)` or `theme.secondary` if no warning color exists. Settings block under the helper text: when `lookup_total > 0 && lookup_pending > 0` → `div().id("lookup-status").test_support().child(format!("Looking up albums… {done} of {total}"))` where `done = total - pending`; else when `!lookup_unmatched.is_empty()` → status line "N album(s) could not be identified", a `div().max_h(rems(8.)).overflow_y_scroll()` listing "Artist – Title" rows, and `button("lookup-retry", "Retry all", ButtonVariant::Secondary, cx)` calling `library.retry_unmatched()`.
- [ ] **Step 4: Run** `cargo test --test ui` and `cargo test --test rendering` — PASS.
- [ ] **Step 5: Commit** `feat: show albums that could not be identified online`

---

### Task 10: Docs and packaging

**Files:**
- Modify: `README.md` (Features bullet, Flatpak permissions table and "No network access" paragraph)
- Modify: `flatpak/io.github.omafellas.omamusic.json` (`--share=network`)

- [ ] **Step 1: README.** Features: add "Optional online lookup (off by default): identifies albums on MusicBrainz, fetches missing covers from Cover Art Archive, iTunes or Deezer, and writes covers and missing tags into your files. Sends only album title, artist name and track count." Flatpak paragraph: "Network access is used only when the online lookup setting is on. The flatpak keeps the music folder read-only, so fetched covers stay in the cache and tags are not written inside the sandbox." Table row: `--share=network` | optional online lookup.
- [ ] **Step 2: Manifest.** Add `"--share=network"` to `finish-args`.
- [ ] **Step 3: Final gate.** `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`.
- [ ] **Step 4: Commit** `docs: describe online lookup and its network use`

---

## Self-review notes

- Spec coverage: setting (T1), needs-work rule and MBIDs (T2), http and throttle (T3), MusicBrainz (T4), scoring and accept rule (T5), covers and validation (T6), file writes and cache (T7), worker, progress, failure behavior (T8), badge, list, retry (T9), privacy docs and flatpak (T10). Manual picker and fingerprinting are out of scope by spec.
- Names used across tasks: `Http`, `Response`, `FakeHttp`, `Throttle` (T3→T6,T8); `Release`, `ReleaseTrack` (T4→T5,T8); `LocalAlbum`, `LocalTrack`, `Match`, `distance`, `accept` (T5→T8); `fetch_cover` (T6→T8); `fill_file`, `TrackFill`, `WriteOutcome`, `cache_path`, `save_cache_cover` (T7→T2,T8); `LookupCandidate`, `LookupRow`, `LookupStatus`, `albums_needing_lookup`, `record_lookup`, `unmatched_albums`, `clear_unmatched` (T2→T8,T9); `lookup_pending`, `lookup_total`, `lookup_unmatched`, `set_online_lookup`, `retry_unmatched` (T8→T9).
- `reconcile` gains a `cache_dir` parameter in T2; T7 only swaps the helper. T2's local helper must hash with sha2 so the path never changes between tasks.
