use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Track {
    #[serde(with = "pathbuf_serde")]
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub track_number: u32,
    pub year: u32,
    pub duration: f64,
    pub album_id: String,
    #[serde(default)]
    pub genre: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Album {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub year: u32,
    #[serde(with = "arc_vec_serde")]
    pub tracks: Arc<[Track]>,
    #[serde(default)]
    pub unmatched: bool,
    #[serde(default)]
    pub genre: String,
}

/// An album without its tracks, for grids and artist pages that only need
/// cover art, title, artist and a track count.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AlbumSummary {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub year: u32,
    pub track_count: usize,
    #[serde(default)]
    pub unmatched: bool,
}

/// A row in the Artists view: aggregate counts plus a representative album
/// for the cover art.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ArtistEntry {
    pub name: String,
    pub album_count: usize,
    pub song_count: usize,
    pub cover_album_id: Option<String>,
}

mod pathbuf_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::path::{Path, PathBuf};

    pub fn serialize<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        path.to_string_lossy().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(PathBuf::from(s))
    }
}

mod arc_vec_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::sync::Arc;

    pub fn serialize<T, S>(arc: &Arc<[T]>, serializer: S) -> Result<S::Ok, S::Error>
    where
        T: Serialize,
        S: Serializer,
    {
        arc.as_ref().serialize(serializer)
    }

    pub fn deserialize<'de, T, D>(deserializer: D) -> Result<Arc<[T]>, D::Error>
    where
        T: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        let vec: Vec<T> = Vec::deserialize(deserializer)?;
        Ok(vec.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    RecentlyAdded,
    Songs,
    Albums,
    Artists,
    Genres,
    Liked,
    Playlist(u64),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GenreEntry {
    pub name: String,
    pub song_count: usize,
    pub album_count: usize,
    /// Up to four albums with cover art, the ones with most songs in the
    /// genre first, for the tile mosaic.
    pub cover_album_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SongColumn {
    #[default]
    Album,
    Title,
    Artist,
    Genre,
    Time,
    Added,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SongSort {
    pub column: SongColumn,
    pub descending: bool,
}

impl SongSort {
    pub const RECENT: SongSort = SongSort {
        column: SongColumn::Added,
        descending: true,
    };

    pub fn toggled(self, column: SongColumn) -> Self {
        if self.column == column {
            Self {
                column,
                descending: !self.descending,
            }
        } else {
            Self {
                column,
                descending: column == SongColumn::Added,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlbumSort {
    #[default]
    Artist,
    Title,
    Year,
    RecentlyAdded,
}

impl AlbumSort {
    pub const ALL: [AlbumSort; 4] = [
        AlbumSort::Artist,
        AlbumSort::Title,
        AlbumSort::Year,
        AlbumSort::RecentlyAdded,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AlbumSort::Artist => "Artist",
            AlbumSort::Title => "Title",
            AlbumSort::Year => "Year",
            AlbumSort::RecentlyAdded => "Recently Added",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlbumView {
    #[default]
    Grid,
    List,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

impl RepeatMode {
    pub fn next(&self) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayerState {
    #[default]
    Stopped,
    Playing,
    Paused,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicking_a_column_sorts_by_it_then_flips_direction() {
        let sort = SongSort::default().toggled(SongColumn::Title);
        assert_eq!(
            sort,
            SongSort {
                column: SongColumn::Title,
                descending: false
            }
        );
        assert!(sort.toggled(SongColumn::Title).descending);
        assert!(
            !sort
                .toggled(SongColumn::Title)
                .toggled(SongColumn::Title)
                .descending
        );
        assert_eq!(sort.toggled(SongColumn::Added), SongSort::RECENT);
    }

    #[test]
    fn repeat_mode_cycles_off_all_one() {
        assert_eq!(RepeatMode::Off.next(), RepeatMode::All);
        assert_eq!(RepeatMode::All.next(), RepeatMode::One);
        assert_eq!(RepeatMode::One.next(), RepeatMode::Off);
    }
}
