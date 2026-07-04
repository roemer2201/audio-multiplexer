//! Config persistence: %APPDATA%/audio-multiplexer/config.toml.
//!
//! Stale device IDs are kept in the config (the device may be temporarily
//! unplugged); consumers mark them unavailable instead of dropping them.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Endpoint ID of the loopback source; None means the default render
    /// device at engine start.
    pub source: Option<String>,
    #[serde(default)]
    pub targets: Vec<TargetConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetConfig {
    pub id: String,
    /// Friendly name at save time; display fallback while unplugged.
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_volume")]
    pub volume: u8,
    /// Reserved for the deferred per-device delay feature; not applied yet.
    #[serde(default)]
    pub delay_ms: u32,
}

fn default_volume() -> u8 {
    100
}

impl Config {
    pub fn target(&self, id: &str) -> Option<&TargetConfig> {
        self.targets.iter().find(|t| t.id == id)
    }

    pub fn target_mut(&mut self, id: &str) -> Option<&mut TargetConfig> {
        self.targets.iter_mut().find(|t| t.id == id)
    }
}

/// On Windows this resolves below %APPDATA% (roaming profile).
pub fn config_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("could not determine the user config directory")?;
    Ok(dir.join("audio-multiplexer").join("config.toml"))
}

/// Loads the config; a missing file yields the default (empty) config.
pub fn load() -> Result<Config> {
    let path = config_path()?;
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn save(config: &Config) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(config).context("serializing config")?;
    fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_all_fields() {
        let config = Config {
            source: Some("{id-source}".to_string()),
            targets: vec![TargetConfig {
                id: "{id-a}".to_string(),
                name: "Speakers".to_string(),
                volume: 80,
                delay_ms: 0,
            }],
        };
        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn missing_optional_fields_get_defaults() {
        let parsed: Config = toml::from_str(
            "[[targets]]\n\
             id = \"{id-a}\"\n",
        )
        .unwrap();
        assert_eq!(parsed.source, None);
        assert_eq!(parsed.targets[0].volume, 100);
        assert_eq!(parsed.targets[0].delay_ms, 0);
        assert_eq!(parsed.targets[0].name, "");
    }
}
