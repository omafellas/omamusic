use anyhow::Result;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const AUDIO_EXTENSIONS: [&str; 9] = [
    "mp3", "flac", "ogg", "m4a", "mp4", "aac", "wav", "aiff", "caf",
];

#[derive(Debug, Default, Clone)]
pub struct AudioMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub track_number: u32,
    pub year: u32,
    pub duration: f64,
    pub artwork: Option<Vec<u8>>,
}

/// macOS leaves AppleDouble metadata files (`._*`) and `__MACOSX` folders
/// behind when a zip is extracted. They often reuse audio extensions, so they
/// must be rejected explicitly instead of being indexed as songs.
pub fn is_macos_junk(path: &Path) -> bool {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("._"))
    {
        return true;
    }
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case("__MACOSX"))
    })
}

pub fn is_audio(path: &Path) -> bool {
    if is_macos_junk(path) {
        return false;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
}

/// Read tags, duration and embedded cover art. Any unreadable or untagged
/// file yields an empty `AudioMetadata`; the indexer fills in names from the
/// path.
pub fn read(path: &Path) -> AudioMetadata {
    read_tagged(path).unwrap_or_default()
}

fn read_tagged(path: &Path) -> Result<AudioMetadata> {
    use lofty::config::{ParseOptions, ParsingMode};
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::picture::PictureType;
    use lofty::prelude::ItemKey;
    use lofty::tag::Accessor;

    let options = ParseOptions::new().parsing_mode(ParsingMode::Relaxed);
    let tagged = lofty::probe::Probe::open(path)?.options(options).read()?;
    let mut meta = AudioMetadata {
        duration: tagged.properties().duration().as_secs_f64(),
        ..AudioMetadata::default()
    };
    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        return Ok(meta);
    };

    let text = |key: ItemKey| {
        tag.get_string(&key)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    meta.title = text(ItemKey::TrackTitle);
    meta.artist = text(ItemKey::TrackArtist);
    meta.album = text(ItemKey::AlbumTitle);
    meta.genre = parse_genre(
        &tag.get_strings(&ItemKey::Genre)
            .collect::<Vec<_>>()
            .join("/"),
    );
    meta.track_number = tag
        .get_string(&ItemKey::TrackNumber)
        .map(parse_track)
        .filter(|n| *n > 0)
        .or_else(|| tag.track())
        .unwrap_or(0);
    meta.year = [
        ItemKey::RecordingDate,
        ItemKey::Year,
        ItemKey::OriginalReleaseDate,
    ]
    .into_iter()
    .filter_map(|key| tag.get_string(&key))
    .map(parse_year)
    .find(|year| *year > 0)
    .or_else(|| tag.year())
    .or_else(|| id3_date_fallback(path))
    .unwrap_or(0);
    let pictures = tag.pictures();
    meta.artwork = pictures
        .iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or(pictures.first())
        .map(|p| p.data().to_vec())
        .filter(|data| !data.is_empty());
    Ok(meta)
}

fn parse_year(text: &str) -> u32 {
    text.chars()
        .take(4)
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

fn parse_track(text: &str) -> u32 {
    text.split('/')
        .next()
        .unwrap_or("0")
        .trim()
        .parse()
        .unwrap_or(0)
}

/// ID3v1 genre names, including the Winamp extensions. ID3v2 `TCON` frames can
/// reference these by number, for example `(17)` or `(17)Rock`.
const ID3_GENRES: [&str; 192] = [
    "Blues",
    "Classic Rock",
    "Country",
    "Dance",
    "Disco",
    "Funk",
    "Grunge",
    "Hip-Hop",
    "Jazz",
    "Metal",
    "New Age",
    "Oldies",
    "Other",
    "Pop",
    "R&B",
    "Rap",
    "Reggae",
    "Rock",
    "Techno",
    "Industrial",
    "Alternative",
    "Ska",
    "Death Metal",
    "Pranks",
    "Soundtrack",
    "Euro-Techno",
    "Ambient",
    "Trip-Hop",
    "Vocal",
    "Jazz+Funk",
    "Fusion",
    "Trance",
    "Classical",
    "Instrumental",
    "Acid",
    "House",
    "Game",
    "Sound Clip",
    "Gospel",
    "Noise",
    "AlternRock",
    "Bass",
    "Soul",
    "Punk",
    "Space",
    "Meditative",
    "Instrumental Pop",
    "Instrumental Rock",
    "Ethnic",
    "Gothic",
    "Darkwave",
    "Techno-Industrial",
    "Electronic",
    "Pop-Folk",
    "Eurodance",
    "Dream",
    "Southern Rock",
    "Comedy",
    "Cult",
    "Gangsta",
    "Top 40",
    "Christian Rap",
    "Pop/Funk",
    "Jungle",
    "Native American",
    "Cabaret",
    "New Wave",
    "Psychedelic",
    "Rave",
    "Showtunes",
    "Trailer",
    "Lo-Fi",
    "Tribal",
    "Acid Punk",
    "Acid Jazz",
    "Polka",
    "Retro",
    "Musical",
    "Rock & Roll",
    "Hard Rock",
    "Folk",
    "Folk-Rock",
    "National Folk",
    "Swing",
    "Fast Fusion",
    "Bebob",
    "Latin",
    "Revival",
    "Celtic",
    "Bluegrass",
    "Avantgarde",
    "Gothic Rock",
    "Progressive Rock",
    "Psychedelic Rock",
    "Symphonic Rock",
    "Slow Rock",
    "Big Band",
    "Chorus",
    "Easy Listening",
    "Acoustic",
    "Humour",
    "Speech",
    "Chanson",
    "Opera",
    "Chamber Music",
    "Sonata",
    "Symphony",
    "Booty Bass",
    "Primus",
    "Porn Groove",
    "Satire",
    "Slow Jam",
    "Club",
    "Tango",
    "Samba",
    "Folklore",
    "Ballad",
    "Power Ballad",
    "Rhythmic Soul",
    "Freestyle",
    "Duet",
    "Punk Rock",
    "Drum Solo",
    "A Cappella",
    "Euro-House",
    "Dance Hall",
    "Goa",
    "Drum & Bass",
    "Club-House",
    "Hardcore Techno",
    "Terror",
    "Indie",
    "BritPop",
    "Afro-Punk",
    "Polsk Punk",
    "Beat",
    "Christian Gangsta Rap",
    "Heavy Metal",
    "Black Metal",
    "Crossover",
    "Contemporary Christian",
    "Christian Rock",
    "Merengue",
    "Salsa",
    "Thrash Metal",
    "Anime",
    "JPop",
    "Synthpop",
    "Abstract",
    "Art Rock",
    "Baroque",
    "Bhangra",
    "Big Beat",
    "Breakbeat",
    "Chillout",
    "Downtempo",
    "Dub",
    "EBM",
    "Eclectic",
    "Electro",
    "Electroclash",
    "Emo",
    "Experimental",
    "Garage",
    "Global",
    "IDM",
    "Illbient",
    "Industro-Goth",
    "Jam Band",
    "Krautrock",
    "Leftfield",
    "Lounge",
    "Math Rock",
    "New Romantic",
    "Nu-Breakz",
    "Post-Punk",
    "Post-Rock",
    "Psytrance",
    "Shoegaze",
    "Space Rock",
    "Trop Rock",
    "World Music",
    "Neoclassical",
    "Audiobook",
    "Audio Theatre",
    "Neue Deutsche Welle",
    "Podcast",
    "Indie Rock",
    "G-Funk",
    "Dubstep",
    "Garage Rock",
    "Psybient",
];

/// Normalise a genre tag. Handles plain text (`Black Metal`), ID3 numeric
/// references (`(22)`, `(22)Death Metal`) and multiple values, joining them
/// with `, `.
fn parse_genre(text: &str) -> String {
    let text = text.replace(")(", ")/(");
    let mut genres: Vec<String> = Vec::new();
    for part in text.split(['/', '\0']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let genre = match part.strip_prefix('(').and_then(|rest| rest.split_once(')')) {
            Some((_, tail)) if !tail.trim().is_empty() => tail.trim().to_string(),
            Some((number, _)) => number
                .trim()
                .parse::<usize>()
                .ok()
                .and_then(|index| ID3_GENRES.get(index))
                .map_or_else(|| part.to_string(), |genre| (*genre).to_string()),
            None => part.to_string(),
        };
        let genre = ID3_GENRES
            .iter()
            .find(|known| known.eq_ignore_ascii_case(&genre))
            .map_or(genre, |known| (*known).to_string());
        if !genres
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&genre))
        {
            genres.push(genre);
        }
    }
    genres.join(", ")
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().unwrap())
}

