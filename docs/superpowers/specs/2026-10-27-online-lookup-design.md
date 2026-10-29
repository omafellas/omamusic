# Online lookup: missing covers and tags

Date: 2026-10-27. Status: approved for planning (owner opted out of further questions; calls marked "builder's call" were made by the author and are open to review).

## Intent

Albums in the library sometimes have no cover art, and tracks sometimes have empty title, artist, album or year tags. Today omamusic shows initials instead of a cover and falls back to file and folder names. The owner wants omamusic to repair this on its own, the way `beets` does with its `fetchart`, `embedart` and autotagger, while staying a single static Linux binary with no helper programs.

## Goals

- One Settings switch, off by default, that lets omamusic look albums up online and repair them.
- For each album that needs it: identify the release on MusicBrainz, fetch a front cover, write the cover and every missing tag into the audio files themselves, so any other player benefits and omamusic needs no special bookkeeping afterwards.
- Never ask the user anything while running. Doubt means skip.
- Make skipped albums visible: a badge on the album and a list in Settings.
- Everything native in the binary: pure-Rust HTTP, TLS and tag writing. No `curl`, no Python, no `fpcalc`.

## Non-goals (this version)

- Correcting tags that already have a value. Only empty fields are filled.
- Manual picker ("this album is actually X"). The data model must allow it later; the UI is not built now.
- Acoustic fingerprinting (Chromaprint + AcoustID). Phase 2, see end.
- Renaming or moving files. Lyrics, genres, album artist, disc numbers.
- Any network traffic while the switch is off, including in tests.

## Anti-goals

- Writing a wrong cover or wrong title into a user's file. A false positive is worse than ten misses.
- Corrupting an audio file through a partial write.
- Hammering MusicBrainz. Their policy: one request per second, descriptive `User-Agent` with a contact URL.
- Quietly changing the project's "fully local" promise. README and the flatpak manifest must say what changed.

## Constraints and facts about the codebase

- Linux only. Flatpak build mounts `xdg-music:ro`; file writes fail there and must degrade to cache-only without errors in the UI.
- `src/metadata.rs` now reads tags through `lofty` 0.22 (pure Rust, reads and writes ID3v2, MP4 `ilst`, FLAC, Vorbis comments, RIFF, AIFF). Writing goes through the same crate.
- `src/index.rs::reconcile` is the scanner. `update_artwork` already resolves cover art as embedded picture, then folder image, and already lists albums whose `artwork.data IS NULL`. `album_key` is `dir + "\n" + album title`.
- SQLite schema is disposable: bumping `SCHEMA_VERSION` drops and rebuilds. Anything that must survive a rebuild lives in files, not the database.
- `ArtworkCache` (`src/artwork.rs`) is invalidated per album key via `remove`, which the scanner already calls for `changed_albums`.
- Settings panel in `src/app.rs::render_settings` uses `pref_switch` rows writing `AppConfig`. Smoke tests in `tests/ui.rs` drive the real view; every user-visible change needs one.
- Measured: `ureq 3` + `rustls` + `webpki-roots` adds about 1.55 MB to the 23.8 MB release binary. `lofty` is already in.
- Dependency tree already has `serde_json`, `url` (percent encoding), `image` (validation and decode), `sha2`.

## Design

### Setting

