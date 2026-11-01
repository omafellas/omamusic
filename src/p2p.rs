//! P2P playlist sharing over the internet, powered by iroh.
//!
//! A playlist can be shared as a single opaque string
//! (`omamusic://<iroh-blob-ticket>`). The ticket points at a content-addressed
//! manifest (BLAKE3-hashed via iroh-blobs) that lists every song in the
//! playlist. Any Omamusic user can paste the string into the search field to
//! join the playlist and download every song locally.
//!
//! Security model
//! -------------
//! - Only audio files may ever enter a P2P share: extension + macOS-junk
//!   rejection, magic-byte inspection, mimetype derivation, per-file and
//!   per-playlist size caps, and a full header parse via `lofty`.
//! - P2P files live in an isolated directory under the cache dir. They are
//!   never added to the user's music folders and never indexed by the main
//!   library.
//! - Every blob is integrity-verified by iroh-blobs' BLAKE3 content hashing, so
//!   a peer cannot serve corrupted or unexpected bytes for a given hash.
//! - The manifest is bound to the owner's iroh node id (the ticket carries the
//!   owner's endpoint address), so a share can only be fetched from its owner.
//! - A playlist cannot exceed `max_p2p_playlist_size_gb` from Settings.
//! - Playback is gated: a track plays only once it is fully synced locally.
//!
//! The manager runs iroh + iroh-blobs on a dedicated background tokio thread.
//! The UI talks to it through synchronous command methods and drains events
//! each frame, so the rest of the app stays synchronous.

use crate::db;
use crate::metadata;
use anyhow::{anyhow, bail, ensure, Context, Result};
use async_channel::{unbounded, Receiver, Sender};
use iroh::endpoint::{presets, Connection};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, PublicKey, SecretKey, Signature};
use iroh_blobs::ticket::BlobTicket;
use iroh_blobs::{store::fs::FsStore, BlobFormat, BlobsProtocol, Hash, HashAndFormat, ALPN};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Prefix that identifies an Omamusic P2P share string.
pub const P2P_PREFIX: &str = "omamusic://";

/// ALPN used by the iroh blobs protocol.
const BLOBS_ALPN: &[u8] = ALPN;

/// Default per-file size cap for audio imported into a P2P share (500 MiB).
pub const DEFAULT_MAX_FILE_SIZE_BYTES: u64 = 500 * 1024 * 1024;

/// Sub-directory of the cache dir that holds all isolated P2P files.
const P2P_SUBDIR: &str = "p2p";

/// Name of the hidden folder, inside the user's primary music directory, where
/// synced shares live. The library indexer skips this folder, so shared songs
/// stay isolated yet on-disk (playable offline), while living in the music dir.
pub const SHARE_SUBDIR: &str = ".omamusic-shared";

// ---------------------------------------------------------------------------
// Share strings
// ---------------------------------------------------------------------------

/// Encode a manifest hash + owner endpoint address into a share string.
///
/// The returned string is `omamusic://` + the base32-encoded iroh blob ticket.
pub fn encode_share(addr: iroh::EndpointAddr, hash: Hash) -> String {
    let ticket = BlobTicket::new(addr, hash, BlobFormat::Raw);
    format!("{P2P_PREFIX}{ticket}")
}

/// Parse a share string back into a manifest hash and the owner's address.
///
/// Accepts both `omamusic://<ticket>` and a raw ticket string.
pub fn parse_share(share: &str) -> Result<(Hash, EndpointAddr, BlobFormat)> {
    let ticket = if let Some(rest) = share.strip_prefix(P2P_PREFIX) {
        BlobTicket::from_str(rest.trim())
    } else {
        BlobTicket::from_str(share.trim())
    }
    .map_err(|_| anyhow!("invalid P2P share string"))?;
    Ok((ticket.hash(), ticket.addr().clone(), ticket.format()))
}

/// Current sync progress for one playlist.
#[derive(Debug, Clone, Default)]
pub struct SyncProgress {
    pub bytes_synced: u64,
    pub total_bytes: u64,
    /// Bytes per second, smoothed over the transfer.
    pub speed_bps: f64,
}

/// Persisted in-memory description of one P2P playlist.
#[derive(Debug, Clone, Default)]
pub struct ShareInfo {
    /// The `omamusic://` share string, set for the owner and for any peer that
    /// is currently re-seeding the share.
    pub share: Option<String>,
    /// Number of peers currently syncing this playlist.
    pub peer_count: usize,
    pub progress: Option<SyncProgress>,
    /// True once every song in the playlist is fully synced locally.
    pub synced: bool,
    /// Are we the owner of this share?
    pub owner: bool,
    /// Are we actively seeding this share (endpoint bound, tickets live)?
    pub seeding: bool,
    /// Manifest (root) blob hash, hex encoded.
    pub manifest_hash: Option<String>,
    /// Owner's iroh node id, hex encoded.
    pub owner_node_id: Option<String>,
    pub total_size: u64,
    pub last_updated: Option<i64>,
    /// Cumulative bytes served to peers (bandwidth "given"), persisted so a
    /// restart keeps the running total.
    pub seeded_bytes: u64,
}

/// Commands the UI sends to the P2P background thread.
#[derive(Debug)]
pub enum P2pCommand {
    /// Import `songs` into the blob store and publish a share for `playlist_id`.
    CreateShare {
        playlist_id: u64,
        songs: Vec<PathBuf>,
    },
    /// Join a share: download the manifest and every song into `playlist_id`.
    JoinShare { playlist_id: u64, share: String },
    /// Stop syncing `playlist_id` and delete its local files.
    Leave { playlist_id: u64 },
    /// Re-publish an already-synced playlist's manifest from this node, so it
    /// becomes a seeder too (peer-to-peer sharing without the original owner).
    Reshare { playlist_id: u64 },
    /// Stop advertising a share we seeded, keeping the synced files + membership.
    StopSeeding { playlist_id: u64 },
    /// Refresh the peer count / progress snapshot (answered with events).
    Refresh { playlist_id: u64 },
}

/// Events the background thread sends back to the UI.
#[derive(Debug, Clone)]
pub enum P2pEvent {
    EnabledChanged {
        enabled: bool,
    },
    MaxSizeChanged {
        gb: u64,
    },
    ShareCreated {
        playlist_id: u64,
        share: String,
    },
    ShareFailed {
        playlist_id: u64,
        reason: String,
    },
    PeerCountChanged {
        playlist_id: u64,
        count: usize,
    },
    SyncProgress {
        playlist_id: u64,
        bytes_synced: u64,
        total_bytes: u64,
        speed_bps: f64,
    },
    SyncCompleted {
        playlist_id: u64,
        ok: bool,
    },
    Left {
        playlist_id: u64,
    },
    StoppedSeeding {
        playlist_id: u64,
    },
    /// A seeding playlist's running totals changed (uploaded bytes, peers).
    SeedingStats {
        playlist_id: u64,
        seeded_bytes: u64,
        peer_count: usize,
    },
}

/// Information about an audio file validated for sharing.
#[derive(Debug, Clone)]
pub struct AudioInfo {
    pub path: PathBuf,
    pub size: u64,
    pub mime: String,
    /// Duration in seconds, from the file header.
    pub duration: f64,
}

