use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default)]
pub struct AppPaths {
    pub config_path: PathBuf,
    pub state_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl AppPaths {
    pub fn from_explicit(explicit: Option<PathBuf>) -> Result<Self> {
        let config_path = explicit.unwrap_or_else(default_config_path);
        let state_dir = default_state_dir()?;
        let cache_dir = default_cache_dir()?;
        Ok(Self {
            config_path,
            state_dir,
            cache_dir,
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    #[serde(skip, default)]
    pub paths: AppPaths,
    pub server: ServerConfig,
    pub telegram: TelegramConfig,
    pub codex: CodexConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub listen: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub allowed_user_ids: Vec<i64>,
    pub public_base_url: String,
    pub webhook_secret: String,
    #[serde(default = "default_telegram_api_base")]
    pub api_base_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CodexConfig {
    #[serde(default = "default_codex_connect_url")]
    pub connect_url: String,
    #[serde(default = "default_codex_listen_url")]
    pub listen_url: String,
    #[serde(default = "default_true")]
    pub reuse_existing_server: bool,
    pub working_directory: PathBuf,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub additional_directories: Vec<PathBuf>,
    #[serde(default = "default_approval_policy")]
    pub approval_policy: String,
    #[serde(default = "default_sandbox_mode")]
    pub sandbox_mode: String,
    #[serde(default = "default_skip_git_repo_check")]
    pub skip_git_repo_check: bool,
    #[serde(default = "default_network_access_enabled")]
    pub network_access_enabled: bool,
    #[serde(default = "default_web_search_enabled")]
    pub web_search_enabled: bool,
    #[serde(default)]
    pub orchestrator_name: Option<String>,
}

impl AppConfig {
    pub fn load(explicit_path: Option<PathBuf>) -> Result<Self> {
        let paths = AppPaths::from_explicit(explicit_path)?;
        let raw = fs::read_to_string(&paths.config_path).with_context(|| {
            format!(
                "failed to read config file at {}",
                paths.config_path.display()
            )
        })?;
        let mut config: AppConfig =
            toml::from_str(&raw).context("failed to parse config as TOML")?;
        validate(&config)?;
        fs::create_dir_all(&paths.state_dir)
            .with_context(|| format!("failed to create {}", paths.state_dir.display()))?;
        fs::create_dir_all(&paths.cache_dir)
            .with_context(|| format!("failed to create {}", paths.cache_dir.display()))?;
        config.paths = paths;
        Ok(config)
    }

    pub fn sessions_path(&self) -> PathBuf {
        self.paths.state_dir.join("sessions.json")
    }

    pub fn attachment_root(&self) -> PathBuf {
        self.paths.cache_dir.join("attachments")
    }

    pub fn webhook_path(&self) -> String {
        format!("/telegram/{}", self.telegram.webhook_secret)
    }

    pub fn webhook_url(&self) -> String {
        format!(
            "{}{}",
            self.telegram.public_base_url.trim_end_matches('/'),
            self.webhook_path()
        )
    }
}

fn validate(config: &AppConfig) -> Result<()> {
    if config.telegram.bot_token.trim().is_empty() {
        bail!("telegram.bot_token must not be empty");
    }
    if config.telegram.webhook_secret.trim().is_empty() {
        bail!("telegram.webhook_secret must not be empty");
    }
    if !config.telegram.public_base_url.starts_with("https://") {
        bail!("telegram.public_base_url must start with https://");
    }
    if !config.codex.working_directory.is_absolute() {
        bail!("codex.working_directory must be an absolute path");
    }
    Ok(())
}

fn default_config_path() -> PathBuf {
    if let Some(dir) = dirs::config_dir() {
        return dir.join("codexclaw").join("config.toml");
    }
    Path::new(".").join("codexclaw.toml")
}

fn default_state_dir() -> Result<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|dir| dir.join("codexclaw"))
        .context("failed to determine state directory")
}

fn default_cache_dir() -> Result<PathBuf> {
    dirs::cache_dir()
        .map(|dir| dir.join("codexclaw"))
        .context("failed to determine cache directory")
}

fn default_true() -> bool {
    true
}

fn default_telegram_api_base() -> String {
    "https://api.telegram.org".to_string()
}

fn default_codex_connect_url() -> String {
    "ws://127.0.0.1:4222".to_string()
}

fn default_codex_listen_url() -> String {
    "ws://127.0.0.1:4222".to_string()
}

fn default_approval_policy() -> String {
    "never".to_string()
}

fn default_sandbox_mode() -> String {
    "danger-full-access".to_string()
}

fn default_skip_git_repo_check() -> bool {
    true
}

fn default_network_access_enabled() -> bool {
    true
}

fn default_web_search_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_webhook_url() {
        let config: AppConfig = toml::from_str(
            r#"
[server]
listen = "127.0.0.1:8787"

[telegram]
bot_token = "token"
allowed_user_ids = [1]
public_base_url = "https://example.com/base"
webhook_secret = "secret"

[codex]
working_directory = "/tmp"
"#,
        )
        .expect("config");
        let mut config = config;
        config.paths = AppPaths {
            config_path: PathBuf::from("/tmp/config.toml"),
            state_dir: PathBuf::from("/tmp/state"),
            cache_dir: PathBuf::from("/tmp/cache"),
        };
        assert_eq!(config.webhook_path(), "/telegram/secret");
        assert_eq!(
            config.webhook_url(),
            "https://example.com/base/telegram/secret"
        );
    }
}
