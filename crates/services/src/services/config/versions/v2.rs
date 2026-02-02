use std::path::PathBuf;

use executors::profile::ExecutorProfileId;
use serde::{Deserialize, Serialize};
use strum_macros::EnumString;
use ts_rs::TS;
use utils::{
    assets::{default_config, SoundAssets},
    cache_dir,
};

pub use crate::services::config::editor::{EditorConfig, EditorType};

#[derive(Clone, Debug, Serialize, Deserialize, TS, Default)]
pub struct ProxyConfig {
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub no_proxy: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
pub struct TelegramConfig {
    pub enabled: bool,
    pub bot_token: Option<String>,
    pub chat_id: Option<i64>,
    #[serde(default = "default_telegram_executor")]
    pub default_executor: String,
}

fn default_telegram_executor() -> String {
    "CLAUDE_CODE".to_string()
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token: None,
            chat_id: None,
            default_executor: default_telegram_executor(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, TS, Default)]
pub struct ShowcaseState {
    #[serde(default)]
    pub seen_features: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, EnumString)]
#[ts(use_ts_enum)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[strum(serialize_all = "SCREAMING_SNAKE_CASE")]
pub enum ThemeMode {
    Light,
    Dark,
    System,
}

impl Default for ThemeMode {
    fn default() -> Self {
        ThemeMode::System
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, TS, Default)]
#[ts(export)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum UiLanguage {
    #[default]
    Browser, // Detect from browser
    En,      // Force English
    Ja,      // Force Japanese
    Es,      // Force Spanish
    Ko,      // Force Korean
    ZhHans,  // Force Simplified Chinese
    ZhHant,  // Force Traditional Chinese
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, TS, Default, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PowerMode {
    #[default]
    SystemDefault,
    KeepAwake,
    KeepScreenOn,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct NotificationConfig {
    pub sound_enabled: bool,
    pub push_enabled: bool,
    pub sound_file: SoundFile,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            sound_enabled: true,
            push_enabled: true,
            sound_file: SoundFile::CowMooing,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct GitHubConfig {
    pub pat: Option<String>,
    pub oauth_token: Option<String>,
    pub username: Option<String>,
    pub primary_email: Option<String>,
    pub default_pr_base: Option<String>,
}

impl Default for GitHubConfig {
    fn default() -> Self {
        Self {
            pat: None,
            oauth_token: None,
            username: None,
            primary_email: None,
            default_pr_base: Some("main".to_string()),
        }
    }
}

impl GitHubConfig {
    pub fn token(&self) -> Option<String> {
        self.pat
            .as_deref()
            .or(self.oauth_token.as_deref())
            .map(|s| s.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, EnumString)]
#[ts(use_ts_enum)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[strum(serialize_all = "SCREAMING_SNAKE_CASE")]
pub enum SoundFile {
    AbstractSound1,
    AbstractSound2,
    AbstractSound3,
    AbstractSound4,
    CowMooing,
    PhoneVibration,
    Rooster,
}

impl SoundFile {
    pub fn to_filename(&self) -> &'static str {
        match self {
            SoundFile::AbstractSound1 => "abstract-sound1.wav",
            SoundFile::AbstractSound2 => "abstract-sound2.wav",
            SoundFile::AbstractSound3 => "abstract-sound3.wav",
            SoundFile::AbstractSound4 => "abstract-sound4.wav",
            SoundFile::CowMooing => "cow-mooing.wav",
            SoundFile::PhoneVibration => "phone-vibration.wav",
            SoundFile::Rooster => "rooster.wav",
        }
    }

    // load the sound file from the embedded assets or cache
    pub async fn serve(&self) -> Result<rust_embed::EmbeddedFile, anyhow::Error> {
        match SoundAssets::get(self.to_filename()) {
            Some(content) => Ok(content),
            None => {
                tracing::error!("Sound file not found: {}", self.to_filename());
                Err(anyhow::anyhow!(
                    "Sound file not found: {}",
                    self.to_filename()
                ))
            }
        }
    }

    /// Get or create a cached sound file with the embedded sound data
    pub async fn get_path(&self) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
        use std::io::Write;

        let filename = self.to_filename();
        let cache_dir = cache_dir();
        let cached_path = cache_dir.join(format!("sound-{filename}"));

        // Check if cached file already exists and is valid
        if cached_path.exists() {
            // Verify file has content (basic validation)
            if let Ok(metadata) = std::fs::metadata(&cached_path)
                && metadata.len() > 0
            {
                return Ok(cached_path);
            }
        }

        // File doesn't exist or is invalid, create it
        let sound_data = SoundAssets::get(filename)
            .ok_or_else(|| format!("Embedded sound file not found: {filename}"))?
            .data;

        // Ensure cache directory exists
        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| format!("Failed to create cache directory: {e}"))?;

        let mut file = std::fs::File::create(&cached_path)
            .map_err(|e| format!("Failed to create cached sound file: {e}"))?;

        file.write_all(&sound_data)
            .map_err(|e| format!("Failed to write sound data to cached file: {e}"))?;

        drop(file); // Ensure file is closed

        Ok(cached_path)
    }
}

