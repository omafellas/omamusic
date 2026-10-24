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

pub fn read(path: &Path) -> AudioMetadata {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "mp3" => read_id3(path),
        "m4a" | "mp4" | "aac" => read_mp4(path),
        "flac" => read_flac(path),
        "ogg" | "opus" | "oga" => read_vorbis(path),
        _ => Ok(AudioMetadata::default()),
    }
    .unwrap_or_default()
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

fn le_u32(bytes: &[u8]) -> usize {
    u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize
}

fn read_id3(path: &Path) -> Result<AudioMetadata> {
    let mut file = File::open(path)?;
    let mut header = [0u8; 10];
    file.read_exact(&mut header)?;
    if &header[0..3] != b"ID3" {
        return Ok(AudioMetadata::default());
    }
    let version = header[3];
    if version != 3 && version != 4 {
        return Ok(AudioMetadata::default());
    }
    let flags = header[5];
    let size = syncsafe(&header[6..10]);
    let mut meta = AudioMetadata::default();
    let mut offset: u64 = 10;

    if flags & 0x40 != 0 {
        let mut ext = [0u8; 4];
        file.read_exact(&mut ext)?;
        let ext_size = if version == 4 {
            syncsafe(&ext)
        } else {
            be_u32(&ext) as usize
        };
        offset += ext_size as u64 + 4;
    }

    let end = 10 + size as u64;
    let mut frame_header = [0u8; 10];

    while offset + 10 <= end {
        file.seek(SeekFrom::Start(offset))?;
        if file.read(&mut frame_header)? < 10 || frame_header[0] == 0 {
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
        let mut data = vec![0u8; frame_size];
        file.read_exact(&mut data)?;

        match &frame_header[0..4] {
            b"TIT2" => meta.title = id3_text(&data),
            b"TPE1" => meta.artist = id3_text(&data),
            b"TALB" => meta.album = id3_text(&data),
            b"TCON" => meta.genre = parse_genre(&id3_text(&data)),
            b"TRCK" => meta.track_number = parse_track(&id3_text(&data)),
            b"TYER" | b"TDRC" => meta.year = parse_year(&id3_text(&data)),
            b"TDOR" | b"TORY" if meta.year == 0 => meta.year = parse_year(&id3_text(&data)),
            b"APIC" if meta.artwork.is_none() => meta.artwork = apic_data(&data),
            _ => {}
        }
        offset += 10 + frame_size as u64;
    }
    meta.duration = mp3_duration(path, end);
    Ok(meta)
}

fn mp3_duration(path: &Path, audio_start: u64) -> f64 {
    const BR_MPEG1: [u32; 16] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
    ];
    const BR_MPEG2: [u32; 16] = [
        0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
    ];
    const RATES: [[u32; 3]; 3] = [
        [44100, 48000, 32000],
        [22050, 24000, 16000],
        [11025, 12000, 8000],
    ];

    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return 0.0,
    };
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut buf = vec![0u8; 8192];
    if file.seek(SeekFrom::Start(audio_start)).is_err() {
        return 0.0;
    }
    let n = file.read(&mut buf).unwrap_or(0);
    if n < 16 {
        return 0.0;
    }
    let buf = &buf[..n];

    let mut i = 0;
    while i + 4 < buf.len() {
        if buf[i] == 0xFF && buf[i + 1] & 0xE0 == 0xE0 {
            let ver_bits = (buf[i + 1] >> 3) & 3;
            let layer = (buf[i + 1] >> 1) & 3;
            let br_idx = (buf[i + 2] >> 4) as usize;
            let rate_idx = ((buf[i + 2] >> 2) & 3) as usize;
            let padding = ((buf[i + 2] >> 1) & 1) as u32;
            let mono = buf[i + 3] >> 6 == 3;
            // version: 0 = MPEG2.5, 2 = MPEG2, 3 = MPEG1; layer 1 = III
            let (vi, mpeg1) = match ver_bits {
                3 => (0, true),
                2 => (1, false),
                0 => (2, false),
                _ => {
                    i += 1;
                    continue;
                }
            };
            if layer != 1 || br_idx == 0 || br_idx == 15 || rate_idx == 3 {
                i += 1;
                continue;
            }
            let bitrate = if mpeg1 {
                BR_MPEG1[br_idx]
            } else {
                BR_MPEG2[br_idx]
            } * 1000;
            let rate = RATES[vi][rate_idx];
            if bitrate == 0 || rate == 0 {
                return 0.0;
            }
            let spf = if mpeg1 { 1152 } else { 576 };
            // Xing/Info header after side info
            let side = match (mpeg1, mono) {
                (true, true) | (false, false) => 17,
                (true, false) => 32,
                (false, true) => 9,
            };
            let x = i + 4 + side;
            if x + 12 <= buf.len()
                && (buf[x..].starts_with(b"Xing") || buf[x..].starts_with(b"Info"))
            {
                let flags = be_u32(&buf[x + 4..x + 8]);
                if flags & 1 != 0 {
                    let frames = be_u32(&buf[x + 8..x + 12]);
                    return frames as f64 * spf as f64 / rate as f64;
                }
            }
            let frame_len = ((if mpeg1 { 144 } else { 72 } * bitrate / rate) + padding) as u64;
            if frame_len > 0 && file_len > audio_start {
                let frames = (file_len - audio_start) / frame_len;
                return frames as f64 * spf as f64 / rate as f64;
            }
            return 0.0;
        }
        i += 1;
    }
    0.0
}

