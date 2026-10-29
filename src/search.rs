//! Search query translation and BM25 search over the SQLite FTS index.

use crate::db;
use crate::models::{AlbumSummary, ArtistEntry, Track};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashMap;
use std::ops::Range;

/// Maximum number of tracks returned for a query, matching what the list shows.
pub const TRACK_LIMIT: usize = 500;
/// Maximum number of albums returned for a query.
pub const ALBUM_LIMIT: usize = 300;

/// Translate raw user input into an FTS5 MATCH expression.
///
/// Every whitespace-separated word is reduced to its alphanumeric characters so
/// that FTS syntax characters (`" * ( ) : ^ -` and the `AND`/`OR`/`NOT`/`NEAR`
/// operators) cannot leak into the query, then quoted and AND-joined. Each word
/// is a prefix query, so partial words match as the user types. An input with
/// no letters or digits means "no filter".
pub fn match_query(input: &str) -> Option<String> {
    let tokens: Vec<String> = input
        .split_whitespace()
        .map(sanitize)
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.is_empty() {
        return None;
    }
    Some(
        tokens
            .iter()
            .map(|token| format!("\"{token}\"*"))
            .collect::<Vec<_>>()
            .join(" AND "),
    )
}

fn sanitize(token: &str) -> String {
    token.chars().filter(|c| c.is_alphanumeric()).collect()
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub track_ids: Vec<i64>,
    pub album_keys: Vec<String>,
}

/// Run a BM25 query. `None` means the query filtered nothing (return everything).
pub fn search(conn: &Connection, query: &str) -> Result<Option<SearchResult>> {
    let Some(expr) = match_query(query) else {
        return Ok(None);
    };
    Ok(Some(SearchResult {
        track_ids: db::search_tracks(conn, &expr, TRACK_LIMIT)?,
        album_keys: db::search_albums(conn, &expr, ALBUM_LIMIT)?,
    }))
}

/// Build the Artists view from the albums matched by a search, in the same way
/// the unfiltered view groups albums by artist.
pub fn artists_from_albums(conn: &Connection, keys: &[String]) -> Result<Vec<ArtistEntry>> {
    let albums = db::albums_by_keys(conn, keys)?;
    let mut grouped: HashMap<String, (usize, usize, Option<String>)> = HashMap::new();
    for album in albums {
        let entry = grouped.entry(album.artist).or_insert((0, 0, None));
        entry.0 += 1;
        entry.1 += album.track_count;
        if entry.2.is_none() {
            entry.2 = Some(album.id);
        }
    }
    let mut artists: Vec<ArtistEntry> = grouped
        .into_iter()
        .map(
            |(name, (album_count, song_count, cover_album_id))| ArtistEntry {
                name,
                album_count,
                song_count,
                cover_album_id,
            },
        )
        .collect();
    artists.sort_by_key(|entry| entry.name.to_lowercase());
    Ok(artists)
}

#[derive(Debug, Clone, PartialEq)]
pub enum TopResult {
    Artist(ArtistEntry),
    Album(AlbumSummary),
    Track(usize, Track),
}

fn folded(text: &str) -> String {
    text.chars().flat_map(char::to_lowercase).collect()
}

pub fn top_result(
    query: &str,
    artists: &[ArtistEntry],
    albums: &[AlbumSummary],
    tracks: &[Track],
) -> Option<TopResult> {
    let query = folded(query.trim());
    if query.is_empty() {
        return None;
    }
    let matches = |text: &str, exact: bool| {
        let text = folded(text);
        if exact {
            text == query
        } else {
            text.starts_with(&query)
        }
    };
    for exact in [true, false] {
        if let Some(artist) = artists.iter().find(|a| matches(&a.name, exact)) {
            return Some(TopResult::Artist(artist.clone()));
        }
        if let Some(album) = albums.iter().find(|a| matches(&a.title, exact)) {
            return Some(TopResult::Album(album.clone()));
        }
        if let Some(i) = tracks.iter().position(|t| matches(&t.title, exact)) {
            return Some(TopResult::Track(i, tracks[i].clone()));
        }
    }
    if let Some(track) = tracks.first() {
        return Some(TopResult::Track(0, track.clone()));
    }
    if let Some(album) = albums.first() {
        return Some(TopResult::Album(album.clone()));
    }
    artists.first().cloned().map(TopResult::Artist)
}

