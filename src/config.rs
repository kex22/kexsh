use crossterm::event::{KeyCode, KeyModifiers};
use serde::Deserialize;
use std::path::PathBuf;

use crate::error::{KexshError, Result};

#[derive(Debug, Clone)]
pub struct PrefixKey {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl PrefixKey {
    pub fn display_name(&self) -> String {
        if self.modifiers.contains(KeyModifiers::CONTROL)
            && let KeyCode::Char(c) = self.code
        {
            return format!("Ctrl-{c}");
        }
        format!("{:?}", self.code)
    }

    pub fn to_config_string(&self) -> String {
        if self.modifiers.contains(KeyModifiers::CONTROL)
            && let KeyCode::Char(c) = self.code
        {
            return format!("ctrl-{c}");
        }
        format!("{:?}", self.code)
    }
}

impl Default for PrefixKey {
    fn default() -> Self {
        Self {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::CONTROL,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub prefix: PrefixKey,
    pub status_bar: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            prefix: PrefixKey::default(),
            status_bar: true,
        }
    }
}

#[derive(Deserialize, Default)]
struct RawConfig {
    #[serde(default)]
    keys: RawKeysConfig,
    #[serde(default)]
    ui: RawUiConfig,
}

#[derive(Deserialize)]
struct RawKeysConfig {
    #[serde(default = "default_prefix")]
    prefix: String,
}

impl Default for RawKeysConfig {
    fn default() -> Self {
        Self {
            prefix: default_prefix(),
        }
    }
}

fn default_prefix() -> String {
    "ctrl-a".into()
}

#[derive(Deserialize)]
struct RawUiConfig {
    #[serde(default = "default_true")]
    status_bar: bool,
}

impl Default for RawUiConfig {
    fn default() -> Self {
        Self { status_bar: true }
    }
}

fn default_true() -> bool {
    true
}

fn parse_prefix(s: &str) -> Result<PrefixKey> {
    let s = s.trim().to_lowercase();
    if let Some(ch) = s.strip_prefix("ctrl-") {
        let c = ch
            .chars()
            .next()
            .filter(|c| c.is_ascii_lowercase() && ch.len() == 1)
            .ok_or_else(|| KexshError::Config(format!("invalid prefix key: {s}")))?;
        return Ok(PrefixKey {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
        });
    }
    Err(KexshError::Config(format!(
        "unsupported prefix format: {s} (expected ctrl-<char>)"
    )))
}

pub fn config_path() -> PathBuf {
    dirs_config_path().unwrap_or_else(|| PathBuf::from("~/.config/kexsh/config.toml"))
}

fn dirs_config_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/kexsh/config.toml"))
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|e| KexshError::Config(format!("failed to read {}: {e}", path.display())))?;
        Self::from_toml(&content)
    }

    pub fn from_toml(content: &str) -> Result<Self> {
        let raw: RawConfig =
            toml::from_str(content).map_err(|e| KexshError::Config(format!("parse error: {e}")))?;
        let prefix = parse_prefix(&raw.keys.prefix)?;
        Ok(Self {
            prefix,
            status_bar: raw.ui.status_bar,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let cfg = Config::default();
        assert_eq!(cfg.prefix.code, KeyCode::Char('a'));
        assert_eq!(cfg.prefix.modifiers, KeyModifiers::CONTROL);
        assert!(cfg.status_bar);
    }

    #[test]
    fn parse_valid_toml() {
        let cfg = Config::from_toml(
            r#"
[keys]
prefix = "ctrl-b"

[ui]
status_bar = false
"#,
        )
        .unwrap();
        assert_eq!(cfg.prefix.code, KeyCode::Char('b'));
        assert_eq!(cfg.prefix.modifiers, KeyModifiers::CONTROL);
        assert!(!cfg.status_bar);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let cfg = Config::from_toml("").unwrap();
        assert_eq!(cfg.prefix.code, KeyCode::Char('a'));
        assert!(cfg.status_bar);
    }

    #[test]
    fn partial_config() {
        let cfg = Config::from_toml("[ui]\nstatus_bar = false\n").unwrap();
        assert_eq!(cfg.prefix.code, KeyCode::Char('a'));
        assert!(!cfg.status_bar);
    }

    #[test]
    fn invalid_prefix_errors() {
        assert!(Config::from_toml("[keys]\nprefix = \"alt-x\"\n").is_err());
        assert!(Config::from_toml("[keys]\nprefix = \"ctrl-\"\n").is_err());
        assert!(Config::from_toml("[keys]\nprefix = \"ctrl-ab\"\n").is_err());
    }

    #[test]
    fn invalid_toml_errors() {
        assert!(Config::from_toml("not valid toml [[[").is_err());
    }
}
