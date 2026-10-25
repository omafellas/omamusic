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
        }
    }
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
        Self::default()
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
        std::fs::write(p, data)?;
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
        let dir = std::env::temp_dir().join(format!("omamusic-config-{}", std::process::id()));
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
}
