use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use ts_rs::TS;

pub mod editor;
mod versions;

pub use editor::EditorOpenError;

/// Result of loading config - contains the config and optionally a parse error message
#[derive(Clone, Debug, Serialize, Deserialize, TS)]
pub struct ConfigLoadResult {
    pub config: Config,
    /// If config file exists but failed to parse, this contains the error message
    pub parse_error: Option<String>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("Validation error: {0}")]
    ValidationError(String),
}

pub type Config = versions::v1::Config;
pub type NotificationConfig = versions::v1::NotificationConfig;
pub type EditorConfig = versions::v1::EditorConfig;
pub type ThemeMode = versions::v1::ThemeMode;
pub type SoundFile = versions::v1::SoundFile;
pub type EditorType = versions::v1::EditorType;
pub type GitHubConfig = versions::v1::GitHubConfig;
pub type UiLanguage = versions::v1::UiLanguage;
pub type ShowcaseState = versions::v1::ShowcaseState;
pub type ProxyConfig = versions::v1::ProxyConfig;

/// Load config from file, creating default if not exists
/// Returns ConfigLoadResult which includes the config and optionally a parse error
pub async fn load_config_from_file(config_path: &PathBuf) -> ConfigLoadResult {
    use versions::v1::ConfigParseResult;

    if config_path.exists() {
        match std::fs::read_to_string(config_path) {
            Ok(raw_config) => {
                tracing::info!("Config file loaded from {:?}", config_path);
                match Config::try_from_string(&raw_config) {
                    ConfigParseResult::Ok(config) => ConfigLoadResult {
                        config,
                        parse_error: None,
                    },
                    ConfigParseResult::ParseError(error) => {
                        tracing::warn!(
                            "Config parse failed: {}, using default (file not overwritten)",
                            error
                        );
                        // Return default config but with the parse error
                        // DO NOT save/overwrite the config file
                        ConfigLoadResult {
                            config: Config::default(),
                            parse_error: Some(error),
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to read config file: {}, using default", e);
                ConfigLoadResult {
                    config: Config::default(),
                    parse_error: None,
                }
            }
        }
    } else {
        tracing::info!("No config file found, creating default at {:?}", config_path);
        let config = Config::default();
        if let Err(e) = save_config_to_file(&config, config_path).await {
            tracing::warn!("Failed to save default config: {}", e);
        }
        ConfigLoadResult {
            config,
            parse_error: None,
        }
    }
}

/// Saves the config to the given path
pub async fn save_config_to_file(
    config: &Config,
    config_path: &PathBuf,
) -> Result<(), ConfigError> {
    let raw_config = serde_json::to_string_pretty(config)?;
    std::fs::write(config_path, raw_config)?;
    Ok(())
}
