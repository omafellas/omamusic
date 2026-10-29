//! Writing fetched covers and tags: into the cache directory, and into the
//! audio files themselves.

use anyhow::{Context, Result};
use lofty::config::{ParseOptions, ParsingMode, WriteOptions};
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::ItemKey;
use lofty::tag::{Accessor, Tag};
use sha2::{Digest, Sha256};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

const TEMP_SUFFIX: &str = ".omamusic-tmp";

/// Values to write into one track. `None` fields are left alone, and a field
/// that already has a value in the file is never replaced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackFill<'a> {
    pub title: Option<&'a str>,
    pub artist: Option<&'a str>,
    pub album: Option<&'a str>,
    pub year: Option<u32>,
    pub track_number: Option<u32>,
    pub release_group: Option<&'a str>,
    pub release: Option<&'a str>,
    pub recording: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Written,
    /// Every requested field already had a value; the file was not touched.
    Unchanged,
    /// The file or its directory is not writable; the file was not touched.
    ReadOnly,
}

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

pub fn save_cache_cover(cache_dir: &Path, album_key: &str, bytes: &[u8]) -> Result<PathBuf> {
    let path = cache_path(cache_dir, album_key);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, bytes).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Add the missing tag values and the cover to the file at `path`. The
/// rewrite goes through a temporary copy in the same directory that is
/// renamed over the original, so a crash leaves the original intact.
pub fn fill_file(path: &Path, fill: &TrackFill, cover: Option<&[u8]>) -> Result<WriteOutcome> {
    let options = ParseOptions::new().parsing_mode(ParsingMode::Relaxed);
    let mut tagged = lofty::probe::Probe::open(path)?.options(options).read()?;
    if tagged.primary_tag().is_none() {
        let tag_type = tagged.primary_tag_type();
        tagged.insert_tag(Tag::new(tag_type));
    }
    let Some(tag) = tagged.primary_tag_mut() else {
        return Ok(WriteOutcome::Unchanged);
    };
    if !apply(tag, fill, cover) {
        return Ok(WriteOutcome::Unchanged);
    }

    let temp = temp_path(path);
    let permissions = std::fs::metadata(path)?.permissions();
    if let Err(err) = std::fs::copy(path, &temp) {
        return read_only_or(err, &temp);
    }
    let saved = tagged
        .save_to_path(&temp, WriteOptions::default())
        .map_err(std::io::Error::other)
        .and_then(|()| std::fs::File::open(&temp)?.sync_all())
        .and_then(|()| std::fs::set_permissions(&temp, permissions))
        .and_then(|()| std::fs::rename(&temp, path));
    match saved {
        Ok(()) => Ok(WriteOutcome::Written),
        Err(err) => read_only_or(err, &temp),
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(TEMP_SUFFIX);
    path.with_file_name(name)
}

fn read_only_or(err: std::io::Error, temp: &Path) -> Result<WriteOutcome> {
    let _ = std::fs::remove_file(temp);
    if matches!(
        err.kind(),
        ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem
    ) {
        return Ok(WriteOutcome::ReadOnly);
    }
    Err(err.into())
}

/// Set each `Some` value whose slot is empty. Returns whether anything changed.
fn apply(tag: &mut Tag, fill: &TrackFill, cover: Option<&[u8]>) -> bool {
    let mut changed = false;
    let texts = [
        (ItemKey::TrackTitle, fill.title),
        (ItemKey::TrackArtist, fill.artist),
        (ItemKey::AlbumTitle, fill.album),
        (ItemKey::MusicBrainzReleaseGroupId, fill.release_group),
        (ItemKey::MusicBrainzReleaseId, fill.release),
        (ItemKey::MusicBrainzRecordingId, fill.recording),
    ];
    for (key, value) in texts {
        let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
            continue;
        };
        if tag
            .get_string(&key)
            .is_none_or(|current| current.trim().is_empty())
        {
            tag.insert_text(key, value.to_string());
            changed = true;
        }
    }
    if let Some(year) = fill.year.filter(|y| *y > 0) {
        if tag.year().is_none_or(|current| current == 0) {
            tag.set_year(year);
            changed = true;
        }
    }
    if let Some(number) = fill.track_number.filter(|n| *n > 0) {
        if tag.track().is_none_or(|current| current == 0) {
            tag.set_track(number);
            changed = true;
        }
    }
    if let Some(cover) = cover.filter(|c| !c.is_empty()) {
        if tag.pictures().is_empty() {
            let mime = match image::guess_format(cover) {
                Ok(image::ImageFormat::Png) => Some(MimeType::Png),
                Ok(image::ImageFormat::Jpeg) => Some(MimeType::Jpeg),
                _ => None,
            };
            tag.push_picture(Picture::new_unchecked(
                PictureType::CoverFront,
                mime,
                None,
                cover.to_vec(),
            ));
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{self, fixtures};
    use std::os::unix::fs::PermissionsExt;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("omamusic-write-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn png() -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_pixel(4, 4, image::Rgb([9, 9, 9]))
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

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

    #[test]
    fn save_cache_cover_creates_the_directory() {
        let temp = TempDir::new("cache");
        let path = save_cache_cover(&temp.0, "k", b"bytes").unwrap();
        assert_eq!(path, cache_path(&temp.0, "k"));
        assert_eq!(std::fs::read(&path).unwrap(), b"bytes");
    }

    #[test]
    fn fills_only_empty_fields_and_adds_cover_once() {
        use lofty::config::WriteOptions;
        use lofty::prelude::*;
        use lofty::tag::{Tag, TagType};

        let temp = TempDir::new("fill");
        let path = temp.0.join("song.wav");
        std::fs::write(&path, fixtures::wav_bytes(1)).unwrap();
        let mut tag = Tag::new(TagType::Id3v2);
        tag.set_title("Keep".into());
        tag.save_to_path(&path, WriteOptions::default()).unwrap();
        let before_mode = std::fs::metadata(&path).unwrap().permissions().mode();

        let fill = TrackFill {
            title: Some("New"),
            artist: Some("Exxótica"),
            album: Some("IV"),
            year: Some(2006),
            track_number: Some(7),
            release_group: Some("rg-1"),
            release: Some("rel-1"),
            recording: None,
        };
        assert_eq!(
            fill_file(&path, &fill, Some(&png())).unwrap(),
            WriteOutcome::Written
        );
        let meta = metadata::read(&path);
        assert_eq!(meta.title, "Keep", "existing values are never replaced");
        assert_eq!(meta.artist, "Exxótica");
        assert_eq!(meta.album, "IV");
        assert_eq!(meta.year, 2006);
        assert_eq!(meta.track_number, 7);
        assert_eq!(meta.mb_release_group, "rg-1");
        assert_eq!(meta.mb_release, "rel-1");
        assert_eq!(meta.artwork, Some(png()));
        assert!(
            (meta.duration - 1.0).abs() < 0.01,
            "audio survives the rewrite"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode(),
            before_mode
        );
        assert!(
            std::fs::read_dir(&temp.0).unwrap().count() == 1,
            "no temp file left behind"
        );

        let other = TrackFill {
            title: Some("Ignored"),
            artist: Some("Ignored"),
            ..TrackFill::default()
        };
        assert_eq!(
            fill_file(&path, &other, Some(b"other image")).unwrap(),
            WriteOutcome::Unchanged
        );
        assert_eq!(metadata::read(&path).artwork, Some(png()));
    }

    #[test]
    fn read_only_directory_is_skipped() {
        let temp = TempDir::new("ro");
        let path = temp.0.join("song.wav");
        let bytes = fixtures::wav_bytes(1);
        std::fs::write(&path, &bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::set_permissions(&temp.0, std::fs::Permissions::from_mode(0o555)).unwrap();

        let fill = TrackFill {
            title: Some("New"),
            ..TrackFill::default()
        };
        let outcome = fill_file(&path, &fill, None).unwrap();
        std::fs::set_permissions(&temp.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(outcome, WriteOutcome::ReadOnly);
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "file untouched");
        assert_eq!(std::fs::read_dir(&temp.0).unwrap().count(), 1);
    }

    #[test]
    fn nothing_to_fill_is_unchanged_without_touching_the_file() {
        let temp = TempDir::new("noop");
        let path = temp.0.join("song.wav");
        std::fs::write(&path, fixtures::wav_bytes(1)).unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(
            fill_file(&path, &TrackFill::default(), None).unwrap(),
            WriteOutcome::Unchanged
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
    }
}