/// Year from a raw ID3v2 date frame when the tag library rejected it. Some
/// taggers write several null-separated values into `TYER` (`2002\02002`),
/// which is not a valid timestamp, so the whole frame is dropped upstream.
/// Workaround until lofty keeps malformed date frames as text.
fn id3_date_fallback(path: &Path) -> Option<u32> {
    let mut file = File::open(path).ok()?;
    let mut header = [0u8; 10];
    file.read_exact(&mut header).ok()?;
    if &header[0..3] != b"ID3" || !matches!(header[3], 3 | 4) {
        return None;
    }
    let version = header[3];
    let end = 10 + syncsafe(&header[6..10]) as u64;
    let mut offset: u64 = 10;
    if header[5] & 0x40 != 0 {
        let mut ext = [0u8; 4];
        file.read_exact(&mut ext).ok()?;
        offset += 4 + if version == 4 {
            syncsafe(&ext)
        } else {
            be_u32(&ext) as usize
        } as u64;
    }
    let mut frame_header = [0u8; 10];
    let mut fallback = None;
    while offset + 10 <= end {
        file.seek(SeekFrom::Start(offset)).ok()?;
        if file.read(&mut frame_header).ok()? < 10 || frame_header[0] == 0 {
            break;
        }
        let frame_size = if version == 4 {
            syncsafe(&frame_header[4..8])
        } else {
            be_u32(&frame_header[4..8]) as usize
        };
        if frame_size == 0 || frame_size > 4_000_000 {
            offset += 10;
            continue;
        }
        let id = &frame_header[0..4];
        if matches!(id, b"TYER" | b"TDRC" | b"TDOR" | b"TORY") {
            let mut data = vec![0u8; frame_size];
            file.read_exact(&mut data).ok()?;
            let year = parse_year(&id3_text(&data));
            if year > 0 {
                if matches!(id, b"TYER" | b"TDRC") {
                    return Some(year);
                }
                fallback.get_or_insert(year);
            }
        }
        offset += 10 + frame_size as u64;
    }
    fallback
}

