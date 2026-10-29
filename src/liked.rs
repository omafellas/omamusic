use crate::config::AppConfig;
use std::path::{Path, PathBuf};

/// Songs marked with the heart, most recently liked first.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Liked {
    paths: Vec<PathBuf>,
}

impl Liked {
    pub fn path() -> PathBuf {
        AppConfig::path().with_file_name("liked.json")
    }

    pub fn load() -> Self {
        let paths = std::fs::read(Self::path())
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default();
        Self { paths }
    }

    pub fn save(&self) {
        let path = Self::path();
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| {
                let data =
                    serde_json::to_string_pretty(&self.paths).map_err(std::io::Error::other)?;
                std::fs::write(&path, data)
            });
        if let Err(err) = result {
            eprintln!("save liked songs: {err}");
        }
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.paths.iter().any(|p| p == path)
    }

    /// Likes the song, or unlikes it when already liked. Returns whether it
    /// is liked afterwards.
    pub fn toggle(&mut self, path: &Path) -> bool {
        if self.contains(path) {
            self.paths.retain(|p| p != path);
            false
        } else {
            self.paths.insert(0, path.to_path_buf());
            true
        }
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggling_likes_newest_first_and_unlikes() {
        let mut liked = Liked::default();
        assert!(liked.toggle(Path::new("/a.flac")));
        assert!(liked.toggle(Path::new("/b.flac")));
        assert_eq!(
            liked.paths(),
            [PathBuf::from("/b.flac"), PathBuf::from("/a.flac")]
        );
        assert!(!liked.toggle(Path::new("/b.flac")));
        assert!(!liked.contains(Path::new("/b.flac")));
        assert_eq!(liked.paths(), [PathBuf::from("/a.flac")]);
    }
}
