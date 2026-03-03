use std::fs;
use std::path::PathBuf;

use crate::error::{DgxTopError, Result};

use super::AppConfig;

/// Return the config directory path (~/.config/dgxtop/).
fn config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir()
        .ok_or_else(|| DgxTopError::Config("cannot determine user config directory".to_owned()))?;
    Ok(base.join("dgxtop"))
}

fn config_file_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

/// Load config from disk, returning defaults if file is missing.
pub fn load_config() -> AppConfig {
    let path = match config_file_path() {
        Ok(p) => p,
        Err(_) => return AppConfig::default(),
    };

    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return AppConfig::default(),
    };

    match serde_json::from_str::<AppConfig>(&contents) {
        Ok(mut config) => {
            config.sanitize();
            config
        }
        Err(e) => {
            eprintln!(
                "Warning: failed to parse config at {}: {e} — using defaults",
                path.display()
            );
            AppConfig::default()
        }
    }
}

/// Persist config to disk.
pub fn save_config(config: &AppConfig) -> Result<()> {
    let dir = config_dir()?;
    fs::create_dir_all(&dir)?;

    let path = dir.join("config.json");
    let json = serde_json::to_string_pretty(config)?;
    fs::write(path, json)?;
    Ok(())
}
