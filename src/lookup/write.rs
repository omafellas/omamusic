//! Writing fetched covers and tags: into the cache directory, and into the
//! audio files themselves.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Where a fetched cover for `album_key` is kept. The cache survives database
/// rebuilds, so a cover is fetched once per album.
pub fn cache_path(cache_dir: &Path, album_key: &str) -> PathBuf {
    let hash = Sha256::digest(album_key.as_bytes());
    let mut name = String::with_capacity(68);
    for byte in hash {
        name.push_str(&format!("{byte:02x}"));
    }
    name.push_str(".jpg");
    cache_dir.join("covers").join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_path_is_stable_and_hides_the_key() {
        let dir = Path::new("/cache");
        let a = cache_path(dir, "/music/x\nAlbum");
        assert_eq!(a, cache_path(dir, "/music/x\nAlbum"));
        assert_ne!(a, cache_path(dir, "/music/y\nAlbum"));
        assert!(a.starts_with("/cache/covers"));
        assert_eq!(a.extension().unwrap(), "jpg");
        assert_eq!(a.file_stem().unwrap().len(), 64);
    }
}