fn mp4_box(file: &mut File, len: u64, pos: u64) -> Result<Option<([u8; 4], u64, u64)>> {
    if pos + 8 > len {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(pos))?;
    let mut header = [0u8; 8];
    if file.read(&mut header)? < 8 {
        return Ok(None);
    }
    let typ = header[4..8].try_into().unwrap();
    let (start, end) = match be_u32(&header) {
        0 => (pos + 8, len),
        1 => {
            let mut large = [0u8; 8];
            file.read_exact(&mut large)?;
            (pos + 16, pos + u64::from_be_bytes(large))
        }
        2..=7 => return Ok(None),
        size => (pos + 8, pos + size as u64),
    };
    Ok(Some((typ, start, end.min(len))))
}

fn mp4_find(
    file: &mut File,
    len: u64,
    start: u64,
    end: u64,
    target: &[u8; 4],
) -> Result<Option<(u64, u64)>> {
    let mut pos = start;
    while pos + 8 <= end {
        let Some((typ, s, e)) = mp4_box(file, len, pos)? else {
            break;
        };
        if &typ == target {
            return Ok(Some((s, e)));
        }
        if e <= pos {
            break;
        }
        pos = e;
    }
    Ok(None)
}

fn mp4_text(data: &[u8]) -> String {
    String::from_utf8_lossy(data).trim_matches('\0').to_string()
}

fn read_mp4(path: &Path) -> Result<AudioMetadata> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut meta = AudioMetadata::default();

    let Some((moov_s, moov_e)) = mp4_find(&mut file, len, 0, len, b"moov")? else {
        return Ok(meta);
    };

    if let Some((mvhd_s, _)) = mp4_find(&mut file, len, moov_s, moov_e, b"mvhd")? {
        file.seek(SeekFrom::Start(mvhd_s))?;
        let mut buf = [0u8; 32];
        file.read_exact(&mut buf)?;
        let (scale, dur) = if buf[0] == 1 {
            (
                be_u32(&buf[20..24]),
                u64::from_be_bytes(buf[24..32].try_into().unwrap()),
            )
        } else {
            (be_u32(&buf[12..16]), be_u32(&buf[16..20]) as u64)
        };
        if scale > 0 {
            meta.duration = dur as f64 / scale as f64;
        }
    }

    let Some((udta_s, udta_e)) = mp4_find(&mut file, len, moov_s, moov_e, b"udta")? else {
        return Ok(meta);
    };
    let Some((meta_s, meta_e)) = mp4_find(&mut file, len, udta_s, udta_e, b"meta")? else {
        return Ok(meta);
    };
    let Some((ilst_s, ilst_e)) = mp4_find(&mut file, len, meta_s + 4, meta_e, b"ilst")? else {
        return Ok(meta);
    };

    let mut pos = ilst_s;
    while pos + 8 <= ilst_e {
        let Some((typ, s, e)) = mp4_box(&mut file, len, pos)? else {
            break;
        };
        if let Some((data_s, data_e)) =
            mp4_find(&mut file, len, s, e, b"data")?.filter(|(data_s, data_e)| *data_e > data_s + 8)
        {
            let n = ((data_e - data_s - 8) as usize).min(1_000_000);
            let mut buf = vec![0u8; n];
            file.seek(SeekFrom::Start(data_s + 8))?;
            file.read_exact(&mut buf)?;
            match &typ {
                b"\xa9nam" => meta.title = mp4_text(&buf),
                b"\xa9ART" => meta.artist = mp4_text(&buf),
                b"\xa9alb" => meta.album = mp4_text(&buf),
                b"\xa9day" => meta.year = parse_year(&mp4_text(&buf)),
                b"\xa9gen" => meta.genre = parse_genre(&mp4_text(&buf)),
                b"gnre" if buf.len() >= 2 => {
                    let index = u16::from_be_bytes([buf[0], buf[1]]) as usize;
                    if let Some(genre) = index.checked_sub(1).and_then(|i| ID3_GENRES.get(i)) {
                        meta.genre = (*genre).to_string();
                    }
                }
                b"trkn" if buf.len() >= 4 => {
                    meta.track_number = u16::from_be_bytes([buf[2], buf[3]]) as u32
                }
                b"covr" => meta.artwork = Some(buf),
                _ => {}
            }
        }
        if e <= pos {
            break;
        }
        pos = e;
    }
    Ok(meta)
}

