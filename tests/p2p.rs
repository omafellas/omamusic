//! End-to-end P2P tests that spin up two `P2pManager`s in-process and make
//! one seed a real audio file and the other join and download it over iroh.
//! These exercise the whole path (import → share → connect → fetch → verify →
//! export → SQLite cache). They bind UDP sockets and talk over loopback, so
//! they need a working network stack but no external relay reachability.
use omamusic::db::{ensure_p2p_schema, list_p2p_tracks};
use omamusic::p2p::{p2p_db_path, validate_audio_file, P2pManager};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static SEQ: AtomicUsize = AtomicUsize::new(0);

fn tmp(label: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "omamusic-p2p-e2e-{label}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write a short silent, valid PCM WAV that `lofty` can parse.
fn write_wav(path: &Path, seconds: u32) {
    let samples = 8_000 * seconds;
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + samples * 2).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&8_000u32.to_le_bytes()); // rate
    wav.extend_from_slice(&(8_000u32 * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(samples * 2).to_le_bytes());
    wav.resize(wav.len() + samples as usize * 2, 0);
    std::fs::write(path, wav).unwrap();
}

fn fresh_cache() -> (P2pManager, PathBuf, PathBuf) {
    let base = tmp("node");
    let cache = base.join("cache");
    let share = base.join("music").join("shared");
    let mgr = P2pManager::new(&cache, &share, true, 5).expect("manager");
    (mgr, cache, share)
}

/// Poll a manager's events until `done` is satisfied, surfacing any
/// `ShareFailed`. Returns once per 50 ms up to `limit`.
fn drain_until<T>(
    mgr: &P2pManager,
    limit: Duration,
    mut pick: impl FnMut(&omamusic::p2p::P2pEvent) -> Option<T>,
) -> T {
    let start = Instant::now();
    loop {
        for ev in mgr.try_events() {
            if let omamusic::p2p::P2pEvent::ShareFailed { reason, .. } = &ev {
                panic!("share failed: {reason}");
            }
            if let Some(found) = pick(&ev) {
                return found;
            }
        }
        assert!(start.elapsed() < limit, "timed out waiting for a P2P event");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// These need a loopback-capable network stack; mark them ignored on hosts that
// sandbox UDP entirely, and run them explicitly in CI with `--ignored`.
#[test]
#[ignore = "binds UDP + talks over loopback; run with: cargo test --test p2p -- --ignored"]
fn owner_seeds_and_peer_joins_and_downloads() {
    let song = tmp("song").join("tone.wav");
    write_wav(&song, 2);

    let (owner, owner_cache, _owner_share) = fresh_cache();
    let (peer, peer_cache, peer_share) = fresh_cache();

    // Owner publishes the playlist.
    owner.create_share(7, vec![song.clone()]);
    let share: String = drain_until(&owner, Duration::from_secs(30), |ev| match ev {
        omamusic::p2p::P2pEvent::ShareCreated { share, .. } => Some(share.clone()),
        _ => None,
    });
    assert!(share.starts_with("omamusic://"), "share has the prefix");
    assert!(
        owner.is_seeding(7),
        "the owner keeps seeding after sharing, with nothing playing"
    );

    // Peer joins and downloads.
    peer.join_share(3, share.clone());
    drain_until(&peer, Duration::from_secs(30), |ev| match ev {
        omamusic::p2p::P2pEvent::SyncCompleted { ok: true, .. } => Some(()),
        _ => None,
    });

    // The peer cached exactly one track that points at a real, valid audio
    // file on disk in its share root.
    let conn = rusqlite::Connection::open(p2p_db_path(&peer_cache.join("p2p"))).unwrap();
    ensure_p2p_schema(&conn).unwrap();
    let tracks = list_p2p_tracks(&conn, 3).unwrap();
    assert_eq!(tracks.len(), 1, "peer cached one synced track");
    let local = PathBuf::from(&tracks[0].0);
    assert!(local.exists(), "synced file is on disk");
    assert!(
        local.starts_with(&peer_share),
        "synced file lives in the peer's share folder"
    );
    let info = validate_audio_file(&local, 500 * 1024 * 1024).expect("synced file is valid audio");
    assert!(info.duration > 0.0, "downloaded file has real duration");

    // The owner's cache DB also has the seeded track.
    let owner_conn = rusqlite::Connection::open(p2p_db_path(&owner_cache.join("p2p"))).unwrap();
    ensure_p2p_schema(&owner_conn).unwrap();
    let owner_tracks = list_p2p_tracks(&owner_conn, 7).unwrap();
    assert_eq!(owner_tracks.len(), 1);
    // Both sides hashed the same content, so the manifest/track agree.
    assert_eq!(owner_tracks[0].1, tracks[0].1, "same content hash");
}

#[test]
#[ignore = "binds UDP + talks over loopback; run with: cargo test --test p2p -- --ignored"]
fn peer_to_peer_transfer_works_without_the_owner() {
    let song = tmp("song").join("tone.wav");
    write_wav(&song, 2);

    let (owner, _oc, _os) = fresh_cache();
    let (peer_a, _ac, _a_share) = fresh_cache();
    let (peer_b, _bc, b_share) = fresh_cache();

    // Owner seeds; peer A joins and syncs.
    owner.create_share(1, vec![song.clone()]);
    let owner_ticket: String = drain_until(&owner, Duration::from_secs(30), |ev| match ev {
        omamusic::p2p::P2pEvent::ShareCreated { share, .. } => Some(share.clone()),
        _ => None,
    });
    peer_a.join_share(10, owner_ticket);
    drain_until(&peer_a, Duration::from_secs(30), |ev| match ev {
        omamusic::p2p::P2pEvent::SyncCompleted { ok: true, .. } => Some(()),
        _ => None,
    });

    // Peer A re-publishes the same signed manifest from itself; only then does
    // it advertise a ticket and count as seeding.
    peer_a.reshare(10);
    let a_ticket: String = drain_until(&peer_a, Duration::from_secs(30), |ev| match ev {
        omamusic::p2p::P2pEvent::ShareCreated { share, .. } => Some(share.clone()),
        _ => None,
    });
    assert!(peer_a.is_seeding(10), "a re-sharing peer seeds what it has");

    // The owner leaves; peer B joins using peer A's ticket and must still get
    // the file, served peer-to-peer.
    owner.leave(1);
    drain_until(&owner, Duration::from_secs(20), |ev| match ev {
        omamusic::p2p::P2pEvent::Left { .. } => Some(()),
        _ => None,
    });

    peer_b.join_share(20, a_ticket);
    drain_until(&peer_b, Duration::from_secs(30), |ev| match ev {
        omamusic::p2p::P2pEvent::SyncCompleted { ok: true, .. } => Some(()),
        _ => None,
    });

    // B now has the file on disk in its share folder, validated as audio.
    let dir = P2pManager::playlist_dir(&b_share, 20);
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(files.len(), 1, "peer B received one synced file");
    let got = files[0].path();
    assert!(
        validate_audio_file(&got, 500 * 1024 * 1024).is_ok(),
        "B's file is valid audio"
    );
}
