//! Search query translation and BM25 search over the SQLite FTS index.

use crate::db;
use crate::models::ArtistEntry;
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashMap;

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
}