`AppConfig.online_lookup: bool`, default `false`, serialized as `online_lookup`. One switch in Settings, id `pref-online-lookup`, label "Complete missing tags and covers online". Helper line beneath it: "Sends album and artist names to MusicBrainz, Apple and Deezer. Writes covers and missing tags into your music files." (builder's call on wording).

Toggling on starts the worker immediately. Toggling off stops it after the album in flight finishes. No other switches in this version: cache-only mode happens automatically when a file cannot be written.

### What counts as "needs work"

An album needs work when any of these hold:

- `artwork.data IS NULL` after the scanner's own resolution (no embedded picture, no folder image, no cached fetch), or
- any of its tracks has an empty title or artist tag, an album tag that fell back to the folder name, or year 0,

and there is no `lookup` row for its key newer than 30 days with status `unmatched` or `error`. A `matched` row older than 30 days with still-missing cover is retried too (Cover Art Archive gains images over time).

The scanner must therefore record, per track, whether title, artist, album and year came from tags or from fallbacks. Builder's call: add `tags_missing INTEGER` bitmask column to `tracks` (bit 0 title, 1 artist, 2 album, 3 year) set in `reconcile`, and roll `MAX(tags_missing)` into `albums`.

### Reading MusicBrainz IDs

Extend `AudioMetadata` and `tracks` with `mb_release_group`, `mb_release`, `mb_recording` (TEXT, empty when absent), read through lofty `ItemKey::MusicBrainzReleaseGroupId`, `MusicBrainzReleaseId`, `MusicBrainzRecordingId`. Picard- and beets-tagged libraries then skip search entirely.

### Identification

Module `src/lookup/` (builder's call on layout):

- `http.rs`: `trait Http { fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response> }` with `Response { status: u16, body: Vec<u8> }`. Production impl wraps a `ureq::Agent` with 10 s timeouts, gzip, `User-Agent: omamusic/<version> (+https://github.com/omafellas/omamusic)`, follows redirects (Cover Art Archive answers 307 to archive.org). Tests use a fake keyed by URL prefix. `ureq` is the only place that touches the network.
- `musicbrainz.rs`: query building and JSON parsing. Search endpoint `https://musicbrainz.org/ws/2/release/?fmt=json&limit=5&query=<lucene>`. Lucene query: `release:"<album>"` plus `AND artist:"<artist>"` when the artist is known and not "Various Artists", plus `AND tracks:<n>`. Album and artist strings are escaped for Lucene and percent-encoded. For candidates and for known IDs: `https://musicbrainz.org/ws/2/release/<id>?fmt=json&inc=recordings+artist-credits+release-groups`. A global throttle guarantees at least 1000 ms between any two MusicBrainz requests.
- `score.rs`: beets-style distance in `[0, 1]`, lower is better. Pure function over `(local album, candidate release)`. Components and weights (builder's call, tuned from beets defaults):
  - album title similarity, weight 3
  - artist similarity, weight 3, skipped when local artist unknown or Various Artists
  - track count mismatch, weight 2 per extra or missing track, capped
  - per track: title similarity weight 1 (skipped when the local title came from the file name), duration difference beyond 2 s weight 2 scaled by `min(1, diff/30 s)`
  - year difference, weight 1, only when both known
  Tracks are paired by track number when the local numbers are complete and unique, otherwise greedily by closest duration. Similarity is normalized Levenshtein over lowercased strings with punctuation, articles and bracketed suffixes like "(Deluxe Edition)" stripped. Implemented in-house, no crate.
  Accept when distance ≤ 0.15 and at least one of these is true: durations were available for every track and matched within 2 s, or an ID came from tags. Anything else is `unmatched` with the best candidate recorded.
- `covers.rs`: ordered sources. Cover Art Archive `https://coverartarchive.org/release/<id>/front-500`, then `release-group/<rg>/front-500`. Then iTunes Search `https://itunes.apple.com/search?entity=album&limit=5&term=<artist album>`; take `artworkUrl100` with `100x100bb` replaced by `1200x1200bb`, only when normalized artist and album both match ≥ 0.9. Then Deezer `https://api.deezer.com/search/album?q=artist:"<artist>" album:"<album>"`, same check, `cover_xl`. The iTunes and Deezer paths also run for albums whose tags are complete but which found no MusicBrainz match, since covers need less certainty than tags. Every downloaded image must decode with `image`, be at least 300 px on the short side, and have aspect ratio within 10 % of square.
- `write.rs`: per track file. Open with lofty, add a front cover picture when the tag has none, fill each empty field (title, artist, album, year, track number when 0, the three MusicBrainz IDs when absent) from the matched release's paired track, save. Save path: copy the original to `<name>.omamusic-tmp` in the same directory, let lofty write into the copy, `fsync`, rename over the original, preserve permissions. If the directory or file is read-only, skip the file and continue; the album still gets its cache cover. Never touch a file whose tag already holds the value.
- `worker.rs`: one background thread owned by `Library`. Receives "scan finished" and "setting toggled" events. On each wake, when `online_lookup` is on and no scan is running, pulls the next album that needs work (oldest attempt first, never-attempted first), processes it fully, writes its `lookup` row, invalidates `ArtworkCache` for the key, and loops until the queue is empty or the switch turns off. Owns its own SQLite writer connection (`Db::open_writer`; `configure` already sets a busy timeout). Writes the fetched image to `~/.cache/omamusic/covers/<sha256(album_key) hex>.jpg` before touching any file, so cache-only mode and file mode share one path.

### Storage

Schema version 3:

```sql
CREATE TABLE IF NOT EXISTS lookup (
    album_key     TEXT PRIMARY KEY,
    status        TEXT NOT NULL,      -- 'matched' | 'unmatched' | 'error'
    release       TEXT,               -- MusicBrainz release id when matched
    release_group TEXT,
    score         REAL,               -- best distance seen
    candidate     TEXT,               -- "Artist – Title (year)" of the best candidate, for the UI
    attempted_at  INTEGER NOT NULL    -- unix seconds
);
```

`tracks` gains `tags_missing INTEGER NOT NULL DEFAULT 0`, `mb_release_group`, `mb_release`, `mb_recording` (TEXT NOT NULL DEFAULT ''). `albums` gains `tags_missing` and `mb_release_group` from the roll-up.

`index::update_artwork` gains a third source after folder art: the cache file for the key. `AlbumSummary` gains `unmatched: bool` (a `lookup` row with status `unmatched`).

Cache files survive database rebuilds; `lookup` rows do not, which only costs a retry.

### UI

- Album grid card and album detail header: when `unmatched`, a small badge in the cover's top-right corner, `IconName::Info` or the closest available, tooltip "Could not be identified online". Element id `album-unmatched-<key>` with `test_support()`.
- Settings, below the switch: a status line. While working: "Looking up albums… 3 of 12". When idle with unmatched albums: "4 albums could not be identified" followed by a compact scrollable list, one row per album "Artist – Title", and a "Retry all" button (id `lookup-retry`) that deletes `unmatched` rows and wakes the worker. When idle with none: nothing.
- Progress comes from `LibrarySnapshot` fields `lookup_pending: usize`, `lookup_total: usize`, `lookup_unmatched: Vec<AlbumSummary>` updated by the worker through the existing `update` path, so views redraw via the version counter.

### Privacy, docs, packaging

- README: the "Features" bullet and the flatpak paragraph change from "fully local" to "local by default; one opt-in setting talks to MusicBrainz, Apple and Deezer and writes into your music files". List exactly what is sent: album title, artist name, track count. Nothing else.
- Flatpak manifest gains `--share=network` and `--filesystem=xdg-music` becomes read-write only if the maintainer wants file writes inside the sandbox; builder's call is to keep `:ro` and document that the flatpak gets cache-only covers.
- `User-Agent` carries the repo URL as contact, per MusicBrainz policy.

### Failure behavior

- Network errors, non-200 statuses, JSON that does not parse: `lookup.status = 'error'`, retried after 30 days or on "Retry all". Never surfaces as an error dialog; appears in the Settings count as "could not be identified".
- Rate limiting (503 from MusicBrainz): back off 5 s, then 30 s, then stop the run; resume on next wake.
- Image fails validation: try the next source.
- File write fails: log to stderr, continue with next file; album still `matched`.
- App quits mid-write: the temp-plus-rename scheme leaves the original intact.

## Testing

- No test may open a socket. The worker is constructed with `Arc<dyn Http>`; production wiring happens in `Library::new` only.
- Unit tests (same file, pure logic): Lucene escaping and URL building, MusicBrainz JSON parsing from inline fixtures, distance scoring (exact match near 0, wrong track count rejected, duration-only match accepted, title-only weak match rejected), cover URL rewriting, image validation (tiny or non-square rejected).
- Worker test (`#[cfg(test)]`, in-memory DB, temp dir): three WAV files written by lofty with empty tags and known durations; fake `Http` returns a search page and a release with matching durations and a PNG; after one run the files carry title, artist, album, year and a picture; the cache file exists; `lookup` says `matched`; `ArtworkCache` was invalidated. Second test: fake returns no candidates; `lookup` says `unmatched`; `db::album_summaries` marks the album `unmatched`. Third: read-only directory; files untouched, cache file written, status `matched`.
- Smoke tests in `tests/ui.rs`: clicking `pref-online-lookup` persists `online_lookup: true` to the temp config and the switch reads back on; with a `Library` whose DB has an `unmatched` row, the badge element is present on the card and the Settings count reads "1 album could not be identified"; clicking `lookup-retry` removes the row.
- Pixel test: badge bounds contain ink when `unmatched`, none when not.

## Phase 2: fingerprinting (not in this plan)

For albums where text matching yields nothing (no tags, generic file names): decode the first 120 s of each track through symphonia, compute a Chromaprint-compatible fingerprint with the `rusty-chromaprint` crate (verify output against upstream `fpcalc` on three files before adopting), query `https://api.acoustid.org/v2/lookup` with an application key registered for omamusic, feed the returned recording and release-group IDs into the same candidate scorer. Adds roughly 0.5–1 s CPU per track, one request per track at 3 req/s. Needs the key stored in the binary like beets does.

## Open for review by the owner

- Setting label and helper text wording.
- Accept threshold 0.15 and the "durations or IDs required" rule. Stricter is safer, looser fixes more albums.
- Whether the flatpak should get read-write music access for file writes.
- Badge icon choice.