pub fn folder_artwork(path: &Path) -> Option<Vec<u8>> {
    let dir = path.parent()?;
    for name in ["cover", "folder", "front", "album", "Cover", "Folder"] {
        for ext in ["jpg", "jpeg", "png", "webp"] {
            let cand = dir.join(format!("{name}.{ext}"));
            if let Ok(data) = std::fs::read(&cand) {
                if !data.is_empty() {
                    return Some(data);
                }
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
                if matches!(ext.to_lowercase().as_str(), "jpg" | "jpeg" | "png" | "webp") {
                    if let Ok(data) = std::fs::read(&p) {
                        if !data.is_empty() {
                            return Some(data);
                        }
                    }
                }
            }
        }
    }
    None
}

fn syncsafe(b: &[u8]) -> usize {
    b.iter().fold(0, |acc, &x| (acc << 7) | (x & 0x7F) as usize)
}

fn id3_text(data: &[u8]) -> String {
    if data.is_empty() {
        return String::new();
    }
    let (enc, text) = (data[0], &data[1..]);
    // ID3v2: 0 = latin1, 1 = UTF-16 with BOM (endianness follows the BOM),
    // 2 = UTF-16BE, 3 = UTF-8.
    let be = match enc {
        1 => !text.starts_with(&[0xFF, 0xFE]),
        2 => true,
        _ => {
            return String::from_utf8_lossy(text)
                .trim_matches('\0')
                .trim()
                .to_string();
        }
    };
    let text = text
        .strip_prefix(&[0xFF, 0xFE])
        .or_else(|| text.strip_prefix(&[0xFE, 0xFF]))
        .unwrap_or(text);
    let (chunks, _) = text.as_chunks::<2>();
    let u16s: Vec<u16> = chunks
        .iter()
        .map(|c| {
            if be {
                u16::from_be_bytes(*c)
            } else {
                u16::from_le_bytes(*c)
            }
        })
        .collect();
    String::from_utf16_lossy(&u16s)
        .trim_matches('\0')
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_extensions_are_recognized() {
        assert!(is_audio(Path::new("/m/song.mp3")));
        assert!(is_audio(Path::new("/m/song.FLAC")));
        assert!(!is_audio(Path::new("/m/cover.jpg")));
    }

    #[test]
    fn macos_appledouble_files_are_not_audio() {
        assert!(!is_audio(Path::new("/m/._song.mp3")));
        assert!(!is_audio(Path::new("/m/__MACOSX/Album/._song.mp3")));
        assert!(!is_audio(Path::new("/m/__MACOSX/Album/song.mp3")));
        assert!(!is_audio(Path::new("/m/__macosx/Album/song.mp3")));
        // A normal hidden file is still a song.
        assert!(is_audio(Path::new("/m/.hidden-song.mp3")));
    }

    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("omamusic-metadata-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir.join(name))
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Minimal PCM WAV: one second of silence at 8 kHz mono.
    fn wav_bytes() -> Vec<u8> {
        let samples = 8000u32 * 2;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + samples).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&8000u32.to_le_bytes());
        out.extend_from_slice(&16000u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&samples.to_le_bytes());
        out.resize(out.len() + samples as usize, 0);
        out
    }

    /// ID3v2.3 header plus the given frames, followed by four silent MPEG-1
    /// Layer III frames (128 kbit/s, 44.1 kHz, 417 bytes each).
    fn mp3_bytes(frames: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (id, data) in frames {
            body.extend_from_slice(*id);
            body.extend_from_slice(&(data.len() as u32).to_be_bytes());
            body.extend_from_slice(&[0, 0]);
            body.extend_from_slice(data);
        }
        let size = body.len() as u32;
        let mut out = b"ID3\x03\x00\x00".to_vec();
        out.extend_from_slice(&[
            ((size >> 21) & 0x7F) as u8,
            ((size >> 14) & 0x7F) as u8,
            ((size >> 7) & 0x7F) as u8,
            (size & 0x7F) as u8,
        ]);
        out.extend_from_slice(&body);
        for _ in 0..4 {
            out.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
            out.resize(out.len() + 413, 0);
        }
        out
    }

    #[test]
    fn reads_tags_and_cover_written_by_a_tagger() {
        use lofty::config::WriteOptions;
        use lofty::picture::{MimeType, Picture, PictureType};
        use lofty::prelude::*;
        use lofty::tag::{Tag, TagType};

        let file = TempFile::new("tagged.wav");
        std::fs::write(&file.0, wav_bytes()).unwrap();
        let mut tag = Tag::new(TagType::Id3v2);
        tag.set_title("Teu Olhar".into());
        tag.set_artist("Exxótica".into());
        tag.set_album("Estranhos no Ninho".into());
        tag.insert_text(ItemKey::Genre, "(22)".into());
        tag.insert_text(ItemKey::TrackNumber, "12/14".into());
        tag.insert_text(ItemKey::RecordingDate, "2002-06-01".into());
        tag.push_picture(Picture::new_unchecked(
            PictureType::CoverFront,
            Some(MimeType::Png),
            None,
            vec![1, 2, 3],
        ));
        tag.save_to_path(&file.0, WriteOptions::default()).unwrap();

        let meta = read(&file.0);
        assert_eq!(meta.title, "Teu Olhar");
        assert_eq!(meta.artist, "Exxótica");
        assert_eq!(meta.album, "Estranhos no Ninho");
        assert_eq!(meta.genre, "Death Metal");
        assert_eq!(meta.track_number, 12);
        assert_eq!(meta.year, 2002);
        assert_eq!(meta.artwork, Some(vec![1, 2, 3]));
        assert!((meta.duration - 1.0).abs() < 0.01, "{}", meta.duration);
    }

    #[test]
    fn malformed_multi_value_tyer_still_yields_a_year() {
        let file = TempFile::new("tyer.mp3");
        std::fs::write(
            &file.0,
            mp3_bytes(&[
                (b"TIT2", b"\x00No Palco"),
                (b"TYER", b"\x002002\x002002\x00"),
            ]),
        )
        .unwrap();
        let meta = read(&file.0);
        assert_eq!(meta.title, "No Palco");
        assert_eq!(meta.year, 2002);
        assert!(meta.duration > 0.0);
    }

    #[test]
    fn unreadable_file_yields_empty_metadata() {
        let file = TempFile::new("garbage.mp3");
        std::fs::write(&file.0, b"not audio").unwrap();
        assert_eq!(read(&file.0).title, "");
        assert_eq!(read(Path::new("/nonexistent/x.mp3")).duration, 0.0);
    }

    #[test]
    fn genre_tags_are_normalized() {
        assert_eq!(parse_genre("Black Metal"), "Black Metal");
        assert_eq!(parse_genre("  "), "");
        assert_eq!(parse_genre("(17)"), "Rock");
        assert_eq!(parse_genre("(17)Rock"), "Rock");
        assert_eq!(parse_genre("(22)"), "Death Metal");
        assert_eq!(parse_genre("(17)(22)"), "Rock, Death Metal");
        assert_eq!(parse_genre("Rock/Pop"), "Rock, Pop");
        assert_eq!(parse_genre("Rock\0Pop"), "Rock, Pop");
        assert_eq!(parse_genre("Rock/Rock"), "Rock");
        assert_eq!(parse_genre("Death metal"), "Death Metal");
        assert_eq!(parse_genre("Blackened Thrash"), "Blackened Thrash");
    }
}
