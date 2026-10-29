# Omamusic

Minimalist local music player optimized for Omarchy Linux written in Rust & GPUI.

[![CI](https://github.com/omafellas/omamusic/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/omafellas/omamusic/actions/workflows/ci.yml?query=branch%3Amain)

## Install

```sh
cargo install --path .
```

The installed player needs no separate media player, codec package, or GStreamer plugin. It uses the desktop's normal audio service through PulseAudio (including PipeWire's PulseAudio compatibility service), with ALSA available as a fallback. The optional folder picker uses `zenity` or `kdialog`; you can also type a directory path in Settings.

Building from source on Linux requires the ALSA development headers for CPAL (`alsa-lib` on Arch/Omarchy or `libasound2-dev` on Ubuntu), in addition to GPUI's normal graphics build requirements. These are build requirements, not playback plugins or companion programs.

Run `omamusic`: on first launch it picks up `~/Music` automatically, no setup needed. Settings can add, browse or remove folders. Configuration is stored at `~/.config/omamusic/config.json`. Set `OMAMUSIC_CONFIG_DIR` to use a different config directory.

## Features

- Local library indexed into a SQLite database at `~/.cache/omamusic/library.db`; album, artist and song views; no accounts, and no network use unless you turn it on.
- Optional online lookup, off by default: identifies albums on MusicBrainz, fetches missing covers from Cover Art Archive, iTunes or Deezer, and writes covers and missing tags into your files. Only empty fields are filled; existing tags are never changed. Sends only album title, artist name and track count. Albums it cannot identify get a badge and are listed in Settings with a retry button.
- Fingerprint matching for files with no usable tags: a Chromaprint fingerprint of the first two minutes of each track (computed in-process, no `fpcalc`) is sent to AcoustID, and the recordings it returns drive the MusicBrainz match. The omamusic AcoustID application key is baked in at build time from `OMAMUSIC_ACOUSTID_KEY`, which `mise.toml` and CI set, so builds made through `mise` or from a release carry it. A plain `cargo install` outside mise has no key; set the variable when building, or put your own key from https://acoustid.org/new-application into `~/.config/omamusic/config.json` as `"acoustid_key": "..."` and restart. Without a key, untagged albums are simply left as unidentified and Settings say so.
- Embedded metadata (title, artist, album, genre, track number, year) and cover art for MP3, FLAC, MP4/M4A, Ogg Vorbis, WAV and AIFF, with folder cover art fallback.
- In-process decoding through Rodio/Symphonia and output through CPAL. Supports MP3, FLAC, Vorbis, AAC/ALAC, WAV and AIFF without installed codecs.
- Play/pause, seek, previous/next, mute, shuffle, repeat and saved volume.
- BM25 full-text search (SQLite FTS5) with partial-word matching, and album detail views inspired by Overdrive.
- Background incremental rescan on filesystem changes: only new or changed files are re-read, and folders added in Settings are picked up.
- Music-reactive visualizer strip along the bottom edge that follows the live Omarchy theme (toggle in Settings).
- Genres as a grid of cover mosaics, with play and queue actions on each tile.
- Short transitions: pages fade in and covers fade in as they decode. The desktop's reduced-motion setting (XDG portal) turns them off.
- Omarchy theme following through `gpui-omarchy`.

Space toggles playback, Left/Right changes tracks, Ctrl+F jumps to the search field, and Ctrl+R rescans the library. Ctrl+= / Ctrl+- adjust the text size (Ctrl+0 resets it); the setting is also in Settings and persists across restarts. A folder picker uses `zenity` or `kdialog` if available.

## Flatpak

Build a sandboxed bundle from this checkout:

```sh
./flatpak/build.sh
```

The script exports a clean source tree, installs the GNOME 48 runtime and SDK from Flathub on first run, builds the release binary inside the flatpak sandbox (crates are fetched only during the build), and produces a standalone `io.github.omafellas.omamusic.flatpak` bundle:

```sh
flatpak install --user ./io.github.omafellas.omamusic.flatpak
flatpak run io.github.omafellas.omamusic
```

The bundle asks for only the permissions the player actually uses:

| Permission | Needed for |
| --- | --- |
| `--socket=wayland`, `--socket=fallback-x11`, `--device=dri` | showing the window (GPUI renders through Vulkan) |
| `--socket=pulseaudio` | audio output |
| `--own-name=org.mpris.MediaPlayer2.omamusic` | MPRIS media controls |
| `--own-name=org.kde.StatusNotifierItem-*`, `--talk-name=org.kde.StatusNotifierWatcher` | the tray icon |
| `--talk-name=org.freedesktop.Notifications` | the "still running in tray" hint |
| `--filesystem=xdg-music:ro` | scanning the default `~/Music` library (read-only) |
| `--filesystem=~/.local/state/omarchy:ro` | following the live Omarchy theme colors |
| `--share=network` | the optional online lookup (MusicBrainz, Cover Art Archive, iTunes, Deezer) |

The network is used only while the online lookup setting is on. The music folder stays read-only inside the sandbox, so fetched covers are kept in the cache and tags are not written into files there. Folders beyond `~/Music` are added through the system file chooser portal; if you type a path in Settings instead, grant the sandbox read access to it:

```sh
flatpak override --user --filesystem=/path/to/Music:ro io.github.omafellas.omamusic
```

Config and the SQLite cache live inside the sandbox's own directory (`~/.var/app/io.github.omafellas.omamusic/`), separate from the native install paths above.

## Architecture

`src/library.rs` owns the in-memory library state and pages rows from SQLite on demand, so the full library is never held in memory. `src/db.rs` defines the schema: `tracks` is the source of truth, `albums` is a materialized roll-up, and two FTS5 tables index title, artist, album and genre for BM25 search through triggers. `src/index.rs` reconciles the configured directories in the background, comparing size and mtime so unchanged files are never re-read; cover art is stored as blobs and loaded lazily by `src/artwork.rs`. `src/search.rs` translates user input into safe FTS5 queries. `src/metadata.rs` reads local tags without requiring a codec plugin. `src/player.rs` decodes and buffers audio on a worker thread and feeds CPAL's output callback, keeping disk reads and decoding off the audio thread. `src/app.rs` renders the GPUI interface and consumes the library and playback snapshots. `src/config.rs` persists folders and volume.

The SQLite index is disposable: if it is missing, corrupt or from an older schema it is rebuilt from the audio files on the next start. Deleting `~/.cache/omamusic/library.db` is always safe.

Opus is not decoded by the bundled Symphonia version, so `.opus` files are not included in the library. Opus streams inside `.ogg` containers are also unsupported.

MPRIS media controls and the system tray icon are integrated through the session bus. Track transitions occur at end of file but are not gapless.

## Development

The toolchain is managed with [mise](https://mise.jdx.dev). It installs Rust (with `clippy`, `rustfmt` and `rust-analyzer`), the `mbx` cargo wrapper, [hk](https://hk.jdx.dev) and `pkl`:

```sh
mise trust
mise install
hk install
```

`hk install` sets up the pre-commit hook, which runs `cargo fmt` and `cargo clippy`. Run the same checks over the whole repo with:

```sh
mise run check
mise run fix
```

Machine-specific settings, such as `CARGO_BUILD_JOBS`, go in the git-ignored `mise.local.toml`.

The full suite also runs directly through cargo:

```sh
cargo fmt --check
cargo check --offline
cargo clippy --offline --all-targets -- -D warnings
cargo test --offline
```

The regular playback tests use a simulated audio output; the ID3/MP3 regression test also requires `ffmpeg` to generate its fixture. The app itself does not use `ffmpeg`. To verify the actual desktop output with a silent WAV, run `cargo test system_audio_output_plays_file -- --ignored` from a desktop session.

## License

MIT
