//! TOML configuration (Chunk 2).
//!
//! File: `~/.config/nexterm/nexterm.toml` (or `$XDG_CONFIG_HOME/nexterm/…`).
//! Missing file → defaults are used (and written back on `load` so users can
//! discover every knob). No root privileges, no system-wide writes.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Full daemon/CLI configuration (spec §21 shape).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub browser: BrowserConfig,
    #[serde(default)]
    pub localhost: LocalhostConfig,
    #[serde(default)]
    pub integration: IntegrationConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrowserConfig {
    #[serde(default = "default_url")]
    pub default_url: String,
    #[serde(default = "default_true")]
    pub reuse_tabs: bool,
    #[serde(default = "default_true")]
    pub preserve_sessions: bool,
    #[serde(default)]
    pub auto_open_localhost: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalhostConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_ports")]
    pub ports: Vec<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IntegrationConfig {
    #[serde(default = "default_mode")]
    pub preferred_mode: String,
}

fn default_url() -> String {
    "about:blank".to_string()
}
fn default_true() -> bool {
    true
}
fn default_ports() -> Vec<u16> {
    vec![3000, 4173, 5173, 8080]
}
fn default_mode() -> String {
    "auto".to_string()
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            default_url: default_url(),
            reuse_tabs: true,
            preserve_sessions: true,
            auto_open_localhost: false,
        }
    }
}

impl Default for LocalhostConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ports: default_ports(),
        }
    }
}

impl Default for IntegrationConfig {
    fn default() -> Self {
        Self {
            preferred_mode: default_mode(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            browser: BrowserConfig::default(),
            localhost: LocalhostConfig::default(),
            integration: IntegrationConfig::default(),
        }
    }
}

/// Load config from `path`. Creates parent dirs and writes defaults when the
/// file does not exist yet. Unknown fields are ignored (forward compatibility).
pub fn load(path: &std::path::Path) -> Result<Config> {
    if !path.exists() {
        let cfg = Config::default();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create config dir {}", parent.display()))?;
        }
        save(&cfg, path)?;
        return Ok(cfg);
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(cfg)
}

/// Persist config as TOML.
pub fn save(cfg: &Config, path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create config dir {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(cfg).context("serialize config")?;
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!(c.browser.default_url, "about:blank");
        assert!(c.browser.reuse_tabs);
        assert!(c.browser.preserve_sessions);
        assert!(!c.browser.auto_open_localhost);
        assert!(c.localhost.enabled);
        assert_eq!(c.localhost.ports, vec![3000, 4173, 5173, 8080]);
        assert_eq!(c.integration.preferred_mode, "auto");
    }

    #[test]
    fn save_load_roundtrips() {
        let dir = std::env::temp_dir().join(format!("nexterm-cfg-test-{}", std::process::id()));
        let path = dir.join("nexterm.toml");
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config::default();
        save(&cfg, &path).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(cfg, back);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_yields_defaults_and_writes_them() {
        let dir = std::env::temp_dir().join(format!("nexterm-cfg-missing-{}", std::process::id()));
        let path = dir.join("sub").join("nexterm.toml");
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = load(&path).unwrap();
        assert_eq!(cfg, Config::default());
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
