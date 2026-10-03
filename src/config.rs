#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::pricing::PricingTable;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub pricing: Option<PricingTable>,
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(p) = path else { return Ok(Self::default()) };
        if !p.exists() {
            return Ok(Self::default());
        }
        let s = std::fs::read_to_string(p)
            .with_context(|| format!("read config {}", p.display()))?;
        let cfg: Self = toml::from_str(&s)
            .with_context(|| format!("parse config {}", p.display()))?;
        Ok(cfg)
    }

    pub fn pricing_or_defaults(&self) -> PricingTable {
        self.pricing.clone().unwrap_or_else(PricingTable::defaults)
    }
}

pub fn default_config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "lmstudio-monitor")
        .map(|d| d.config_dir().join("config.toml"))
}

pub fn default_db_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "lmstudio-monitor")
        .map(|d| d.data_dir().join("usage.db"))
}

pub fn default_log_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "lmstudio-monitor")
        .map(|d| d.data_dir().join("lmstudio-monitor.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_path_returns_default() {
        let cfg = Config::load(None).unwrap();
        assert!(cfg.pricing.is_none());
    }

    #[test]
    fn nonexistent_path_returns_default() {
        let cfg = Config::load(Some(Path::new("/tmp/definitely-not-here-xyz.toml"))).unwrap();
        assert!(cfg.pricing.is_none());
    }

    #[test]
    fn pricing_or_defaults_loads_baked_when_absent() {
        let cfg = Config::default();
        let table = cfg.pricing_or_defaults();
        for key in crate::pricing::FRONTIER_MODELS {
            assert!(table.lookup(key).is_some());
        }
    }

    #[test]
    fn parses_overridden_pricing() {
        let toml_str = r#"
[pricing.providers.anthropic.models.claude-opus-4-8]
input_per_mtok_usd = 99.99
output_per_mtok_usd = 199.99
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        let table = cfg.pricing.unwrap();
        let p = table.lookup("claude-opus-4-8").unwrap();
        assert_eq!(p.input_per_mtok_usd, 99.99);
        assert_eq!(p.output_per_mtok_usd, 199.99);
    }
}
