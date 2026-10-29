use anyhow::{anyhow, Context, Result};
use lofty::config::{ParseOptions, ParsingMode, WriteOptions};
use lofty::file::{AudioFile, FileType};
use lofty::flac::FlacFile;
use lofty::id3::v2::{Frame, Id3v2Tag, UniqueFileIdentifierFrame};
use lofty::iff::aiff::AiffFile;
use lofty::iff::wav::WavFile;
use lofty::mp4::Mp4File;
use lofty::mpeg::MpegFile;
use lofty::ogg::{OggPictureStorage, OpusFile, VorbisFile};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::ItemKey;
use lofty::tag::{Accessor, MergeTag, SplitTag, Tag};
use sha2::{Digest, Sha256};
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const TEMP_SUFFIX: &str = ".omamusic-tmp";

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
    Unchanged,
    ReadOnly,
    Linked,
}

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

pub fn fill_file(path: &Path, fill: &TrackFill, cover: Option<&[u8]>) -> Result<WriteOutcome> {
    let link_info = std::fs::symlink_metadata(path)?;
    if link_info.file_type().is_symlink() || link_info.nlink() > 1 {
        return Ok(WriteOutcome::Linked);
    }
    let Some(edited) = edit(path, fill, cover)? else {
        return Ok(WriteOutcome::Unchanged);
    };

    let temp = temp_path(path);
    let permissions = link_info.permissions();
    if let Err(err) = std::fs::copy(path, &temp) {
        return read_only_or(err, &temp);
    }
    let saved = edited
        .save(&temp)
        .map_err(std::io::Error::other)
        .and_then(|()| std::fs::File::open(&temp)?.sync_all())
        .and_then(|()| std::fs::set_permissions(&temp, permissions))
        .and_then(|()| std::fs::rename(&temp, path));
    match saved {
        Ok(()) => Ok(WriteOutcome::Written),
        Err(err) => read_only_or(err, &temp),
    }
}

enum Edited {
    Mpeg(MpegFile),
    Flac(FlacFile),
    Mp4(Mp4File),
    Vorbis(VorbisFile),
    Opus(OpusFile),
    Wav(WavFile),
    Aiff(AiffFile),
}

impl Edited {
    fn save(&self, path: &Path) -> lofty::error::Result<()> {
        let options = WriteOptions::default();
        match self {
            Edited::Mpeg(f) => f.save_to_path(path, options),
            Edited::Flac(f) => f.save_to_path(path, options),
            Edited::Mp4(f) => f.save_to_path(path, options),
            Edited::Vorbis(f) => f.save_to_path(path, options),
            Edited::Opus(f) => f.save_to_path(path, options),
            Edited::Wav(f) => f.save_to_path(path, options),
            Edited::Aiff(f) => f.save_to_path(path, options),
        }
    }
}

fn through<T: SplitTag>(
    tag: T,
    fill: &TrackFill,
    cover: Option<&[u8]>,
    has_picture: bool,
) -> (<T::Remainder as MergeTag>::Merged, bool) {
    let (remainder, mut generic) = tag.split_tag();
    let changed = apply(&mut generic, fill, cover, has_picture);
    (remainder.merge_tag(generic), changed)
}

/// lofty does not create a `UFID` frame from a generic recording id, so ID3
/// tags get the MusicBrainz identifier frame added by hand.
/// Workaround for lofty 0.22; the read side already understands the frame.
fn ensure_recording_frame(tag: &mut Id3v2Tag, recording: Option<&str>) -> bool {
    let Some(recording) = recording.filter(|r| !r.is_empty() && r.is_ascii()) else {
        return false;
    };
    let owner = "http://musicbrainz.org";
    let present = (&*tag)
        .into_iter()
        .any(|frame| matches!(frame, Frame::UniqueFileIdentifier(ufid) if ufid.owner == owner));
    if present {
        return false;
    }
    tag.insert(Frame::UniqueFileIdentifier(UniqueFileIdentifierFrame::new(
        owner.to_string(),
        recording.as_bytes().to_vec(),
    )));
    true
}

