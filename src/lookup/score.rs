use crate::lookup::musicbrainz::Release;
use crate::lookup::text::{normalize, similarity};

pub const ACCEPT_THRESHOLD: f64 = 0.15;
pub const DURATION_TOLERANCE_SECS: f64 = 2.0;

const WEIGHT_ALBUM: f64 = 3.0;
const WEIGHT_ARTIST: f64 = 3.0;
const WEIGHT_TRACK_COUNT: f64 = 2.0;
const WEIGHT_TITLE: f64 = 1.0;
const WEIGHT_DURATION: f64 = 2.0;
const WEIGHT_YEAR: f64 = 1.0;
const MAX_COUNTED_TRACK_DIFF: usize = 10;
const DURATION_FULL_PENALTY_SECS: f64 = 30.0;
const YEAR_FULL_PENALTY: f64 = 5.0;

#[derive(Debug, Clone, PartialEq)]
pub struct LocalAlbum {
    pub title: String,
    pub title_from_folder: bool,
    pub artist: Option<String>,
    pub year: u32,
    pub tracks: Vec<LocalTrack>,
    pub has_ids: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalTrack {
    pub number: u32,
    pub title: Option<String>,
    pub duration: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    pub distance: f64,
    pub pairing: Vec<Option<usize>>,
    pub durations_matched: bool,
}

struct Distance {
    weighted: f64,
    weight: f64,
}

impl Distance {
    fn add(&mut self, weight: f64, penalty: f64) {
        self.weighted += weight * penalty.clamp(0.0, 1.0);
        self.weight += weight;
    }