fn read_flac(path: &Path) -> Result<AudioMetadata> {
    let mut file = File::open(path)?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)?;
    if &magic != b"fLaC" {
        return Ok(AudioMetadata::default());
    }
    let mut meta = AudioMetadata::default();
    let mut hdr = [0u8; 4];
    loop {
        file.read_exact(&mut hdr)?;
        let last = hdr[0] & 0x80 != 0;
        let len = be_u32(&[0, hdr[1], hdr[2], hdr[3]]) as usize;
        if len > 10_000_000 {
            break;
        }
        let mut block = vec![0u8; len];
        file.read_exact(&mut block)?;
        match hdr[0] & 0x7F {
            0 if block.len() >= 18 => {
                // STREAMINFO bytes 10..18: 20-bit rate, 3-bit ch, 5-bit bps, 36-bit total.
                let q = u64::from_be_bytes(block[10..18].try_into().unwrap());
                let sample_rate = (q >> 44) as u32;
                let total = q & 0x0F_FFFF_FFFF;
                if sample_rate > 0 {
                    meta.duration = total as f64 / sample_rate as f64;
                }
            }
            4 => vorbis_comments(&block, &mut meta),
            6 if meta.artwork.is_none() => meta.artwork = flac_picture(&block),
            _ => {}
        }
        if last {
            break;
        }
    }
    Ok(meta)
}

fn read_vorbis(path: &Path) -> Result<AudioMetadata> {
    let mut file = File::open(path)?;
    let mut meta = AudioMetadata::default();
    let mut packet = Vec::new();
    for _ in 0..64 {
        let mut ph = [0u8; 27];
        if file.read(&mut ph)? != 27 || &ph[0..4] != b"OggS" {
            break;
        }
        let mut table = vec![0u8; ph[26] as usize];
        file.read_exact(&mut table)?;
        for &s in &table {
            let start = packet.len();
            packet.resize(start + s as usize, 0);
            file.read_exact(&mut packet[start..])?;
            if s < 255 {
                let prefix = if packet.starts_with(b"\x03vorbis") {
                    Some(7)
                } else if packet.starts_with(b"OpusTags") {
                    Some(8)
                } else {
                    None
                };
                if let Some(prefix) = prefix {
                    vorbis_comments(&packet[prefix..], &mut meta);
                    return Ok(meta);
                }
                packet.clear();
            } else if packet.len() > 4_000_000 {
                return Ok(meta);
            }
        }
    }
    Ok(meta)
}

fn vorbis_comments(data: &[u8], meta: &mut AudioMetadata) {
    let Some(vendor) = data.get(0..4) else {
        return;
    };
    let mut pos = 4 + le_u32(vendor);
    let Some(count) = data.get(pos..pos + 4) else {
        return;
    };
    pos += 4;
    for _ in 0..le_u32(count) {
        let Some(len) = data.get(pos..pos + 4) else {
            break;
        };
        let len = le_u32(len);
        pos += 4;
        let Some(bytes) = data.get(pos..pos.saturating_add(len)) else {
            break;
        };
        pos += len;
        let comment = String::from_utf8_lossy(bytes);
        let Some((key, value)) = comment.split_once('=') else {
            continue;
        };
        match key.to_ascii_uppercase().as_str() {
            "TITLE" => meta.title = value.to_string(),
            "ARTIST" => meta.artist = value.to_string(),
            "ALBUM" => meta.album = value.to_string(),
            "GENRE" => meta.genre = parse_genre(value),
            "TRACKNUMBER" => meta.track_number = parse_track(value),
            "DATE" | "YEAR" => meta.year = parse_year(value),
            _ => {}
        }
    }
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

fn apic_data(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 4 {
        return None;
    }
    let enc = data[0];
    let mut pos = 1;
    while pos < data.len() && data[pos] != 0 {
        pos += 1;
    }
    pos += 1; // mime
    if pos >= data.len() {
        return None;
    }
    pos += 1; // picture type
    if enc == 1 || enc == 2 {
        while pos + 1 < data.len() && (data[pos] != 0 || data[pos + 1] != 0) {
            pos += 2;
        }
        pos += 2;
    } else {
        while pos < data.len() && data[pos] != 0 {
            pos += 1;
        }
        pos += 1;
    }
    if pos >= data.len() {
        return None;
    }
    Some(data[pos..].to_vec())
}

fn flac_picture(block: &[u8]) -> Option<Vec<u8>> {
    if block.len() < 32 {
        return None;
    }
    let mut idx = 4 + 4;
    if idx + 4 > block.len() {
        return None;
    }
    let mime_len = be_u32(&block[idx..idx + 4]) as usize;
    idx += 4 + mime_len;
    if idx + 4 > block.len() {
        return None;
    }
    let desc_len = be_u32(&block[idx..idx + 4]) as usize;
    idx += 4 + desc_len + 4 + 4 + 4 + 4;
    if idx + 4 > block.len() {
        return None;
    }
    let data_len = be_u32(&block[idx..idx + 4]) as usize;
    idx += 4;
    if idx + data_len > block.len() || data_len == 0 {
        return None;
    }
    Some(block[idx..idx + data_len].to_vec())
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
    }
}
