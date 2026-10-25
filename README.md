# Omamusic

Minimalist local music player optimized for Omarchy Linux written in Rust & GPUI.

[![CI](https://github.com/omafellas/omamusic/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/omafellas/omamusic/actions/workflows/ci.yml?query=branch%3Amain)

## Install

```sh
cargo install --path .
```

The installed player needs no separate media player, codec package, or GStreamer plugin. It uses the desktop's normal audio service through PulseAudio (including PipeWire's PulseAudio compatibility service), with ALSA available as a fallback. The optional folder picker uses `zenity` or `kdialog`; you can also type a directory path in Settings.

Building from source on Linux requires the ALSA development headers for CPAL (`alsa-lib` on Arch/Omarchy or `libasound2-dev` on Ubuntu), in addition to GPUI's normal graphics build requirements. These are build requirements, not playback plugins or companion programs.

Run `omamusic`, open Settings, and add a music folder. Configuration is stored at `~/.config/omamusic/config.json`. Set `OMAMUSIC_CONFIG_DIR` to use a different config directory.

## Features

- Local, read-only library indexed into a SQLite database at `~/.cache/omamusic/library.db`; album, artist and song views; no accounts or network services.
- Embedded metadata (title, artist, album, genre, track number, year) and cover art for MP3, FLAC, MP4/M4A and Vorbis comments, with folder cover art fallback.
- In-process decoding through Rodio/Symphonia and output through CPAL. Supports MP3, FLAC, Vorbis, AAC/ALAC, WAV and AIFF without installed codecs.
- Play/pause, seek, previous/next, shuffle, repeat and saved volume.
- BM25 full-text search (SQLite FTS5) with partial-word matching, and album detail views inspired by Overdrive.
- Background incremental rescan on filesystem changes: only new or changed files are re-read, and folders added in Settings are picked up.
- Omarchy theme following through `gpui-omarchy`.

Space toggles playback, Left/Right changes tracks, and Ctrl+R rescans the library. Ctrl+= / Ctrl+- adjust the text size (Ctrl+0 resets it); the setting is also in Settings and persists across restarts. A folder picker uses `zenity` or `kdialog` if available.

## Architecture

`src/library.rs` owns the in-memory library state and pages rows from SQLite on demand, so the full library is never held in memory. `src/db.rs` defines the schema: `tracks` is the source of truth, `albums` is a materialized roll-up, and two FTS5 tables index title, artist, album and genre for BM25 search through triggers. `src/index.rs` reconciles the configured directories in the background, comparing size and mtime so unchanged files are never re-read; cover art is stored as blobs and loaded lazily by `src/artwork.rs`. `src/search.rs` translates user input into safe FTS5 queries. `src/metadata.rs` reads local tags without requiring a codec plugin. `src/player.rs` decodes and buffers audio on a worker thread and feeds CPAL's output callback, keeping disk reads and decoding off the audio thread. `src/app.rs` renders the GPUI interface and consumes the library and playback snapshots. `src/config.rs` persists folders and volume.

The SQLite index is disposable: if it is missing, corrupt or from an older schema it is rebuilt from the audio files on the next start. Deleting `~/.cache/omamusic/library.db` is always safe.

Opus is not decoded by the bundled Symphonia version, so `.opus` files are not included in the library. Opus streams inside `.ogg` containers are also unsupported.

MPRIS integration and system media key registration are not implemented yet. Track transitions occur at end of file but are not gapless.

## Development

```sh
cargo fmt --check
cargo check --offline
cargo clippy --offline --all-targets -- -D warnings
cargo test --offline
```

The regular playback tests use a simulated audio output; the ID3/MP3 regression test also requires `ffmpeg` to generate its fixture. The app itself does not use `ffmpeg`. To verify the actual desktop output with a silent WAV, run `cargo test system_audio_output_plays_file -- --ignored` from a desktop session.

## License

MIT
