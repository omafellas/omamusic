# Omamusic

A small local music player for [Omarchy Linux](https://github.com/omacom/omarchy), written in Rust with [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui). Linux only (macOS and Windows may compile but are not actively supported).

[![CI](https://github.com/omafellas/omamusic/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/omafellas/omamusic/actions/workflows/ci.yml?query=branch%3Amain)

## Install

Requirements: stable Rust, GPUI's Linux graphics build dependencies (Vulkan, Wayland/X11, fontconfig, freetype), and the ALSA and PulseAudio development headers. On Arch/Omarchy the audio packages are `alsa-lib` and `libpulse`; the [CI workflow](.github/workflows/ci.yml) lists the Ubuntu packages used to build and test.

```sh
cargo install --path .
omamusic
```

On first launch omamusic indexes `~/Music` if it exists. Add or remove folders in Settings; Browse opens the system file chooser.

At runtime it uses the desktop's PulseAudio/PipeWire service and falls back to ALSA. Decoders are built in, so no GStreamer plugins or extra codec packages are needed.

## Features

- Local library with album, artist, song, recently added and genre views (genres as cover mosaics), liked songs, playlists and a play queue.
- BM25 full-text search (SQLite FTS5) that matches partial words as you type.
- Play/pause, seek, previous/next, shuffle, repeat, mute, saved volume and play counts.
- Embedded tags and cover art for MP3, FLAC, Ogg Vorbis, AAC/ALAC (M4A/MP4), WAV, AIFF and CAF, with folder covers as fallback.
- Background incremental rescan: only new or changed files are re-read.
- Omarchy theme following and a spectrum visualizer along the bottom edge (reduced motion turns fades off).
- MPRIS media controls and a tray icon; closing the window keeps omamusic running there when a tray is available.

## Shortcuts

| Key | Action |
| --- | --- |
| `Space` | Play/pause |
| `Left` / `Right` | Previous / next track |
| `Alt+Left` / `Alt+Right` | Back / forward |
| `Ctrl+F` | Focus search |
| `Ctrl+R` | Rescan library |
| `Ctrl+=` / `Ctrl+-` | Increase / decrease text size |
| `Ctrl+0` | Reset text size |
| `Ctrl+Q` | Quit |

Text size is also in Settings and persists across restarts.

## Online lookup

When enabled, omamusic searches MusicBrainz, fills only empty tags, and fetches missing covers from Cover Art Archive, then iTunes or Deezer. Existing tags are never overwritten, and new covers are written into files where possible. Albums that cannot be identified get a badge and appear in Settings with a Retry button.

Lookup sends album titles, artist names, track counts and any stored MusicBrainz release-group IDs to MusicBrainz, album and artist names to cover providers, and—when a key is configured and name matching fails—an in-process Chromaprint fingerprint of the first two minutes of each track to AcoustID. That is the only network traffic.

AcoustID matching needs an application key. Builds through mise or CI bake in the project key from `OMAMUSIC_ACOUSTID_KEY`; a plain `cargo install` or Flatpak build outside a mise shell has none. Set that variable while building, or put your own key from https://acoustid.org/new-application in `~/.config/omamusic/config.json` as `"acoustid_key": "..."` and restart. Without a key, untagged albums cannot be identified by sound.

## Data

| Path | Contents |
| --- | --- |
| `~/.config/omamusic/config.json` | Settings |
| `~/.config/omamusic/liked.json` | Liked songs |
| `~/.config/omamusic/playlists.json` | Playlists |
| `~/.cache/omamusic/library.db` | SQLite index and play counts |
| `~/.cache/omamusic/covers/` | Cached tray and lookup covers |

Set `OMAMUSIC_CONFIG_DIR` to use a different config directory.

## Flatpak

Build a sandboxed bundle from this checkout:

```sh
./flatpak/build.sh
```

The script needs `flatpak`, `flatpak-builder`, `git` and `cargo`, plus the host build dependencies above. It builds the release binary on the host, exports the source tree, installs the GNOME 48 runtime and SDK from Flathub on first use (about 2 GB), and writes `io.github.omafellas.omamusic.flatpak`.

```sh
flatpak install --user ./io.github.omafellas.omamusic.flatpak
flatpak run io.github.omafellas.omamusic
```

`~/Music` is read-only inside the sandbox, so fetched covers stay in the cache and tags are not written into those files. For a path typed in Settings, grant read access with:

```sh
flatpak override --user --filesystem=/path/to/Music:ro io.github.omafellas.omamusic
```

Flatpak config and cache live in `~/.var/app/io.github.omafellas.omamusic/`, separate from the native paths above.

## Limitations

- Opus is not supported: the bundled Symphonia build has no Opus decoder, so `.opus` files and Opus streams in `.ogg` containers are skipped.
- Track changes happen at end of file and are not gapless.

## Development

The toolchain is managed with [mise](https://mise.jdx.dev), which installs stable Rust with clippy, rustfmt and rust-analyzer:

```sh
mise trust
mise install
```

Run the suite with:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`OMAMUSIC_CONFIG_DIR=/tmp/omamusic-qa cargo run` runs against a throwaway config; `cargo run -- --dump-library` prints the scanned index without opening a window.

## License

MIT
