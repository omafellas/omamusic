use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::path::PathBuf;

thread_local! {
    static CONFIG_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub folders: Vec<PathBuf>,
    pub volume: f64,
    pub quit_on_close: bool,
    pub plain_tray_icon: bool,
    pub start_hidden: bool,
    pub tray_hint_shown: bool,
    /// UI scale factor applied to the base rem size. 1.0 is the default.
    pub ui_scale: f64,
    /// Music-reactive background strip; on by default, switchable in Settings.
    pub visual_effects: bool,
    pub online_lookup: bool,
    /// AcoustID application key for identifying untagged files by sound.
    /// Empty disables fingerprinting; a build-time `OMAMUSIC_ACOUSTID_KEY`
    /// provides the default.
    pub acoustid_key: String,
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
            ui_scale: 1.0,
            visual_effects: true,
            online_lookup: false,
            acoustid_key: default_acoustid_key(),
        }
    }
}

pub fn default_acoustid_key() -> String {
    option_env!("OMAMUSIC_ACOUSTID_KEY")
        .unwrap_or_default()
        .to_string()
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
    /// Resolution order: `use_dir` on this thread, `OMAMUSIC_CONFIG_DIR`,
    /// then the platform config directory.
    pub fn path() -> PathBuf {
        if let Some(dir) = CONFIG_DIR.with(|d| d.borrow().clone()) {
            return dir.join("config.json");
        }
        if let Some(dir) = std::env::var_os("OMAMUSIC_CONFIG_DIR") {
            return PathBuf::from(dir).join("config.json");
        }
        let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        base.join("omamusic").join("config.json")
    }

    /// Read and write the config under `dir` on the calling thread only. Tests
    /// use this so each one has its own file and never touches the user's.
    pub fn use_dir(dir: impl Into<PathBuf>) {
        CONFIG_DIR.with(|d| *d.borrow_mut() = Some(dir.into()));
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
    fn ui_scale_defaults_and_round_trips() {
        // Configs written before the setting existed still load as 1.0.
        let old: AppConfig = serde_json::from_str(r#"{"volume": 0.5}"#).unwrap();
        assert_eq!(old.ui_scale, 1.0);
        let cfg = AppConfig {
            ui_scale: 1.5,
            ..AppConfig::default()
        };
        let back: AppConfig = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.ui_scale, 1.5);
    }

    #[test]
    fn use_dir_redirects_reads_and_writes_on_this_thread() {
        let dir = std::env::temp_dir().join(format!("omamusic-use-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        AppConfig::use_dir(&dir);
        assert_eq!(AppConfig::path(), dir.join("config.json"));
        assert_eq!(AppConfig::load().ui_scale, 1.0);
        AppConfig::update(|c| c.ui_scale = 1.3);
        assert_eq!(AppConfig::load().ui_scale, 1.3);
        let written = std::fs::read_to_string(dir.join("config.json")).unwrap();
        assert!(written.contains("\"ui_scale\": 1.3"));
        let other = std::thread::spawn(AppConfig::path).join().unwrap();
        assert_ne!(other, dir.join("config.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn online_lookup_defaults_off_and_round_trips() {
        assert!(!AppConfig::default().online_lookup);
        let cfg: AppConfig = serde_json::from_str("{}").unwrap();
        assert!(!cfg.online_lookup);
        let cfg: AppConfig = serde_json::from_str(r#"{"online_lookup": true}"#).unwrap();
        assert!(cfg.online_lookup);
    }

    #[test]
    fn acoustid_key_round_trips_and_defaults_to_the_build_key() {
        let cfg: AppConfig = serde_json::from_str(r#"{"acoustid_key": "abc"}"#).unwrap();
        assert_eq!(cfg.acoustid_key, "abc");
        let cfg: AppConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.acoustid_key, default_acoustid_key());
        let back: AppConfig = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.acoustid_key, cfg.acoustid_key);
    }

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
