use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub folders: Vec<PathBuf>,
    pub volume: f64,
    pub quit_on_close: bool,
    pub plain_tray_icon: bool,
    pub start_hidden: bool,
    pub tray_hint_shown: bool,
    /// Music-reactive background strip; on by default, switchable in Settings.
    pub visual_effects: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            folders: Vec::new(),
            volume: 1.0,
            quit_on_close: false,
            plain_tray_icon: false,
            start_hidden: false,
            tray_hint_shown: false,
            visual_effects: true,
        }
    }
}

/// Where the system keeps music, if the platform exposes it.
fn music_dir() -> Option<PathBuf> {
    dirs::audio_dir().or_else(|| dirs::home_dir().map(|home| home.join("Music")))
}

/// First-run folders: `~/Music` when it exists, so the app works with no
/// setup. A saved config (even with an empty list) always wins, so removing
/// the folder in Settings persists.
fn folders_from(music: Option<PathBuf>) -> Vec<PathBuf> {
    music.filter(|dir| dir.is_dir()).into_iter().collect()
}

impl AppConfig {
    pub fn path() -> PathBuf {
        let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        base.join("omamusic").join("config.json")
    }

    pub fn load() -> Self {
        let p = Self::path();
        if let Ok(data) = std::fs::read(&p) {
            if let Ok(cfg) = serde_json::from_slice::<Self>(&data) {
                return cfg;
            }
        }
        Self {
            folders: folders_from(music_dir()),
            ..Self::default()
        }
    }

    pub fn update(change: impl FnOnce(&mut Self)) {
        let mut config = Self::load();
        change(&mut config);
        if let Err(err) = config.save() {
            eprintln!("save config: {err:#}");
        }
    }

    fn save(&self) -> Result<()> {
        let p = Self::path();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(&p, &data)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_effects_enabled_by_default() {
        assert!(AppConfig::default().visual_effects);
        let cfg: AppConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.visual_effects);
        let cfg: AppConfig = serde_json::from_str(r#"{"visual_effects": false}"#).unwrap();
        assert!(!cfg.visual_effects, "Settings can still turn it off");
    }

    #[test]
    fn default_folders_only_take_an_existing_music_dir() {
        let root = std::env::temp_dir().join(format!("omamusic-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let music = root.join("Music");
        assert_eq!(
            folders_from(Some(music.clone())),
            Vec::<PathBuf>::new(),
            "a missing ~/Music is not added"
        );
        assert_eq!(folders_from(None), Vec::<PathBuf>::new());
        std::fs::create_dir_all(&music).unwrap();
        assert_eq!(
            folders_from(Some(music.clone())).as_slice(),
            std::slice::from_ref(&music)
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_saved_empty_folder_list_stays_empty() {
        // A user who removed ~/Music in Settings must not get it back.
        let cfg: AppConfig = serde_json::from_str(r#"{"folders": [], "volume": 0.5}"#).unwrap();
        assert!(cfg.folders.is_empty());
        assert_eq!(cfg.volume, 0.5);
    }
}
