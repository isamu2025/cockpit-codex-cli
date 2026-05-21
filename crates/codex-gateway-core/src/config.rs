use anyhow::{Context, Result};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_UPSTREAM_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8080;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_upstream_base_url")]
    pub upstream_base_url: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            upstream_base_url: default_upstream_base_url(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GatewayKey(pub String);

fn default_host() -> String {
    DEFAULT_HOST.to_string()
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_upstream_base_url() -> String {
    DEFAULT_UPSTREAM_BASE_URL.to_string()
}

pub fn default_data_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot resolve home directory")?;
    Ok(home.join(".cockpit-codex"))
}

pub fn ensure_data_dir(data_dir: &Path) -> Result<()> {
    fs::create_dir_all(data_dir.join("accounts"))
        .with_context(|| format!("create data directory {}", data_dir.display()))
}

pub fn load_config(data_dir: &Path) -> Result<Config> {
    ensure_data_dir(data_dir)?;
    let path = data_dir.join("config.toml");
    if !path.exists() {
        let config = Config::default();
        save_config(data_dir, &config)?;
        return Ok(config);
    }
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?)
}

pub fn save_config(data_dir: &Path, config: &Config) -> Result<()> {
    ensure_data_dir(data_dir)?;
    let path = data_dir.join("config.toml");
    let text = toml::to_string_pretty(config)?;
    fs::write(&path, text).with_context(|| format!("write {}", path.display()))
}

pub fn load_or_create_gateway_key(data_dir: &Path) -> Result<GatewayKey> {
    ensure_data_dir(data_dir)?;
    let path = data_dir.join("gateway.key");
    if path.exists() {
        let key = fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?
            .trim()
            .to_string();
        if !key.is_empty() {
            return Ok(GatewayKey(key));
        }
    }
    rotate_gateway_key(data_dir)
}

pub fn rotate_gateway_key(data_dir: &Path) -> Result<GatewayKey> {
    ensure_data_dir(data_dir)?;
    let key: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(48)
        .map(char::from)
        .collect();
    let path = data_dir.join("gateway.key");
    fs::write(&path, format!("{}\n", key)).with_context(|| format!("write {}", path.display()))?;
    Ok(GatewayKey(key))
}

