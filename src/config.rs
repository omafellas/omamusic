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
}