/// Mutable state shared between the background thread and the UI.
#[derive(Debug, Clone, Default)]
pub struct SharedState {
    pub enabled: bool,
    pub max_size_gb: u64,
    pub local_node_id: Option<String>,
    pub network_ready: bool,
    pub shares: HashMap<u64, ShareInfo>,
    /// Endpoint ids of peers currently connected to our blobs protocol.
    pub connected_peers: HashSet<EndpointId>,
}

/// Handle to the P2P subsystem. Cheap and safe to use from the UI thread.
///
/// The iroh endpoint and the background tokio runtime are created lazily on
/// the first share/join command, so an app that never touches P2P spawns no
/// extra threads and binds no ports.
pub struct P2pManager {
    cmd_tx: Sender<P2pCommand>,
    // Taken once when the worker thread starts.
    cmd_rx: Mutex<Option<Receiver<P2pCommand>>>,
    event_tx: Mutex<Option<Sender<P2pEvent>>>,
    event_rx: Receiver<P2pEvent>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    state: Arc<Mutex<SharedState>>,
    p2p_dir: PathBuf,
    share_root: PathBuf,
}

impl P2pManager {
    /// Start the P2P subsystem.
    ///
    /// `cache_dir` holds the internal blob store and cache DB. `share_root` is
    /// where synced, playable copies of each playlist live (a folder inside the
    /// user's primary music directory) so shares keep working fully offline.
    ///
    /// Binds an iroh endpoint, opens the isolated blob store and attaches the
    /// blobs protocol so this machine can both serve and fetch shares. If the
    /// network is unavailable the manager still works locally (shares can be
    /// created but not joined remotely); `network_ready` reports the state.
    pub fn new(
        cache_dir: &Path,
        share_root: &Path,
        enabled: bool,
        max_size_gb: u64,
    ) -> Result<Self> {
        let p2p_dir = cache_dir.join(P2P_SUBDIR);
        std::fs::create_dir_all(&p2p_dir)?;
        // Prefer the in-music share folder; if it is not writable (e.g. the
        // first library folder is read-only or is a test fixture), fall back to
        // the cache dir so P2P still works. Synced files live under this folder.
        let share_root = if std::fs::create_dir_all(share_root).is_ok() {
            share_root.to_path_buf()
        } else {
            let fallback = cache_dir.join("shared");
            std::fs::create_dir_all(&fallback)?;
            fallback
        };

        let (cmd_tx, cmd_rx) = unbounded();
        let (event_tx, event_rx) = unbounded();
        let state: Arc<Mutex<SharedState>> = Arc::new(Mutex::new(SharedState {
            enabled,
            max_size_gb,
            ..Default::default()
        }));

        Ok(Self {
            cmd_tx,
            cmd_rx: Mutex::new(Some(cmd_rx)),
            event_tx: Mutex::new(Some(event_tx)),
            event_rx,
            thread: Mutex::new(None),
            state,
            p2p_dir,
            share_root,
        })
    }

    /// Spawn the worker thread on first use. Idempotent.
    fn start(&self) {
        let mut guard = self.thread.lock();
        if guard.is_some() {
            return;
        }
        let Some(cmd_rx) = self.cmd_rx.lock().take() else {
            return;
        };
        let Some(event_tx) = self.event_tx.lock().take() else {
            return;
        };
        let state = self.state.clone();
        let p2p_dir = self.p2p_dir.clone();
        let share_root = self.share_root.clone();
        let handle = std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("omamusic-p2p")
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    eprintln!("p2p: failed to create tokio runtime: {err:#}");
                    return;
                }
            };
            rt.block_on(async {
                let _ = run(p2p_dir, share_root, cmd_rx, event_tx, state).await;
            });
        });
        *guard = Some(handle);
    }

    /// Whether P2P capabilities are enabled (the "P2P capabilities" setting).
    pub fn enabled(&self) -> bool {
        self.state.lock().enabled
    }

    /// Max P2P playlist size in gigabytes.
    pub fn max_size_gb(&self) -> u64 {
        self.state.lock().max_size_gb
    }

    /// Whether the local endpoint is reachable on the network.
    pub fn network_ready(&self) -> bool {
        self.state.lock().network_ready
    }

    /// Local iroh node id (hex), if the endpoint bound successfully.
    pub fn local_node_id(&self) -> Option<String> {
        self.state.lock().local_node_id.clone()
    }

    /// Info for one playlist, if it is a P2P playlist.
    pub fn share_info(&self, playlist_id: u64) -> Option<ShareInfo> {
        self.state.lock().shares.get(&playlist_id).cloned()
    }

    /// All P2P playlist ids currently known.
    pub fn p2p_playlist_ids(&self) -> Vec<u64> {
        self.state.lock().shares.keys().copied().collect()
    }

    /// The share string for `playlist_id` (owner only).
    pub fn share_string(&self, playlist_id: u64) -> Option<String> {
        self.state
            .lock()
            .shares
            .get(&playlist_id)
            .and_then(|s| s.share.clone())
    }

    /// Connected peer count for `playlist_id`.
    pub fn peer_count(&self, playlist_id: u64) -> usize {
        self.state
            .lock()
            .shares
            .get(&playlist_id)
            .map(|s| s.peer_count)
            .unwrap_or(0)
    }

    /// Current sync progress for `playlist_id`.
    pub fn sync_progress(&self, playlist_id: u64) -> Option<SyncProgress> {
        self.state
            .lock()
            .shares
            .get(&playlist_id)
            .and_then(|s| s.progress.clone())
    }

    /// Whether every song in `playlist_id` is fully synced locally.
    pub fn is_synced(&self, playlist_id: u64) -> bool {
        self.state
            .lock()
            .shares
            .get(&playlist_id)
            .map(|s| s.synced)
            .unwrap_or(false)
    }

    /// The live synced flag for `playlist_id`, or `None` when this session's
    /// manager has no state for it yet (e.g. right after a restart). Callers
    /// fall back to the persisted flag, so already-synced shares stay playable
    /// offline.
    pub fn live_synced(&self, playlist_id: u64) -> Option<bool> {
        self.state.lock().shares.get(&playlist_id).map(|s| s.synced)
    }

    /// True while we are actively seeding `playlist_id`: the endpoint is bound
    /// and this node has a live share (as owner or as a re-seeding peer).
    /// Drives the green badge.
    pub fn is_seeding(&self, playlist_id: u64) -> bool {
        let s = self.state.lock();
        s.network_ready
            && s.shares
                .get(&playlist_id)
                .is_some_and(|info| info.seeding && info.share.is_some())
    }

    /// Cumulative bytes this node has seeded for `playlist_id` (persisted).
    pub fn seeded_bytes(&self, playlist_id: u64) -> u64 {
        self.state
            .lock()
            .shares
            .get(&playlist_id)
            .map(|s| s.seeded_bytes)
            .unwrap_or(0)
    }

    /// Drain all pending events. Call once per UI frame.
    pub fn try_events(&self) -> Vec<P2pEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.event_rx.try_recv() {
            out.push(ev);
        }
        out
    }

    fn send(&self, cmd: P2pCommand) {
        // Start the worker thread on the first command that needs it.
        self.start();
        // Unbounded channel and P2P commands are rare, so this never
        // meaningfully blocks the UI thread.
        let _ = self.cmd_tx.try_send(cmd);
    }

    /// Enable or disable all P2P features. Writes shared state directly so it
    /// never starts the background worker on its own.
    pub fn set_enabled(&self, enabled: bool) {
        self.state.lock().enabled = enabled;
    }

    /// Set the max P2P playlist size in gigabytes (shared state only).
    pub fn set_max_size_gb(&self, gb: u64) {
        self.state.lock().max_size_gb = gb;
    }

    /// Publish `songs` as a shareable P2P playlist.
    pub fn create_share(&self, playlist_id: u64, songs: Vec<PathBuf>) {
        self.send(P2pCommand::CreateShare { playlist_id, songs });
    }

    /// Join a share by its `omamusic://` string into `playlist_id`.
    pub fn join_share(&self, playlist_id: u64, share: String) {
        self.send(P2pCommand::JoinShare { playlist_id, share });
    }

    /// Stop syncing a playlist and delete its local files.
    pub fn leave(&self, playlist_id: u64) {
        self.send(P2pCommand::Leave { playlist_id });
    }

    /// Re-publish an already-synced playlist from this node so it seeds the
    /// share for others. Emits `ShareCreated` with a fresh ticket that points
    /// at this node; the manifest bytes (and owner signature) are unchanged,
    /// so anyone holding the same manifest hash trusts it.
    pub fn reshare(&self, playlist_id: u64) {
        self.send(P2pCommand::Reshare { playlist_id });
    }

    /// Stop advertising a share this node seeded, without deleting the synced
    /// files or leaving the playlist.
    pub fn stop_seeding(&self, playlist_id: u64) {
        self.send(P2pCommand::StopSeeding { playlist_id });
    }

    /// Folder holding the synced, playable files for `playlist_id`, inside the
    /// share root (a dedicated folder within the user's primary music dir).
    pub fn playlist_dir(share_root: &Path, playlist_id: u64) -> PathBuf {
        share_root.join(playlist_id.to_string())
    }
}

