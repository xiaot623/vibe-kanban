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

// Use v2 as the current config version
pub type Config = versions::v2::Config;
pub type NotificationConfig = versions::v2::NotificationConfig;
pub type EditorConfig = versions::v2::EditorConfig;
pub type ThemeMode = versions::v2::ThemeMode;
pub type SoundFile = versions::v2::SoundFile;
pub type EditorType = versions::v2::EditorType;
pub type GitHubConfig = versions::v2::GitHubConfig;
pub type UiLanguage = versions::v2::UiLanguage;
pub type ShowcaseState = versions::v2::ShowcaseState;
pub type ProxyConfig = versions::v2::ProxyConfig;
pub type TelegramConfig = versions::v2::TelegramConfig;
pub type PowerMode = versions::v2::PowerMode;

/// Load config from file, creating default if not exists
/// Returns ConfigLoadResult which includes the config and optionally a parse error
/// Handles migration from v1 to v2 automatically
pub async fn load_config_from_file(config_path: &PathBuf) -> ConfigLoadResult {
    if config_path.exists() {
        match std::fs::read_to_string(config_path) {
            Ok(raw_config) => {
                tracing::info!("Config file loaded from {:?}", config_path);

                // Try to detect config version
                let version = detect_config_version(&raw_config);

                match version.as_deref() {
                    Some("v2") => {
                        // Parse as v2 config
                        match serde_json::from_str::<Config>(&raw_config) {
                            Ok(config) => ConfigLoadResult {
                                config,
                                parse_error: None,
                            },
                            Err(e) => {
                                tracing::warn!("Failed to parse v2 config: {}, using default", e);
                                ConfigLoadResult {
                                    config: Config::default(),
                                    parse_error: Some(format!("Failed to parse v2 config: {}", e)),
                                }
                            }
                        }
                    }
                    Some("v1") => {
                        // Parse as v1 config and migrate to v2
                        match serde_json::from_str::<versions::v1::Config>(&raw_config) {
                            Ok(v1_config) => {
                                tracing::info!("Migrating config from v1 to v2");
                                let v2_config: Config = v1_config.into();

                                // Save the migrated config
                                if let Err(e) = save_config_to_file(&v2_config, config_path).await {
                                    tracing::warn!("Failed to save migrated config: {}", e);
                                } else {
                                    tracing::info!("Config migrated and saved successfully");
                                }

                                ConfigLoadResult {
                                    config: v2_config,
                                    parse_error: None,
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to parse v1 config: {}, using default", e);
                                ConfigLoadResult {
                                    config: Config::default(),
                                    parse_error: Some(format!("Failed to parse v1 config: {}", e)),
                                }
                            }
                        }
                    }
                    _ => {
                        tracing::warn!("Unknown config version, using default");
                        ConfigLoadResult {
                            config: Config::default(),
                            parse_error: Some("Unknown config version".to_string()),
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

/// Detect the config version from raw JSON string
fn detect_config_version(raw_config: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct VersionOnly {
        config_version: String,
    }

    serde_json::from_str::<VersionOnly>(raw_config)
        .ok()
        .map(|v| v.config_version)
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
