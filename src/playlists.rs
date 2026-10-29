use crate::config::AppConfig;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Playlist {
    pub id: u64,
    pub name: String,
    pub tracks: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Playlists {
    next_id: u64,
    pub items: Vec<Playlist>,
}

impl Playlists {
    pub fn path() -> PathBuf {
        AppConfig::path().with_file_name("playlists.json")
    }

    pub fn load() -> Self {
        std::fs::read(Self::path())
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = Self::path();
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| {
                let data = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
                std::fs::write(&path, data)
            });
        if let Err(err) = result {
            eprintln!("save playlists: {err}");
        }
    }

    pub fn get(&self, id: u64) -> Option<&Playlist> {
        self.items.iter().find(|p| p.id == id)
    }

    pub fn create(&mut self, name: &str, tracks: Vec<PathBuf>) -> u64 {
        self.next_id = self
            .next_id
            .max(self.items.iter().map(|p| p.id).max().unwrap_or(0))
            + 1;
        let id = self.next_id;
        self.items.push(Playlist {
            id,
            name: name.trim().to_string(),
            tracks,
        });
        id
    }

    pub fn rename(&mut self, id: u64, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        if let Some(playlist) = self.items.iter_mut().find(|p| p.id == id) {
            playlist.name = name.to_string();
        }
    }

    pub fn delete(&mut self, id: u64) {
        self.items.retain(|p| p.id != id);
    }

    pub fn add(&mut self, id: u64, tracks: impl IntoIterator<Item = PathBuf>) {
        if let Some(playlist) = self.items.iter_mut().find(|p| p.id == id) {
            playlist.tracks.extend(tracks);
        }
    }

    pub fn remove(&mut self, id: u64, tracks: &[PathBuf]) {
        let Some(playlist) = self.items.iter_mut().find(|p| p.id == id) else {
            return;
        };
        for path in tracks {
            if let Some(at) = playlist.tracks.iter().position(|p| p == path) {
                playlist.tracks.remove(at);
            }
        }
    }

    pub fn untitled_name(&self) -> String {
        let taken = |name: &str| self.items.iter().any(|p| p.name == name);
        if !taken("Untitled Playlist") {
            return "Untitled Playlist".into();
        }
        (2..)
            .map(|n| format!("Untitled Playlist {n}"))
            .find(|name| !taken(name))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn playlists_keep_order_ids_and_duplicates() {
        let mut lists = Playlists::default();
        let a = lists.create(" Road Trip ", paths(&["/a", "/b"]));
        let b = lists.create("Chill", Vec::new());
        assert_ne!(a, b);
        assert_eq!(lists.get(a).unwrap().name, "Road Trip");
        lists.add(a, paths(&["/a", "/c"]));
        assert_eq!(
            lists.get(a).unwrap().tracks,
            paths(&["/a", "/b", "/a", "/c"])
        );
        lists.remove(a, &paths(&["/a"]));
        assert_eq!(lists.get(a).unwrap().tracks, paths(&["/b", "/a", "/c"]));
        lists.rename(b, "   ");
        assert_eq!(lists.get(b).unwrap().name, "Chill");
        lists.rename(b, "Focus");
        assert_eq!(lists.get(b).unwrap().name, "Focus");
        lists.delete(a);
        assert!(lists.get(a).is_none());
        let c = lists.create("New", Vec::new());
        assert!(c > b, "ids are never reused");
    }

    #[test]
    fn untitled_names_count_up() {
        let mut lists = Playlists::default();
        assert_eq!(lists.untitled_name(), "Untitled Playlist");
        lists.create("Untitled Playlist", Vec::new());
        assert_eq!(lists.untitled_name(), "Untitled Playlist 2");
    }

    #[test]
    fn playlists_round_trip_through_json() {
        let mut lists = Playlists::default();
        lists.create("Mix", paths(&["/x.flac"]));
        let text = serde_json::to_string(&lists).unwrap();
        assert_eq!(serde_json::from_str::<Playlists>(&text).unwrap(), lists);
    }
}