// ---------------------------------------------------------------------------
// Security: audio-file validation
// ---------------------------------------------------------------------------

/// Known audio magic-byte prefixes. A real audio container starts with one of
/// these; random files almost never do.
fn magic_is_audio(buf: &[u8]) -> bool {
    if buf.len() < 4 {
        return false;
    }
    // ID3v2 tag.
    if buf.starts_with(b"ID3") {
        return true;
    }
    // MPEG audio frame sync: 0xFF 0xFx.
    if buf[0] == 0xFF && (buf[1] & 0xF0) == 0xF0 {
        return true;
    }
    // FLAC.
    if buf.starts_with(b"fLaC") {
        return true;
    }
    // Ogg (including Opus-in-Ogg).
    if buf.starts_with(b"OggS") {
        return true;
    }
    // MP4/M4A: a size box followed by "ftyp" at offset 4.
    if buf.len() >= 8 && &buf[4..8] == b"ftyp" {
        return true;
    }
    // WAV: "RIFF"...."WAVE".
    if buf.starts_with(b"RIFF") && buf.len() >= 12 && &buf[8..12] == b"WAVE" {
        return true;
    }
    // AAC ADIF.
    if buf.starts_with(b"ADIF") {
        return true;
    }
    // AAC ADTS: 0xFF 0xF0.
    if buf[0] == 0xFF && buf[1] == 0xF0 {
        return true;
    }
    false
}

/// Derive a mimetype from a file extension. Only ever returns `audio/*`.
fn mime_for_extension(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let mime = match ext.as_str() {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "opus" => "audio/ogg",
        "m4a" | "m4b" | "m4p" | "mp4" => "audio/mp4",
        "aac" => "audio/aac",
        "wav" => "audio/wav",
        "aiff" | "aif" => "audio/aiff",
        "caf" => "audio/x-caf",
        "wma" => "audio/x-ms-wma",
        _ => "application/octet-stream",
    };
    mime.to_string()
}

/// Validate that `path` is safe to import into a P2P share.
///
/// Checks, in order:
/// 1. It is an audio file by extension (rejects macOS junk).
/// 2. It is a regular, non-empty file under the per-file size cap.
/// 3. Its magic bytes look like an audio container.
/// 4. Its headers parse with `lofty` (rejects renamed non-audio files).
///
/// Returns the file's size, mimetype and duration on success.
pub fn validate_audio_file(path: &Path, max_file_size: u64) -> Result<AudioInfo> {
    ensure!(
        metadata::is_audio(path),
        "not an audio file: {}",
        path.display()
    );

    let metadata_fs =
        std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    ensure!(
        metadata_fs.is_file(),
        "not a regular file: {}",
        path.display()
    );
    let size = metadata_fs.len();
    ensure!(size > 0, "empty file: {}", path.display());
    ensure!(
        size <= max_file_size,
        "file too large ({} bytes > {}): {}",
        size,
        max_file_size,
        path.display()
    );

    // Magic-byte inspection: reject renamed non-audio files early.
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut head = [0u8; 64];
    let n = std::io::Read::read(&mut file, &mut head)
        .with_context(|| format!("read {}", path.display()))?;
    ensure!(
        magic_is_audio(&head[..n]),
        "not an audio container: {}",
        path.display()
    );

    // Full header parse: only real audio files parse with lofty.
    let meta = metadata::read(path);
    ensure!(
        meta.duration > 0.0,
        "unparseable audio headers: {}",
        path.display()
    );

    Ok(AudioInfo {
        path: path.to_path_buf(),
        size,
        mime: mime_for_extension(path),
        duration: meta.duration,
    })
}

/// Total bytes for a playlist, enforced against `max_size_gb`.
pub fn playlist_size_cap_ok(total_bytes: u64, max_size_gb: u64) -> bool {
    let cap = max_size_gb as u128 * 1024 * 1024 * 1024;
    (total_bytes as u128) <= cap
}

/// Format bytes as a human readable string (B/KB/MB/GB/TB).
pub fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let mut size = bytes as f64;
    let mut idx = 0;
    while size >= 1024.0 && idx < UNITS.len() - 1 {
        size /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{} {}", bytes, UNITS[idx])
    } else {
        format!("{:.1} {}", size, UNITS[idx])
    }
}

/// Format bytes-per-second as MB/s or KB/s.
pub fn format_speed(bps: f64) -> String {
    let bps = bps.max(0.0);
    if bps >= 1_000_000.0 {
        format!("{:.1} MB/s", bps / 1_000_000.0)
    } else if bps >= 1_000.0 {
        format!("{:.1} KB/s", bps / 1_000.0)
    } else {
        format!("{:.0} B/s", bps)
    }
}

/// Derive a file extension from a mimetype. Falls back to "audio".
pub fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "audio/mpeg" => "mp3",
        "audio/flac" => "flac",
        "audio/ogg" => "ogg",
        "audio/mp4" => "m4a",
        "audio/aac" => "aac",
        "audio/wav" => "wav",
        "audio/aiff" => "aiff",
        "audio/x-caf" => "caf",
        "audio/x-ms-wma" => "wma",
        _ => "audio",
    }
}

/// Make a filesystem-safe file name from an arbitrary title. Collapses
/// separators and control characters, strips leading dots, and truncates to a
/// sane length. Used when copying a synced P2P song into a music folder.
pub fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_start_matches('.').trim();
    let stem = if trimmed.is_empty() {
        "track".to_string()
    } else {
        trimmed.chars().take(120).collect()
    };
    stem
}