pub fn highlight_ranges(text: &str, query: &str) -> Vec<Range<usize>> {
    let tokens: Vec<Vec<char>> = query
        .split_whitespace()
        .map(|token| {
            token
                .chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<Vec<_>>()
        })
        .filter(|token| !token.is_empty())
        .collect();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let word_start =
            chars[i].1.is_alphanumeric() && (i == 0 || !chars[i - 1].1.is_alphanumeric());
        if word_start {
            if let Some(len) = tokens
                .iter()
                .filter_map(|token| prefix_len(&chars[i..], token))
                .max()
            {
                let end = chars.get(i + len).map_or(text.len(), |c| c.0);
                ranges.push(chars[i].0..end);
                i += len;
                continue;
            }
        }
        i += 1;
    }
    ranges
}

fn prefix_len(chars: &[(usize, char)], token: &[char]) -> Option<usize> {
    let mut matched = 0;
    for (consumed, &(_, c)) in chars.iter().enumerate() {
        if matched == token.len() {
            return Some(consumed);
        }
        for lower in c.to_lowercase() {
            if token.get(matched) != Some(&lower) {
                return None;
            }
            matched += 1;
        }
    }
    (matched == token.len()).then_some(chars.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_punctuation_queries_filter_nothing() {
        assert_eq!(match_query(""), None);
        assert_eq!(match_query("   "), None);
        assert_eq!(match_query("!!! :^ \" *"), None);
    }

    #[test]
    fn words_are_quoted_and_anded_as_prefixes() {
        assert_eq!(
            match_query("dark moon"),
            Some("\"dark\"* AND \"moon\"*".into())
        );
        assert_eq!(match_query("neo"), Some("\"neo\"*".into()));
    }

    #[test]
    fn fts_operators_cannot_leak_into_the_query() {
        // Quoting turns operators into literal words, so they cannot widen or
        // otherwise change the query.
        assert_eq!(match_query("abba OR"), Some("\"abba\"* AND \"OR\"*".into()));
        assert_eq!(match_query("\"neon\""), Some("\"neon\"*".into()));
        assert_eq!(match_query("c++"), Some("\"c\"*".into()));
        assert_eq!(match_query("don't"), Some("\"dont\"*".into()));
    }

    #[test]
    fn non_ascii_letters_are_kept() {
        assert_eq!(match_query("café"), Some("\"café\"*".into()));
    }

    fn artist(name: &str) -> ArtistEntry {
        ArtistEntry {
            name: name.into(),
            ..Default::default()
        }
    }

    fn album(title: &str) -> AlbumSummary {
        AlbumSummary {
            title: title.into(),
            ..Default::default()
        }
    }

    fn track(title: &str) -> Track {
        Track {
            title: title.into(),
            ..Default::default()
        }
    }

    #[test]
    fn top_result_prefers_an_exact_match_then_a_prefix() {
        let artists = [artist("Harbor Lights"), artist("Neon Harbor")];
        let albums = [album("Harbor")];
        let tracks = [track("Low Tide"), track("Harbor Song")];
        assert_eq!(
            top_result("harbor", &artists, &albums, &tracks),
            Some(TopResult::Album(album("Harbor")))
        );
        assert_eq!(
            top_result("harb", &artists, &albums, &tracks),
            Some(TopResult::Artist(artist("Harbor Lights")))
        );
        assert_eq!(
            top_result("harbor s", &artists, &albums, &tracks),
            Some(TopResult::Track(1, track("Harbor Song")))
        );
    }

    #[test]
    fn top_result_falls_back_to_the_best_ranked_song() {
        let tracks = [track("Low Tide")];
        assert_eq!(
            top_result("tide", &[artist("X")], &[album("Y")], &tracks),
            Some(TopResult::Track(0, track("Low Tide")))
        );
        assert_eq!(top_result("  ", &[], &[], &tracks), None);
        assert_eq!(top_result("q", &[], &[], &[]), None);
    }

    #[test]
    fn highlights_cover_word_prefixes_of_each_query_word() {
        assert_eq!(highlight_ranges("Neon Harbor", "harb"), vec![5..9]);
        assert_eq!(highlight_ranges("Neon Harbor", "ne ha"), vec![0..2, 5..7]);
        assert_eq!(
            highlight_ranges("Seaharbor", "harb"),
            Vec::<Range<usize>>::new()
        );
        assert_eq!(highlight_ranges("Ótima", "óti"), vec![0..4]);
        assert_eq!(highlight_ranges("ab", "abc"), Vec::<Range<usize>>::new());
        assert_eq!(highlight_ranges("Low", ""), Vec::<Range<usize>>::new());
    }
}