    fn value(&self) -> f64 {
        if self.weight == 0.0 {
            0.0
        } else {
            self.weighted / self.weight
        }
    }
}

pub fn distance(local: &LocalAlbum, release: &Release) -> Match {
    let mut total = Distance {
        weighted: 0.0,
        weight: 0.0,
    };
    total.add(WEIGHT_ALBUM, 1.0 - similarity(&local.title, &release.title));
    if let Some(artist) = local
        .artist
        .as_deref()
        .filter(|a| normalize(a) != "various artists")
    {
        total.add(WEIGHT_ARTIST, 1.0 - similarity(artist, &release.artist));
    }
    let count_diff = local.tracks.len().abs_diff(release.tracks.len());
    if count_diff > 0 {
        let counted = count_diff.min(MAX_COUNTED_TRACK_DIFF) as f64;
        total.add(WEIGHT_TRACK_COUNT * counted, 1.0);
    }
    if local.year > 0 && release.year > 0 {
        let diff = local.year.abs_diff(release.year) as f64;
        total.add(WEIGHT_YEAR, diff / YEAR_FULL_PENALTY);
    }

    let pairing = pair(local, release);
    let mut durations_matched = !local.tracks.is_empty();
    for (track, pair) in local.tracks.iter().zip(&pairing) {
        let Some(other) = pair.map(|i| &release.tracks[i]) else {
            total.add(WEIGHT_TRACK_COUNT, 1.0);
            durations_matched = false;
            continue;
        };
        if let Some(title) = &track.title {
            total.add(WEIGHT_TITLE, 1.0 - similarity(title, &other.title));
        }
        match other.length_secs {
            Some(length) if track.duration > 0.0 => {
                let diff = (track.duration - length).abs();
                let over = (diff - DURATION_TOLERANCE_SECS).max(0.0);
                total.add(WEIGHT_DURATION, over / DURATION_FULL_PENALTY_SECS);
                if diff > DURATION_TOLERANCE_SECS {
                    durations_matched = false;
                }
            }
            _ => durations_matched = false,
        }
    }

    Match {
        distance: total.value(),
        pairing,
        durations_matched,
    }
}

pub fn accept(m: &Match, local: &LocalAlbum) -> bool {
    m.distance <= ACCEPT_THRESHOLD && (m.durations_matched || local.has_ids)
}

fn pair(local: &LocalAlbum, release: &Release) -> Vec<Option<usize>> {
    let numbers: Vec<u32> = local.tracks.iter().map(|t| t.number).collect();
    let mut sorted = numbers.clone();
    sorted.sort_unstable();
    sorted.dedup();
    let by_number = sorted.len() == numbers.len()
        && numbers
            .iter()
            .all(|n| *n > 0 && (*n as usize) <= release.tracks.len());
    if by_number {
        return numbers.iter().map(|n| Some(*n as usize - 1)).collect();
    }

    let mut candidates: Vec<(f64, usize, usize)> = Vec::new();
    for (i, track) in local.tracks.iter().enumerate() {
        if track.duration <= 0.0 {
            continue;
        }
        for (j, other) in release.tracks.iter().enumerate() {
            if let Some(length) = other.length_secs {
                candidates.push(((track.duration - length).abs(), i, j));
            }
        }
    }
    candidates.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut pairing = vec![None; local.tracks.len()];
    let mut taken = vec![false; release.tracks.len()];
    for (_, i, j) in candidates {
        if pairing[i].is_none() && !taken[j] {
            pairing[i] = Some(j);
            taken[j] = true;
        }
    }
    pairing
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookup::musicbrainz::ReleaseTrack;

    fn release(title: &str, artist: &str, year: u32, tracks: &[(&str, f64)]) -> Release {
        Release {
            id: "rel".into(),
            release_group: "rg".into(),
            title: title.into(),
            artist: artist.into(),
            year,
            media: 1,
            tracks: tracks
                .iter()
                .enumerate()
                .map(|(i, (title, secs))| ReleaseTrack {
                    number: i as u32 + 1,
                    title: (*title).into(),
                    artist: artist.into(),
                    recording: format!("rec-{i}"),
                    length_secs: Some(*secs),
                })
                .collect(),
        }
    }

    fn local(title: &str, artist: Option<&str>, tracks: &[(Option<&str>, f64)]) -> LocalAlbum {
        LocalAlbum {
            title: title.into(),
            title_from_folder: false,
            artist: artist.map(str::to_string),
            year: 0,
            has_ids: false,
            tracks: tracks
                .iter()
                .enumerate()
                .map(|(i, (title, secs))| LocalTrack {
                    number: i as u32 + 1,
                    title: title.map(str::to_string),
                    duration: *secs,
                })
                .collect(),
        }
    }

    const NINHO: [(&str, f64); 3] = [
        ("No Palco", 243.0),
        ("Fruto Proibido", 201.5),
        ("Gato Preto", 188.0),
    ];

    #[test]
    fn identical_album_scores_near_zero_and_is_accepted() {
        let rel = release("Estranhos no Ninho", "Exxótica", 2002, &NINHO);
        let loc = local(
            "Estranhos no Ninho",
            Some("Exxótica"),
            &[
                (Some("No Palco"), 243.4),
                (Some("Fruto Proibido"), 201.0),
                (Some("Gato Preto"), 188.9),
            ],
        );
        let m = distance(&loc, &rel);
        assert!(m.distance < 0.01, "{}", m.distance);
        assert_eq!(m.pairing, [Some(0), Some(1), Some(2)]);
        assert!(m.durations_matched);
        assert!(accept(&m, &loc));
    }

    #[test]
    fn durations_alone_identify_an_untagged_folder() {
        let rel = release("Estranhos no Ninho", "Exxótica", 2002, &NINHO);
        let mut loc = local(
            "Estranhos no Ninho",
            None,
            &[(None, 188.5), (None, 243.0), (None, 201.0)],
        );
        for track in &mut loc.tracks {
            track.number = 0;
        }
        let m = distance(&loc, &rel);
        assert_eq!(
            m.pairing,
            [Some(2), Some(0), Some(1)],
            "paired by closest duration"
        );
        assert!(m.durations_matched);
        assert!(m.distance <= ACCEPT_THRESHOLD, "{}", m.distance);
        assert!(accept(&m, &loc));
    }

    #[test]
    fn single_vs_full_album_is_rejected() {
        let mut long: Vec<(&str, f64)> = vec![("No Palco", 243.0), ("Fruto Proibido", 201.5)];
        for _ in 0..18 {
            long.push(("Filler", 200.0));
        }
        let rel = release("Estranhos no Ninho", "Exxótica", 2002, &long);
        let loc = local(
            "Estranhos no Ninho",
            Some("Exxótica"),
            &[(Some("No Palco"), 243.0), (Some("Fruto Proibido"), 201.5)],
        );
        let m = distance(&loc, &rel);
        assert!(m.distance > ACCEPT_THRESHOLD, "{}", m.distance);
        assert!(!accept(&m, &loc));
    }

    #[test]
    fn various_artists_skips_artist_component() {
        let mut rel = release("Now 50", "Various Artists", 2001, &NINHO);
        for (i, track) in rel.tracks.iter_mut().enumerate() {
            track.artist = format!("Band {i}");
        }
        let loc = local(
            "Now 50",
            Some("Various Artists"),
            &[
                (Some("No Palco"), 243.0),
                (Some("Fruto Proibido"), 201.5),
                (Some("Gato Preto"), 188.0),
            ],
        );
        let m = distance(&loc, &rel);
        assert!(m.distance < 0.01, "{}", m.distance);
        assert!(accept(&m, &loc));
    }

    #[test]
    fn weak_title_only_match_without_durations_is_not_accepted() {
        let rel = release("Estranhos no Ninho", "Exxótica", 2002, &NINHO);
        let loc = local(
            "Estranhos no Ninho",
            Some("Exxótica"),
            &[
                (Some("No Palco"), 0.0),
                (Some("Fruto Proibido"), 0.0),
                (Some("Gato Preto"), 0.0),
            ],
        );
        let m = distance(&loc, &rel);
        assert!(m.distance <= ACCEPT_THRESHOLD, "{}", m.distance);
        assert!(!m.durations_matched);
        assert!(!accept(&m, &loc), "titles alone are not proof");
    }

    #[test]
    fn ids_from_tags_accept_without_durations() {
        let rel = release("Estranhos no Ninho", "Exxótica", 2002, &NINHO);
        let mut loc = local(
            "Estranhos no Ninho",
            Some("Exxótica"),
            &[
                (Some("No Palco"), 0.0),
                (Some("Fruto Proibido"), 0.0),
                (Some("Gato Preto"), 0.0),
            ],
        );
        loc.has_ids = true;
        let m = distance(&loc, &rel);
        assert!(accept(&m, &loc));
    }

    #[test]
    fn wrong_album_with_same_track_count_is_rejected() {
        let rel = release(
            "Kid A",
            "Radiohead",
            2000,
            &[
                ("Everything", 298.0),
                ("Kid A", 284.0),
                ("Idioteque", 309.0),
            ],
        );
        let loc = local(
            "Estranhos no Ninho",
            Some("Exxótica"),
            &[
                (Some("No Palco"), 243.0),
                (Some("Fruto Proibido"), 201.5),
                (Some("Gato Preto"), 188.0),
            ],
        );
        let m = distance(&loc, &rel);
        assert!(m.distance > 0.5, "{}", m.distance);
        assert!(!m.durations_matched);
    }
}