/// Read local title/artist/album metadata from a synced file, falling back to
/// the manifest's original name when tags are missing.
fn read_local_metadata(path: &Path, fallback_name: &str) -> (String, String, String) {
    let meta = metadata::read(path);
    let stem = Path::new(fallback_name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| fallback_name.to_string());
    let title = if meta.title.trim().is_empty() {
        stem
    } else {
        meta.title.clone()
    };
    (title, meta.artist.clone(), meta.album.clone())
}

// ---------------------------------------------------------------------------
// Local copy / conversion (peer leaves a share with their music)
// ---------------------------------------------------------------------------

/// Copy a set of synced P2P files into `dest_dir`, validating each as audio
/// first and giving each a safe, readable file name. Returns the new paths.
///
/// This is the escape hatch that lets a peer keep the music outside the
/// isolated cache. Every file is re-validated (`validate_audio_file`) so a
/// corrupted or non-audio blob can never be planted into a music folder.
pub fn copy_audio_files(
    files: &[(PathBuf, String)],
    dest_dir: &Path,
    max_file_size: u64,
) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dest_dir)?;
    let mut copied = Vec::new();
    for (src, title) in files {
        validate_audio_file(src, max_file_size)
            .map_err(|e| anyhow!("skipping {}: {e}", src.display()))?;
        let ext = src
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_else(|| "audio".to_string());
        let stem = sanitize_file_name(title);
        let mut dest = dest_dir.join(format!("{stem}.{ext}"));
        let mut n = 1;
        while dest.exists() {
            dest = dest_dir.join(format!("{stem}-{n}.{ext}"));
            n += 1;
        }
        std::fs::copy(src, &dest)?;
        copied.push(dest);
    }
    Ok(copied)
}

// ---------------------------------------------------------------------------
// Background thread: iroh endpoint + blob store + command loop
// ---------------------------------------------------------------------------

/// Protocol handler that wraps iroh-blobs' [`BlobsProtocol`] and reports
/// connection open/close events so the manager can display a live peer count.
///
/// The wrapped handler is the sole gate for incoming blobs requests: we let it
/// serve only content we've explicitly published (added to the FsStore); a
/// remote peer cannot make us send anything else.
#[derive(Debug)]
struct TrackedBlobs {
    inner: BlobsProtocol,
    peer_tx: Sender<PeerEvent>,
}

#[derive(Debug)]
enum PeerEvent {
    Connected(EndpointId),
    Disconnected(EndpointId),
}

impl ProtocolHandler for TrackedBlobs {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let id = connection.remote_id();
        let _ = self.peer_tx.try_send(PeerEvent::Connected(id));
        let result = self.inner.accept(connection).await;
        let _ = self.peer_tx.try_send(PeerEvent::Disconnected(id));
        result
    }

    async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

/// Isolated SQLite file that caches P2P share/peer state so the app
/// reconnects faster after a restart. Lives next to the blob store, not the
/// main library DB, so a corrupt cache can never hurt the music index.
pub fn p2p_db_path(p2p_dir: &Path) -> PathBuf {
    p2p_dir.join("p2p.db")
}

/// The bound network core: an iroh endpoint, the isolated blob store and the
/// router that serves blobs to peers. Created lazily on the first share or
/// join so a program that never touches P2P binds no ports or threads beyond
/// the (idle) command loop.
struct Core {
    endpoint: Endpoint,
    store: Arc<FsStore>,
    /// Kept alive for the process; dropping it stops serving blobs.
    _router: Router,
}

/// Bind the endpoint + open the blob store + attach the blobs protocol.
async fn bind_core(p2p_dir: PathBuf, peer_tx: Sender<PeerEvent>) -> Result<Core> {
    let secret = load_or_create_secret_key(&p2p_dir);
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .alpns(vec![BLOBS_ALPN.to_vec()])
        .bind()
        .await
        .map_err(|e| anyhow!("bind iroh endpoint: {e}"))?;
    // The relay connects in the background; we do not block the command loop
    // waiting for it. Tickets carry the node id, so reachability resolves
    // whenever the relay is ready.

    let store = Arc::new(
        FsStore::load(&p2p_dir)
            .await
            .map_err(|e| anyhow!("open blob store: {e}"))?,
    );
    #[allow(clippy::explicit_auto_deref)]
    let blobs = BlobsProtocol::new(&*store, None);
    let tracked = TrackedBlobs {
        inner: blobs,
        peer_tx,
    };
    let router = Router::builder(endpoint.clone())
        .accept(BLOBS_ALPN, tracked)
        .spawn();

    Ok(Core {
        endpoint,
        store,
        _router: router,
    })
}