/// V2 Config - adds local_network_access, local_network_password, and telegram fields
#[derive(Clone, Debug, Serialize, Deserialize, TS)]
pub struct Config {
    pub config_version: String,
    pub theme: ThemeMode,
    pub executor_profile: ExecutorProfileId,
    pub disclaimer_acknowledged: bool,
    pub onboarding_acknowledged: bool,
    pub notifications: NotificationConfig,
    pub editor: EditorConfig,
    pub github: GitHubConfig,
    pub analytics_enabled: bool,
    pub workspace_dir: Option<String>,
    pub last_app_version: Option<String>,
    pub show_release_notes: bool,
    #[serde(default)]
    pub language: UiLanguage,
    #[serde(default)]
    pub git_branch_prefix: String,
    #[serde(default)]
    pub showcases: ShowcaseState,
    #[serde(default)]
    pub pr_auto_description_enabled: bool,
    #[serde(default)]
    pub pr_auto_description_prompt: Option<String>,
    #[serde(default)]
    pub beta_workspaces: bool,
    #[serde(default)]
    pub beta_workspaces_invitation_sent: bool,
    #[serde(default)]
    pub local_network_access: bool,
    #[serde(default)]
    pub local_network_password: Option<String>,
    #[serde(default)]
    pub commit_reminder: bool,
    #[serde(default)]
    pub proxy: ProxyConfig,
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub power_mode: PowerMode,
}

impl Default for Config {
    fn default() -> Self {
        let default_bytes = default_config();
        serde_json::from_slice(&default_bytes)
            .expect("Failed to parse embedded default_config.json")
    }
}

impl From<super::v1::Config> for Config {
    fn from(v1: super::v1::Config) -> Self {
        Self {
            config_version: "v2".to_string(),
            theme: match v1.theme {
                super::v1::ThemeMode::Light => ThemeMode::Light,
                super::v1::ThemeMode::Dark => ThemeMode::Dark,
                super::v1::ThemeMode::System => ThemeMode::System,
            },
            executor_profile: v1.executor_profile,
            disclaimer_acknowledged: v1.disclaimer_acknowledged,
            onboarding_acknowledged: v1.onboarding_acknowledged,
            notifications: NotificationConfig {
                sound_enabled: v1.notifications.sound_enabled,
                push_enabled: v1.notifications.push_enabled,
                sound_file: match v1.notifications.sound_file {
                    super::v1::SoundFile::AbstractSound1 => SoundFile::AbstractSound1,
                    super::v1::SoundFile::AbstractSound2 => SoundFile::AbstractSound2,
                    super::v1::SoundFile::AbstractSound3 => SoundFile::AbstractSound3,
                    super::v1::SoundFile::AbstractSound4 => SoundFile::AbstractSound4,
                    super::v1::SoundFile::CowMooing => SoundFile::CowMooing,
                    super::v1::SoundFile::PhoneVibration => SoundFile::PhoneVibration,
                    super::v1::SoundFile::Rooster => SoundFile::Rooster,
                },
            },
            editor: v1.editor,
            github: GitHubConfig {
                pat: v1.github.pat,
                oauth_token: v1.github.oauth_token,
                username: v1.github.username,
                primary_email: v1.github.primary_email,
                default_pr_base: v1.github.default_pr_base,
            },
            analytics_enabled: v1.analytics_enabled,
            workspace_dir: v1.workspace_dir,
            last_app_version: v1.last_app_version,
            show_release_notes: v1.show_release_notes,
            language: match v1.language {
                super::v1::UiLanguage::Browser => UiLanguage::Browser,
                super::v1::UiLanguage::En => UiLanguage::En,
                super::v1::UiLanguage::Ja => UiLanguage::Ja,
                super::v1::UiLanguage::Es => UiLanguage::Es,
                super::v1::UiLanguage::Ko => UiLanguage::Ko,
                super::v1::UiLanguage::ZhHans => UiLanguage::ZhHans,
                super::v1::UiLanguage::ZhHant => UiLanguage::ZhHant,
            },
            git_branch_prefix: v1.git_branch_prefix,
            showcases: ShowcaseState {
                seen_features: v1.showcases.seen_features,
            },
            pr_auto_description_enabled: v1.pr_auto_description_enabled,
            pr_auto_description_prompt: v1.pr_auto_description_prompt,
            beta_workspaces: v1.beta_workspaces,
            beta_workspaces_invitation_sent: v1.beta_workspaces_invitation_sent,
            commit_reminder: v1.commit_reminder,
            proxy: ProxyConfig {
                http_proxy: v1.proxy.http_proxy,
                https_proxy: v1.proxy.https_proxy,
                no_proxy: v1.proxy.no_proxy,
            },
            // New fields with defaults
            local_network_access: false,
            local_network_password: None,
            telegram: TelegramConfig::default(),
            power_mode: PowerMode::default(),
        }
    }
}