fn edit(path: &Path, fill: &TrackFill, cover: Option<&[u8]>) -> Result<Option<Edited>> {
    let options = ParseOptions::new().parsing_mode(ParsingMode::Relaxed);
    let file_type = lofty::probe::Probe::open(path)?
        .guess_file_type()?
        .file_type()
        .ok_or_else(|| anyhow!("unsupported audio format"))?;
    let mut file = std::fs::File::open(path)?;
    let (edited, changed) = match file_type {
        FileType::Mpeg => {
            let mut audio = MpegFile::read_from(&mut file, options)?;
            let (mut tag, mut changed) =
                through(audio.remove_id3v2().unwrap_or_default(), fill, cover, false);
            changed |= ensure_recording_frame(&mut tag, fill.recording);
            audio.set_id3v2(tag);
            (Edited::Mpeg(audio), changed)
        }
        FileType::Wav => {
            let mut audio = WavFile::read_from(&mut file, options)?;
            let (mut tag, mut changed) =
                through(audio.remove_id3v2().unwrap_or_default(), fill, cover, false);
            changed |= ensure_recording_frame(&mut tag, fill.recording);
            audio.set_id3v2(tag);
            (Edited::Wav(audio), changed)
        }
        FileType::Aiff => {
            let mut audio = AiffFile::read_from(&mut file, options)?;
            let (mut tag, mut changed) =
                through(audio.remove_id3v2().unwrap_or_default(), fill, cover, false);
            changed |= ensure_recording_frame(&mut tag, fill.recording);
            audio.set_id3v2(tag);
            (Edited::Aiff(audio), changed)
        }
        FileType::Flac => {
            let mut audio = FlacFile::read_from(&mut file, options)?;
            let has_picture = !audio.pictures().is_empty();
            let (tag, changed) = through(
                audio.remove_vorbis_comments().unwrap_or_default(),
                fill,
                cover,
                has_picture,
            );
            audio.set_vorbis_comments(tag);
            (Edited::Flac(audio), changed)
        }
        FileType::Mp4 => {
            let mut audio = Mp4File::read_from(&mut file, options)?;
            let (tag, changed) =
                through(audio.remove_ilst().unwrap_or_default(), fill, cover, false);
            audio.set_ilst(tag);
            (Edited::Mp4(audio), changed)
        }
        FileType::Vorbis => {
            let mut audio = VorbisFile::read_from(&mut file, options)?;
            let (tag, changed) = through(
                std::mem::take(audio.vorbis_comments_mut()),
                fill,
                cover,
                false,
            );
            *audio.vorbis_comments_mut() = tag;
            (Edited::Vorbis(audio), changed)
        }
        FileType::Opus => {
            let mut audio = OpusFile::read_from(&mut file, options)?;
            let (tag, changed) = through(
                std::mem::take(audio.vorbis_comments_mut()),
                fill,
                cover,
                false,
            );
            *audio.vorbis_comments_mut() = tag;
            (Edited::Opus(audio), changed)
        }
        other => return Err(anyhow!("unsupported audio format {other:?}")),
    };
    Ok(changed.then_some(edited))
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

fn apply(tag: &mut Tag, fill: &TrackFill, cover: Option<&[u8]>, has_picture: bool) -> bool {
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
        if tag.pictures().is_empty() && !has_picture {
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
            recording: Some("rec-9"),
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
        assert_eq!(
            meta.mb_recording, "rec-9",
            "new recording ids land in a UFID frame"
        );
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
    fn keeps_frames_lofty_cannot_map() {
        let temp = TempDir::new("frames");
        let path = temp.0.join("song.mp3");
        std::fs::write(
            &path,
            fixtures::mp3_bytes(&[
                (b"TIT2", b"\x00Keep"),
                (b"UFID", b"http://musicbrainz.org\x00rec-1"),
                (b"TIPL", b"\x00producer\x00Someone\x00"),
                (b"PRIV", b"owner\x00payload"),
            ]),
        )
        .unwrap();
        let fill = TrackFill {
            artist: Some("Exxótica"),
            release_group: Some("rg-1"),
            ..TrackFill::default()
        };
        assert_eq!(
            fill_file(&path, &fill, Some(&png())).unwrap(),
            WriteOutcome::Written
        );
        let bytes = std::fs::read(&path).unwrap();
        for frame in [&b"TIPL"[..], b"PRIV", b"UFID", b"APIC"] {
            assert!(
                bytes.windows(4).any(|w| w == frame),
                "{} frame must survive the rewrite",
                String::from_utf8_lossy(frame)
            );
        }
        let meta = metadata::read(&path);
        assert_eq!(meta.title, "Keep");
        assert_eq!(meta.artist, "Exxótica");
        assert_eq!(meta.mb_recording, "rec-1");
        assert_eq!(meta.mb_release_group, "rg-1");
        assert_eq!(meta.artwork, Some(png()));
    }

    #[test]
    fn symlinked_file_is_left_alone() {
        let temp = TempDir::new("symlink");
        let target = temp.0.join("real.wav");
        std::fs::write(&target, fixtures::wav_bytes(1)).unwrap();
        let link = temp.0.join("link.wav");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let fill = TrackFill {
            title: Some("New"),
            ..TrackFill::default()
        };
        assert_eq!(fill_file(&link, &fill, None).unwrap(), WriteOutcome::Linked);
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(metadata::read(&target).title, "");
    }

    #[test]
    fn hard_linked_file_is_left_alone() {
        let temp = TempDir::new("hardlink");
        let one = temp.0.join("one.wav");
        std::fs::write(&one, fixtures::wav_bytes(1)).unwrap();
        let two = temp.0.join("two.wav");
        std::fs::hard_link(&one, &two).unwrap();
        let fill = TrackFill {
            title: Some("New"),
            ..TrackFill::default()
        };
        assert_eq!(fill_file(&one, &fill, None).unwrap(), WriteOutcome::Linked);
        assert_eq!(metadata::read(&two).title, "");
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