async fn run(
    p2p_dir: PathBuf,
    share_root: PathBuf,
    cmd_rx: Receiver<P2pCommand>,
    event_tx: Sender<P2pEvent>,
    state: Arc<Mutex<SharedState>>,
) -> Result<()> {
    // The endpoint + store bind lazily; the sender is kept in scope so the
    // peer channel never closes while the loop runs.
    let (peer_tx, peer_rx) = async_channel::bounded::<PeerEvent>(1024);
    let core: Arc<tokio::sync::OnceCell<Core>> = Arc::new(tokio::sync::OnceCell::new());

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Ok(cmd) = cmd else { break };
                let core = core.clone();
                let state = state.clone();
                let event_tx = event_tx.clone();
                let p2p_dir = p2p_dir.clone();
                let share_root = share_root.clone();
                let peer_tx = peer_tx.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_command(
                        cmd, core, &state, &event_tx, &p2p_dir, &share_root, peer_tx,
                    ).await {
                        eprintln!("p2p: command error: {err:#}");
                    }
                });
            }
            ev = peer_rx.recv() => {
                let disconnected = matches!(ev, Ok(PeerEvent::Disconnected(_)));
                let Ok(ev) = ev else { break };
                let mut updates: Vec<(u64, u64)> = Vec::new();
                {
                    let mut s = state.lock();
                    match ev {
                        PeerEvent::Connected(id) => {
                            s.connected_peers.insert(id);
                        }
                        PeerEvent::Disconnected(id) => {
                            s.connected_peers.remove(&id);
                        }
                    }
                    let total = s.connected_peers.len();
                    let seeding: Vec<u64> = s
                        .shares
                        .iter()
                        .filter(|(_, info)| info.seeding)
                        .map(|(id, _)| *id)
                        .collect();
                    for id in &seeding {
                        if let Some(info) = s.shares.get_mut(id) {
                            info.peer_count = total;
                            // A peer that connected and left pulled roughly the
                            // full set: credit it once toward bandwidth given.
                            if disconnected {
                                info.seeded_bytes =
                                    info.seeded_bytes.saturating_add(info.total_size);
                                updates.push((*id, info.seeded_bytes));
                            }
                        }
                    }
                    for id in seeding {
                        let count = s.connected_peers.len();
                        let _ = event_tx.try_send(P2pEvent::PeerCountChanged { playlist_id: id, count });
                    }
                }
                // Persist the running seeded total outside the lock and notify.
                if !updates.is_empty() {
                    if let Ok(conn) =
                        rusqlite::Connection::open(p2p_db_path(&p2p_dir))
                    {
                        let _ = db::ensure_p2p_schema(&conn);
                        let mut guard = state.lock();
                        for (id, bytes) in &updates {
                            let share = guard
                                .shares
                                .get(id)
                                .and_then(|i| i.share.clone())
                                .unwrap_or_default();
                            if let Some(info) = guard.shares.get_mut(id) {
                                let _ = event_tx.try_send(P2pEvent::SeedingStats {
                                    playlist_id: *id,
                                    seeded_bytes: info.seeded_bytes,
                                    peer_count: info.peer_count,
                                });
                            }
                            let _ = db::record_p2p_seed_stats(&conn, *id, *bytes, &share);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

async fn handle_command(
    cmd: P2pCommand,
    core: Arc<tokio::sync::OnceCell<Core>>,
    state: &Arc<Mutex<SharedState>>,
    event_tx: &Sender<P2pEvent>,
    p2p_dir: &Path,
    share_root: &Path,
    peer_tx: Sender<PeerEvent>,
) -> Result<()> {
    match cmd {
        P2pCommand::CreateShare { playlist_id, songs } => {
            let init = core
                .get_or_try_init(|| bind_core(p2p_dir.to_path_buf(), peer_tx.clone()))
                .await;
            match init {
                Ok(c) => {
                    mirror_core_state(state, c);
                    match create_share_impl(c, state, playlist_id, songs, p2p_dir, share_root).await
                    {
                        Ok(share) => {
                            let _ = event_tx
                                .send(P2pEvent::ShareCreated { playlist_id, share })
                                .await;
                        }
                        Err(err) => {
                            let _ = event_tx
                                .send(P2pEvent::ShareFailed {
                                    playlist_id,
                                    reason: err.to_string(),
                                })
                                .await;
                        }
                    }
                }
                Err(err) => {
                    report_core_error(state, event_tx, err).await;
                }
            }
        }
        P2pCommand::JoinShare { playlist_id, share } => {
            let init = core
                .get_or_try_init(|| bind_core(p2p_dir.to_path_buf(), peer_tx.clone()))
                .await;
            match init {
                Ok(c) => {
                    mirror_core_state(state, c);
                    if let Err(err) =
                        join_share_impl(c, state, event_tx, playlist_id, share, p2p_dir, share_root)
                            .await
                    {
                        let _ = event_tx
                            .send(P2pEvent::ShareFailed {
                                playlist_id,
                                reason: err.to_string(),
                            })
                            .await;
                    }
                }
                Err(err) => {
                    report_core_error(state, event_tx, err).await;
                }
            }
        }
        P2pCommand::Leave { playlist_id } => {
            leave_impl(state, p2p_dir, share_root, playlist_id).await?;
            let _ = event_tx.send(P2pEvent::Left { playlist_id }).await;
        }
        P2pCommand::Reshare { playlist_id } => {
            let init = core
                .get_or_try_init(|| bind_core(p2p_dir.to_path_buf(), peer_tx.clone()))
                .await;
            match init {
                Ok(c) => {
                    mirror_core_state(state, c);
                    match reshare_impl(c, state, p2p_dir, playlist_id).await {
                        Ok(share) => {
                            let _ = event_tx
                                .send(P2pEvent::ShareCreated { playlist_id, share })
                                .await;
                        }
                        Err(err) => {
                            let _ = event_tx
                                .send(P2pEvent::ShareFailed {
                                    playlist_id,
                                    reason: err.to_string(),
                                })
                                .await;
                        }
                    }
                }
                Err(err) => {
                    report_core_error(state, event_tx, err).await;
                }
            }
        }
        P2pCommand::StopSeeding { playlist_id } => {
            {
                let mut s = state.lock();
                if let Some(info) = s.shares.get_mut(&playlist_id) {
                    info.seeding = false;
                    info.share = None;
                    info.peer_count = 0;
                }
            }
            let _ = event_tx
                .send(P2pEvent::StoppedSeeding { playlist_id })
                .await;
        }
        P2pCommand::Refresh { playlist_id } => {
            refresh_impl(state, event_tx, playlist_id).await?;
        }
    }
    Ok(())
}

/// Publish the node id + readiness so the UI can show status.
fn mirror_core_state(state: &Arc<Mutex<SharedState>>, core: &Core) {
    let mut s = state.lock();
    s.local_node_id = Some(core.endpoint.id().to_string());
    s.network_ready = true;
}

/// The bind failed (offline): report it once and mark the node offline.
async fn report_core_error(
    state: &Arc<Mutex<SharedState>>,
    event_tx: &Sender<P2pEvent>,
    err: anyhow::Error,
) {
    {
        let mut s = state.lock();
        s.network_ready = false;
    }
    let _ = event_tx
        .send(P2pEvent::ShareFailed {
            playlist_id: 0,
            reason: format!("P2P unavailable: {err}"),
        })
        .await;
}

/// File name for a synced song inside the isolated cache: the blob hash plus a
/// real audio extension derived from its mimetype. Content-addressed, so it
/// can never be a path-traversal vector.
pub fn cached_file_name(hash_hex: &str, mime: &str) -> String {
    format!("{}.{}", hash_hex, extension_for_mime(mime))
}

async fn refresh_impl(
    state: &Arc<Mutex<SharedState>>,
    event_tx: &Sender<P2pEvent>,
    playlist_id: u64,
) -> Result<()> {
    let info = {
        let s = state.lock();
        s.shares.get(&playlist_id).cloned()
    };
    if let Some(info) = info {
        let _ = event_tx
            .send(P2pEvent::PeerCountChanged {
                playlist_id,
                count: info.peer_count,
            })
            .await;
        if let Some(progress) = &info.progress {
            let _ = event_tx
                .send(P2pEvent::SyncProgress {
                    playlist_id,
                    bytes_synced: progress.bytes_synced,
                    total_bytes: progress.total_bytes,
                    speed_bps: progress.speed_bps,
                })
                .await;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Share creation (owner side)
// ---------------------------------------------------------------------------

async fn create_share_impl(
    core: &Core,
    state: &Arc<Mutex<SharedState>>,
    playlist_id: u64,
    songs: Vec<PathBuf>,
    p2p_dir: &Path,
    share_root: &Path,
) -> Result<String> {
    let enabled = state.lock().enabled;
    ensure!(enabled, "P2P capabilities are disabled in Settings");
    let endpoint = &core.endpoint;
    let store = &core.store;
    let max_gb = state.lock().max_size_gb;

    let mut infos = Vec::new();
    let mut total_size: u64 = 0;
    for path in &songs {
        let info = validate_audio_file(path, DEFAULT_MAX_FILE_SIZE_BYTES)?;
        ensure!(
            playlist_size_cap_ok(total_size + info.size, max_gb),
            "playlist would exceed the {max_gb} GB P2P size limit"
        );
        total_size += info.size;
        infos.push(info);
    }
    ensure!(!infos.is_empty(), "no valid audio files to share");

    // Import each song into the content-addressed blob store. iroh-blobs
    // computes a BLAKE3 hash for every file; that hash is both the content id
    // and the integrity check peers verify on download.
    let mut song_entries = Vec::new();
    for info in &infos {
        let tag = store
            .add_path(&info.path)
            .temp_tag()
            .await
            .context("import song into blob store")?;
        song_entries.push(SongEntry {
            name: info
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("track")
                .to_string(),
            hash: tag.hash().to_hex(),
            size: info.size,
            mime: info.mime.clone(),
            duration: info.duration,
        });
    }

    // Export every song into the isolated playlist directory so the owner can
    // play the synced copies locally. File names are hashes, never original
    // names, which prevents path traversal and overwriting.
    let dir = P2pManager::playlist_dir(share_root, playlist_id);
    std::fs::create_dir_all(&dir)?;
    for entry in &song_entries {
        let hash = entry
            .hash
            .parse::<Hash>()
            .context("invalid song hash in manifest")?;
        let out_path = dir.join(cached_file_name(&entry.hash, &entry.mime));
        store
            .export(hash, &out_path)
            .finish()
            .await
            .context("export song to isolated dir")?;
    }

    let now = now_ns();
    let owner_node = endpoint.id().to_string();
    let preimage = manifest_preimage(1, &owner_node, now, &song_entries)?;
    let signature = signature_hex_of(&endpoint.secret_key().sign(&preimage).to_bytes());
    let manifest = Manifest {
        version: 1,
        owner: owner_node.clone(),
        created_at: now,
        songs: song_entries.clone(),
        signature,
    };
    let manifest_bytes = serde_json::to_vec(&manifest)?;

    // Import the manifest itself as a blob.
    let manifest_tag = store
        .add_bytes(manifest_bytes)
        .temp_tag()
        .await
        .context("import manifest into blob store")?;
    let manifest_hash = manifest_tag.hash().to_hex();

    // The share string binds the manifest hash to the owner's endpoint
    // address. Only the owner can serve this ticket.
    let share = encode_share(endpoint.addr(), manifest_tag.hash());

    {
        let mut s = state.lock();
        let entry = ShareInfo {
            share: Some(share.clone()),
            owner: true,
            seeding: true,
            manifest_hash: Some(manifest_hash.clone()),
            owner_node_id: Some(endpoint.id().to_string()),
            total_size,
            last_updated: Some(now),
            synced: true, // owner already has the files
            ..Default::default()
        };
        s.shares.insert(playlist_id, entry);
    }

    // Persist the share record so it survives a restart and connects faster.
    let db_path = p2p_db_path(p2p_dir);
    let conn = rusqlite::Connection::open(&db_path)
        .with_context(|| format!("open library db {:?}", db_path))?;
    db::ensure_p2p_schema(&conn)?;
    db::record_p2p_share(
        &conn,
        playlist_id,
        &endpoint.id().to_string(),
        &manifest_hash,
        total_size,
        song_entries.len() as u64,
    )?;
    // Keep the running seeded total across (re)shares and persist the advertised
    // share so a restart knows exactly how seeding was left.
    let prior_seeded = db::load_p2p_share(&conn, playlist_id)?
        .map(|r| r.seeded_bytes)
        .unwrap_or(0);
    let _ = db::record_p2p_seed_stats(&conn, playlist_id, prior_seeded, &share);
    if let Some(info) = state.lock().shares.get_mut(&playlist_id) {
        info.seeded_bytes = prior_seeded;
    }
    let tracks: Vec<db::P2pTrackRow> = song_entries
        .iter()
        .map(|e| {
            let path = dir.join(cached_file_name(&e.hash, &e.mime));
            let (title, artist, album) = read_local_metadata(&path, &e.name);
            (
                path.to_string_lossy().to_string(),
                e.hash.clone(),
                title,
                artist,
                album,
                e.duration,
                e.size,
                e.mime.clone(),
            )
        })
        .collect();
    db::replace_p2p_tracks(&conn, playlist_id, &tracks)?;
    drop(conn);

    Ok(share)
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u64,
    owner: String,
    created_at: i64,
    songs: Vec<SongEntry>,
    /// Hex-encoded ed25519 signature over the canonical preimage
    /// (`version`, `owner`, `created_at`, `songs`).
    signature: String,
}

/// Deterministic preimage signed by the owner: the manifest without its
/// signature field, serialized with the field order fixed.
fn manifest_preimage(
    version: u64,
    owner: &str,
    created_at: i64,
    songs: &[SongEntry],
) -> Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Body<'a> {
        version: u64,
        owner: &'a str,
        created_at: i64,
        songs: &'a [SongEntry],
    }
    let body = Body {
        version,
        owner,
        created_at,
        songs,
    };
    Ok(serde_json::to_vec(&body)?)
}

fn signature_hex_of(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn signature_from_hex(s: &str) -> Result<[u8; Signature::LENGTH]> {
    let s = s.trim();
    if s.len() != Signature::LENGTH * 2 {
        bail!("signature has wrong length");
    }
    let mut out = [0u8; Signature::LENGTH];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let str = std::str::from_utf8(chunk).map_err(|_| anyhow!("bad hex"))?;
        out[i] = u8::from_str_radix(str, 16).map_err(|_| anyhow!("bad hex"))?;
    }
    Ok(out)
}

/// Verify a manifest's signature using its claimed owner node id as the
/// public key. Fails if either the owner id is not a valid `PublicKey` or
/// the signature doesn't check out.
fn verify_manifest_signature(manifest: &Manifest) -> Result<()> {
    let owner: PublicKey = manifest.owner.parse()?;
    let preimage = manifest_preimage(
        manifest.version,
        &manifest.owner,
        manifest.created_at,
        &manifest.songs,
    )?;
    let bytes = signature_from_hex(&manifest.signature)?;
    let sig = Signature::from_bytes(&bytes);
    owner
        .verify(&preimage, &sig)
        .map_err(|_| anyhow!("manifest signature does not match its owner"))
}

/// Load the persisted iroh key, or create and save a new one.
fn load_or_create_secret_key(p2p_dir: &Path) -> SecretKey {
    let path = p2p_dir.join("secret.key");
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) {
            return SecretKey::from_bytes(&arr);
        }
        eprintln!("p2p: secret key unreadable, regenerating");
    }
    let key = SecretKey::generate();
    let _ = std::fs::write(&path, key.to_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    key
}

#[derive(Serialize, Deserialize, Clone)]
struct SongEntry {
    name: String,
    hash: String,
    size: u64,
    mime: String,
    duration: f64,
}

// ---------------------------------------------------------------------------
// Share joining (peer side)
// ---------------------------------------------------------------------------

async fn join_share_impl(
    core: &Core,
    state: &Arc<Mutex<SharedState>>,
    event_tx: &Sender<P2pEvent>,
    playlist_id: u64,
    share: String,
    p2p_dir: &Path,
    share_root: &Path,
) -> Result<()> {
    let (manifest_hash, owner_addr, _format) = parse_share(&share)?;
    let endpoint = &core.endpoint;
    let store = &core.store;
    let enabled = state.lock().enabled;
    ensure!(enabled, "P2P capabilities are disabled in Settings");

    // Connect to the owner over iroh. The ticket carries the owner's address
    // (relay + direct), so hole punching / relays make this work across the
    // internet.
    let conn = endpoint
        .connect(owner_addr.clone(), BLOBS_ALPN)
        .await
        .context("connect to playlist owner")?;

    // Fetch the manifest blob from the owner. iroh-blobs verifies every BLAKE3
    // chunk hash as it arrives, so a malicious owner cannot serve tampered
    // bytes.
    let remote = store.remote();
    let content = HashAndFormat::new(manifest_hash, BlobFormat::Raw);
    let stats = remote
        .fetch(conn.clone(), content)
        .complete()
        .await
        .context("fetch manifest")?;

    let _ = stats; // manifest fetch stats; kept for future progress reporting

    // Read the manifest back out of the local store.
    let manifest_bytes = store
        .get_bytes(manifest_hash)
        .await
        .context("read manifest from store")?;
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).context("manifest is not valid JSON")?;

    // Security: the manifest bytes are guaranteed to match the ticket's hash by
    // BLAKE3, and the signature proves the original owner authored them. So any
    // node that serves this manifest (owner or a syncing peer) is trusted, and
    // the recorded owner is `manifest.owner`, not necessarily who we dialled.
    verify_manifest_signature(&manifest)?;

    let max_gb = state.lock().max_size_gb;
    let total_known: u64 = manifest.songs.iter().map(|s| s.size).sum();
    let mut total_size: u64 = 0;
    let mut cached_tracks = Vec::new();

    let dir = P2pManager::playlist_dir(share_root, playlist_id);
    std::fs::create_dir_all(&dir)?;

    let started = std::time::Instant::now();
    for song in manifest.songs.iter() {
        let hash = song
            .hash
            .parse::<Hash>()
            .context("invalid song hash in manifest")?;
        remote
            .fetch(conn.clone(), HashAndFormat::new(hash, BlobFormat::Raw))
            .complete()
            .await
            .with_context(|| format!("fetch song {}", song.name))?;

        total_size += song.size;
        let speed_bps = total_size as f64 / started.elapsed().as_secs_f64().max(0.001);

        ensure!(
            playlist_size_cap_ok(total_size, max_gb),
            "playlist exceeds the {max_gb} GB P2P size limit"
        );

        let _ = event_tx
            .send(P2pEvent::SyncProgress {
                playlist_id,
                bytes_synced: total_size,
                total_bytes: total_known.max(1),
                speed_bps,
            })
            .await;

        // Export the verified blob into the isolated playlist directory. The
        // file name is the hash plus a safe extension, preventing path
        // traversal or overwriting.
        let out_path = dir.join(cached_file_name(&song.hash, &song.mime));
        store
            .export(hash, &out_path)
            .finish()
            .await
            .context("export song to isolated dir")?;

        // Reject anything that is not real audio before caching it locally.
        validate_audio_file(&out_path, DEFAULT_MAX_FILE_SIZE_BYTES)
            .with_context(|| format!("validating received song {}", song.name))?;
        let (title, artist, album) = read_local_metadata(&out_path, &song.name);
        cached_tracks.push((
            out_path.to_string_lossy().to_string(),
            song.hash.clone(),
            title,
            artist,
            album,
            song.duration,
            song.size,
            song.mime.clone(),
        ));
    }

    // Mark synced and record the peer.
    {
        let mut s = state.lock();
        s.shares.insert(
            playlist_id,
            ShareInfo {
                owner: false,
                seeding: true,
                manifest_hash: Some(manifest_hash.to_string()),
                owner_node_id: Some(manifest.owner.clone()),
                total_size,
                synced: true,
                ..Default::default()
            },
        );
    }
    let db_path = p2p_db_path(p2p_dir);
    let conn = rusqlite::Connection::open(&db_path)
        .with_context(|| format!("open library db {:?}", db_path))?;
    db::ensure_p2p_schema(&conn)?;
    db::record_p2p_share(
        &conn,
        playlist_id,
        &manifest.owner,
        &manifest_hash.to_string(),
        total_size,
        manifest.songs.len() as u64,
    )?;
    db::replace_p2p_tracks(&conn, playlist_id, &cached_tracks)?;
    db::record_p2p_peer(&conn, playlist_id, &owner_addr.id.to_string())?;
    drop(conn);

    let _ = event_tx
        .send(P2pEvent::SyncCompleted {
            playlist_id,
            ok: true,
        })
        .await;

    Ok(())
}

// ---------------------------------------------------------------------------
// Leaving a playlist
// ---------------------------------------------------------------------------

async fn leave_impl(
    state: &Arc<Mutex<SharedState>>,
    p2p_dir: &Path,
    share_root: &Path,
    playlist_id: u64,
) -> Result<()> {
    // Delete the synced files for this playlist.
    let dir = P2pManager::playlist_dir(share_root, playlist_id);
    let _ = std::fs::remove_dir_all(&dir);
    // Drop its cache rows so a later rejoin starts clean.
    let conn = rusqlite::Connection::open(p2p_db_path(p2p_dir))?;
    db::ensure_p2p_schema(&conn)?;
    db::remove_p2p_playlist(&conn, playlist_id)?;

    let mut s = state.lock();
    s.shares.remove(&playlist_id);
    Ok(())
}

/// A parseable share string used in tests (and future non-networking UI
/// checks). The encoded endpoint id and hash are deterministic.
#[cfg(any(test, feature = "test-support"))]
pub fn sample_share() -> String {
    let key = SecretKey::from_bytes(&[7u8; 32]);
    let addr = EndpointAddr::new(key.public());
    encode_share(addr, Hash::new(b"omamusic-test-manifest"))
}

/// Re-publish an already-synced playlist's manifest from this node.
///
/// The manifest bytes (and the original owner's signature over them) are
/// unchanged and already in our content-addressed store, so we can serve them
/// verbatim. The new ticket points at *us*, letting other peers pull from us
/// even when the original owner is offline.
async fn reshare_impl(
    core: &Core,
    state: &Arc<Mutex<SharedState>>,
    p2p_dir: &Path,
    playlist_id: u64,
) -> Result<String> {
    let conn = rusqlite::Connection::open(p2p_db_path(p2p_dir))?;
    db::ensure_p2p_schema(&conn)?;
    let record = db::load_p2p_share(&conn, playlist_id)?
        .ok_or_else(|| anyhow!("playlist is not a synced share"))?;
    let hash = record
        .manifest_hash
        .parse::<Hash>()
        .context("invalid cached manifest hash")?;
    let share = encode_share(core.endpoint.addr(), hash);
    {
        let mut s = state.lock();
        let mut entry = s.shares.get(&playlist_id).cloned().unwrap_or_default();
        entry.share = Some(share.clone());
        entry.owner_node_id = Some(record.owner_node_id.clone());
        entry.manifest_hash = Some(record.manifest_hash.clone());
        entry.total_size = record.total_size;
        entry.seeded_bytes = record.seeded_bytes;
        entry.seeding = true;
        entry.synced = true;
        s.shares.insert(playlist_id, entry);
    }
    // Keep the advertised share persisted with the running seeded total.
    let _ = db::record_p2p_seed_stats(&conn, playlist_id, record.seeded_bytes, &share);
    Ok(share)
}

/// Current nanoseconds since the epoch, for timestamps.
fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_addr() -> iroh::EndpointAddr {
        let key = iroh::SecretKey::generate().public();
        iroh::EndpointAddr::new(key)
    }

    #[test]
    fn magic_bytes_recognize_audio_containers() {
        assert!(magic_is_audio(b"ID3\x03\x04\x00"));
        assert!(magic_is_audio(&[0xFF, 0xF3, 0x00, 0x00])); // MPEG
        assert!(magic_is_audio(b"fLaC\x00\x00\x00\x00"));
        assert!(magic_is_audio(b"OggS\x00\x00\x00\x00"));
        assert!(magic_is_audio(&[0, 0, 0, 0, b'f', b't', b'y', b'p'])); // MP4
        assert!(magic_is_audio(b"RIFF\x00\x00\x00\x00WAVE"));
        assert!(magic_is_audio(b"ADIF\x00\x00\x00\x00"));
        assert!(magic_is_audio(&[0xFF, 0xF0, 0x00, 0x00])); // AAC ADTS
    }

    #[test]
    fn magic_bytes_reject_non_audio() {
        assert!(!magic_is_audio(b""));
        assert!(!magic_is_audio(b"abc"));
        assert!(!magic_is_audio(b"PK\x03\x04\x00\x00")); // zip
        assert!(!magic_is_audio(b"\x7fELF\x00\x00\x00\x00")); // elf
        assert!(!magic_is_audio(b"this is plain text, not audio at all"));
    }

    #[test]
    fn validate_audio_file_rejects_non_audio_and_renamed_files() {
        let dir = std::env::temp_dir().join(format!("omamusic-p2p-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let text = dir.join("song.mp3");
        std::fs::write(&text, b"this is not an audio file, just text").unwrap();
        // Wrong magic bytes for the extension.
        assert!(validate_audio_file(&text, DEFAULT_MAX_FILE_SIZE_BYTES).is_err());
        // Empty file.
        let empty = dir.join("empty.flac");
        std::fs::write(&empty, b"").unwrap();
        assert!(validate_audio_file(&empty, DEFAULT_MAX_FILE_SIZE_BYTES).is_err());
        // Too large.
        let big = dir.join("big.mp3");
        std::fs::write(&big, vec![0u8; DEFAULT_MAX_FILE_SIZE_BYTES as usize + 1]).unwrap();
        assert!(validate_audio_file(&big, DEFAULT_MAX_FILE_SIZE_BYTES).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn share_string_round_trip() {
        let addr = make_addr();
        let hash = Hash::new(b"manifest-content");
        let share = encode_share(addr.clone(), hash);
        assert!(share.starts_with(P2P_PREFIX));
        let (parsed_hash, parsed_addr, _format) = parse_share(&share).unwrap();
        assert_eq!(parsed_hash, hash);
        assert_eq!(parsed_addr.id, addr.id);
        // Raw ticket without prefix also parses.
        let ticket = share.strip_prefix(P2P_PREFIX).unwrap();
        let (parsed_hash, parsed_addr, _format) = parse_share(ticket).unwrap();
        assert_eq!(parsed_hash, hash);
        assert_eq!(parsed_addr.id, addr.id);
    }

    #[test]
    fn share_string_rejects_garbage() {
        assert!(parse_share("not-a-share").is_err());
        assert!(parse_share("omamusic://notavalidticket!!!").is_err());
        assert!(parse_share("").is_err());
    }

    #[test]
    fn format_size_reads_in_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(1500), "1.5 KB");
        assert_eq!(format_size(1_500_000), "1.4 MB");
        assert_eq!(format_size(1_500_000_000), "1.4 GB");
    }

    #[test]
    fn format_speed_reads_in_mb_s() {
        assert_eq!(format_speed(0.0), "0 B/s");
        assert_eq!(format_speed(500.0), "500 B/s");
        assert_eq!(format_speed(2_500.0), "2.5 KB/s");
        assert_eq!(format_speed(2_500_000.0), "2.5 MB/s");
    }

    #[test]
    fn playlist_size_cap_enforced() {
        assert!(playlist_size_cap_ok(0, 1));
        assert!(playlist_size_cap_ok(1024 * 1024 * 1024, 1));
        assert!(!playlist_size_cap_ok(1024 * 1024 * 1024 + 1, 1));
        assert!(playlist_size_cap_ok(2 * 1024 * 1024 * 1024, 2));
    }

    fn signed_manifest(secret: &SecretKey) -> Manifest {
        let owner = secret.public().to_string();
        let songs = vec![SongEntry {
            name: "a.mp3".into(),
            hash: Hash::new(b"a").to_hex(),
            size: 100,
            mime: "audio/mpeg".into(),
            duration: 1.0,
        }];
        let preimage = manifest_preimage(1, &owner, 42, &songs).unwrap();
        let signature = signature_hex_of(&secret.sign(&preimage).to_bytes());
        Manifest {
            version: 1,
            owner,
            created_at: 42,
            songs,
            signature,
        }
    }

    #[test]
    fn manifest_signature_verifies_for_the_owner() {
        let secret = SecretKey::from_bytes(&[9u8; 32]);
        let manifest = signed_manifest(&secret);
        assert!(verify_manifest_signature(&manifest).is_ok());
    }

    #[test]
    fn manifest_signature_rejects_tampering_and_forgery() {
        // Tampered content fails verification.
        let secret = SecretKey::from_bytes(&[9u8; 32]);
        let mut manifest = signed_manifest(&secret);
        manifest.songs[0].size = 999;
        assert!(
            verify_manifest_signature(&manifest).is_err(),
            "a modified manifest must not verify"
        );

        // A manifest signed by a different key than it claims fails too.
        let attacker = SecretKey::from_bytes(&[1u8; 32]);
        let victim = SecretKey::from_bytes(&[2u8; 32]);
        let mut forged = signed_manifest(&attacker);
        forged.owner = victim.public().to_string();
        assert!(
            verify_manifest_signature(&forged).is_err(),
            "a attacker cannot reassign a manifest to another node"
        );
    }

    #[test]
    fn cache_file_name_uses_hash_and_real_extension() {
        let hex = "ab".repeat(32);
        assert_eq!(cached_file_name(&hex, "audio/mpeg"), format!("{hex}.mp3"));
        assert!(cached_file_name(&hex, "audio/ogg").ends_with(".ogg"));
        // Unknown mimetype falls back to `.audio`, never a path separator.
        assert!(cached_file_name(&hex, "").ends_with(".audio"));
    }

    #[test]
    fn sanitized_names_never_contain_traversal_or_separators() {
        assert_eq!(sanitize_file_name("normal.mp3"), "normal.mp3");
        assert_eq!(sanitize_file_name("   "), "track");
        assert_eq!(sanitize_file_name("...hidden"), "hidden");
        let nasty = sanitize_file_name("../../etc/passwd");
        // No separators means `dest_dir.join(nasty)` cannot escape the folder.
        assert!(!nasty.contains('/'), "{nasty}");
        assert!(!nasty.contains('\\'), "{nasty}");
        assert!(!nasty.is_empty());
        let weird = sanitize_file_name("a/b\\c*d?e|");
        assert!(!weird.contains('/'));
        assert!(!weird.contains('\\'));
    }

    #[test]
    fn sample_share_parses_and_exposes_the_owner_node() {
        let share = sample_share();
        let (hash, addr, _format) = parse_share(&share).unwrap();
        assert_eq!(hash, Hash::new(b"omamusic-test-manifest"));
        assert!(!addr.id.to_string().is_empty());
    }
}
