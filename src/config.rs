use anyhow::{anyhow, Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::sequence::compile_pattern;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub patterns: Vec<String>,
}

pub fn load_config() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let txt = fs::read_to_string(&path).context("Failed to read config")?;
    let cfg: Config = toml::from_str(&txt).context("Failed to parse config TOML")?;
    Ok(cfg)
}

pub fn save_config(cfg: &Config) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let txt = toml::to_string_pretty(cfg).context("Failed to serialize config TOML")?;
    fs::write(&path, txt).context("Failed to write config")?;
    Ok(())
}

pub fn config_path() -> Result<PathBuf> {
    let proj = ProjectDirs::from("dev", "zapvis", "zapvis")
        .ok_or_else(|| anyhow!("Could not determine config directory"))?;
    Ok(proj.config_dir().join("config.toml"))
}

pub fn maybe_add_pattern(cfg: &mut Config, pat: String) {
    if !cfg.patterns.iter().any(|p| p == &pat) {
        cfg.patterns.push(pat);
    }
}

pub fn pattern_matches_file(pat: &str, file_name: &str) -> Result<bool> {
    let (re, _, _, _) = compile_pattern(pat)?;
    Ok(re.is_match(file_name))
}
