//! MusicBrainz web service: query building and JSON parsing. No network here.

use crate::lookup::text::normalize;
use anyhow::{Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::Value;

const BASE: &str = "https://musicbrainz.org/ws/2/release";
const SEARCH_LIMIT: usize = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub id: String,
    pub release_group: String,
    pub title: String,
    pub artist: String,
    pub year: u32,
    pub tracks: Vec<ReleaseTrack>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseTrack {
    /// Position counted across all media, starting at 1.
    pub number: u32,
    pub title: String,
    pub artist: String,
    pub recording: String,
    pub length_secs: Option<f64>,
}

/// Escape Lucene query syntax so album names are matched literally.
pub fn lucene_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '&' | '|' if chars.peek() == Some(&c) => {
                chars.next();
                out.push('\\');
                out.push(c);
                out.push(c);
            }
            '+' | '-' | '!' | '(' | ')' | '{' | '}' | '[' | ']' | '^' | '"' | '~' | '*' | '?'
            | ':' | '\\' | '/' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Release search by album title, artist (skipped for compilations) and
/// track count.
pub fn search_url(album: &str, artist: Option<&str>, track_count: usize) -> String {
    let mut query = format!("release:\"{}\"", lucene_escape(album));
    if let Some(artist) = artist.filter(|a| normalize(a) != "various artists") {
        query.push_str(&format!(" AND artist:\"{}\"", lucene_escape(artist)));
    }
    query.push_str(&format!(" AND tracks:{track_count}"));
    format!(
        "{BASE}/?fmt=json&limit={SEARCH_LIMIT}&query={}",
        utf8_percent_encode(&query, NON_ALPHANUMERIC)
    )
}

/// Releases of a known release group, for libraries whose tags carry ids.
pub fn release_group_search_url(release_group: &str) -> String {
    format!(
        "{BASE}/?fmt=json&limit={SEARCH_LIMIT}&query={}",
        utf8_percent_encode(&format!("rgid:{release_group}"), NON_ALPHANUMERIC)
    )
}

pub fn release_url(id: &str) -> String {
    format!("{BASE}/{id}?fmt=json&inc=recordings+artist-credits+release-groups")
}

/// Release ids from a search response, best match first.
pub fn parse_search(body: &[u8]) -> Result<Vec<String>> {
    let json: Value = serde_json::from_slice(body).context("MusicBrainz search response")?;
    Ok(json["releases"]
        .as_array()
        .map(|releases| {
            releases
                .iter()
                .filter_map(|release| release["id"].as_str().map(str::to_string))
                .take(SEARCH_LIMIT)
                .collect()
        })
        .unwrap_or_default())
}

pub fn parse_release(body: &[u8]) -> Result<Release> {
    let json: Value = serde_json::from_slice(body).context("MusicBrainz release response")?;
    let artist = artist_credit(&json["artist-credit"]);
    let year = year_of(json["date"].as_str())
        .or_else(|| year_of(json["release-group"]["first-release-date"].as_str()))
        .unwrap_or(0);
    let mut tracks = Vec::new();
    for medium in json["media"].as_array().into_iter().flatten() {
        for track in medium["tracks"].as_array().into_iter().flatten() {
            let track_artist = artist_credit(&track["artist-credit"]);
            tracks.push(ReleaseTrack {
                number: tracks.len() as u32 + 1,
                title: text(&track["title"]),
                artist: if track_artist.is_empty() {
                    artist.clone()
                } else {
                    track_artist
                },
                recording: text(&track["recording"]["id"]),
                length_secs: track["length"].as_f64().map(|ms| ms / 1000.0),
            });
        }
    }
    Ok(Release {
        id: text(&json["id"]),
        release_group: text(&json["release-group"]["id"]),
        title: text(&json["title"]),
        artist,
        year,
        tracks,
    })
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or("").to_string()
}

fn year_of(date: Option<&str>) -> Option<u32> {
    date?.get(..4)?.parse().ok().filter(|year| *year > 0)
}

fn artist_credit(credit: &Value) -> String {
    credit
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .map(|part| format!("{}{}", text(&part["name"]), text(&part["joinphrase"])))
                .collect::<String>()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_url_quotes_escapes_and_skips_various_artists() {
        let url = search_url("Estranhos no Ninho", Some("Exxótica"), 14);
        assert!(url.starts_with("https://musicbrainz.org/ws/2/release/?fmt=json&limit=5&query="));
        let query = url.split("query=").nth(1).unwrap();
        assert_eq!(
            query,
            "release%3A%22Estranhos%20no%20Ninho%22%20AND%20artist%3A%22Exx%C3%B3tica%22%20AND%20tracks%3A14"
        );

        let url = search_url(
            "Now That's What I Call Music! 50",
            Some("Various Artists"),
            20,
        );
        assert!(!url.contains("artist%3A"));
        let url = search_url("Lonely", None, 3);
        assert!(!url.contains("artist%3A"));
        assert!(url.ends_with("tracks%3A3"));

        let url = search_url("A+B (C) [D] \"quoted\"", None, 1);
        let query = url.split("query=").nth(1).unwrap();
        assert!(query.contains("%5C%2B"), "plus is escaped for lucene");
        assert!(
            query.contains("%5C%28"),
            "parenthesis is escaped for lucene"
        );
        assert!(query.contains("%5C%22quoted%5C%22"), "quotes are escaped");
    }

    #[test]
    fn lucene_escape_covers_special_characters() {
        assert_eq!(
            lucene_escape(r#"a+b-c&&d||e!f(g)h{i}j[k]l^m"n~o*p?q:r\s/t"#),
            r#"a\+b\-c\&&d\||e\!f\(g\)h\{i\}j\[k\]l\^m\"n\~o\*p\?q\:r\\s\/t"#
        );
    }

    #[test]
    fn release_group_search_url_uses_rgid() {
        assert_eq!(
            release_group_search_url("g-1"),
            "https://musicbrainz.org/ws/2/release/?fmt=json&limit=5&query=rgid%3Ag%2D1"
        );
    }

    #[test]
    fn release_url_requests_recordings_and_release_groups() {
        assert_eq!(
            release_url("abc-123"),
            "https://musicbrainz.org/ws/2/release/abc-123?fmt=json&inc=recordings+artist-credits+release-groups"
        );
    }

    #[test]
    fn parse_search_returns_at_most_five_ids() {
        let body = r#"{"releases":[{"id":"r1"},{"id":"r2"},{"id":"r3"},{"id":"r4"},{"id":"r5"},{"id":"r6"}]}"#;
        assert_eq!(
            parse_search(body.as_bytes()).unwrap(),
            ["r1", "r2", "r3", "r4", "r5"]
        );
        assert!(parse_search(br#"{"releases":[]}"#).unwrap().is_empty());
        assert!(parse_search(b"not json").is_err());
    }

    #[test]
    fn parse_release_flattens_media_and_reads_lengths() {
        let body = r#"{
            "id": "rel-1",
            "title": "Estranhos no Ninho",
            "date": "2002-06-01",
            "artist-credit": [{"name": "Exxótica", "joinphrase": " & "}, {"name": "Friends", "joinphrase": ""}],
            "release-group": {"id": "rg-1", "first-release-date": "2002"},
            "media": [
                {"tracks": [
                    {"position": 1, "title": "No Palco", "length": 243000,
                     "artist-credit": [{"name": "Exxótica", "joinphrase": ""}],
                     "recording": {"id": "rec-1"}},
                    {"position": 2, "title": "Fruto Proibido",
                     "recording": {"id": "rec-2"}}
                ]},
                {"tracks": [
                    {"position": 1, "title": "Agora (live)", "length": 180500,
                     "recording": {"id": "rec-3"}}
                ]}
            ]
        }"#;
        let release = parse_release(body.as_bytes()).unwrap();
        assert_eq!(release.id, "rel-1");
        assert_eq!(release.release_group, "rg-1");
        assert_eq!(release.title, "Estranhos no Ninho");
        assert_eq!(release.artist, "Exxótica & Friends");
        assert_eq!(release.year, 2002);
        assert_eq!(release.tracks.len(), 3);
        assert_eq!(release.tracks[0].number, 1);
        assert_eq!(release.tracks[0].title, "No Palco");
        assert_eq!(release.tracks[0].artist, "Exxótica");
        assert_eq!(release.tracks[0].recording, "rec-1");
        assert_eq!(release.tracks[0].length_secs, Some(243.0));
        assert_eq!(release.tracks[1].length_secs, None);
        assert_eq!(
            release.tracks[1].artist, "Exxótica & Friends",
            "falls back to the release artist"
        );
        assert_eq!(
            release.tracks[2].number, 3,
            "numbering continues across media"
        );
        assert_eq!(release.tracks[2].length_secs, Some(180.5));
    }

    #[test]
    fn parse_release_year_falls_back_to_release_group_date() {
        let body = r#"{"id":"r","title":"T","release-group":{"id":"g","first-release-date":"1999-01-01"},"media":[]}"#;
        let release = parse_release(body.as_bytes()).unwrap();
        assert_eq!(release.year, 1999);
        assert_eq!(release.artist, "");
        assert!(release.tracks.is_empty());
    }
}
