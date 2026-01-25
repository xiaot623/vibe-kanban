use std::sync::Arc;

use tokio::sync::RwLock;
use utils::{self, log_msg::LogMsg, msg_store::MsgStore};

use crate::services::config::{Config, SoundFile};

/// Trait for handling notifications
#[async_trait::async_trait]
pub trait Notifier: Send + Sync + std::fmt::Debug {
    async fn notify(&self, title: &str, message: &str);
}

/// Service for handling sound notifications and SSE notifications
#[derive(Debug, Clone)]
pub struct NotificationService {
    config: Arc<RwLock<Config>>,
    msg_store: Option<Arc<MsgStore>>,
}

impl NotificationService {
    pub fn new(config: Arc<RwLock<Config>>, msg_store: Option<Arc<MsgStore>>) -> Self {
        Self { config, msg_store }
    }

    /// Send sound notification if enabled and broadcast to web clients via SSE
    pub async fn notify(&self, title: &str, message: &str) {
        let config = self.config.read().await.notifications.clone();

        // Broadcast to web clients via SSE if push notifications are enabled and msg_store is available
        if config.push_enabled {
            if let Some(msg_store) = &self.msg_store {
                msg_store.push(LogMsg::Notification(title.to_string(), message.to_string()));
            }
        }

        if config.sound_enabled {
            Self::play_sound_notification(&config.sound_file).await;
        }
    }

    /// Play a system sound notification across platforms
    async fn play_sound_notification(sound_file: &SoundFile) {
        let file_path = match sound_file.get_path().await {
            Ok(path) => path,
            Err(e) => {
                tracing::error!("Failed to create cached sound file: {}", e);
                return;
            }
        };

        // Use platform-specific sound notification
        // Note: spawn() calls are intentionally not awaited - sound notifications should be fire-and-forget
        if cfg!(target_os = "macos") {
            let _ = tokio::process::Command::new("afplay")
                .arg(&file_path)
                .spawn();
        } else if cfg!(target_os = "linux") && !utils::is_wsl2() {
            // Try different Linux audio players
            if tokio::process::Command::new("paplay")
                .arg(&file_path)
                .spawn()
                .is_ok()
            {
                // Success with paplay
            } else if tokio::process::Command::new("aplay")
                .arg(&file_path)
                .spawn()
                .is_ok()
            {
                // Success with aplay
            } else {
                // Try system bell as fallback
                let _ = tokio::process::Command::new("echo")
                    .arg("-e")
                    .arg("\\a")
                    .spawn();
            }
        } else if cfg!(target_os = "windows") || (cfg!(target_os = "linux") && utils::is_wsl2()) {
            // Convert WSL path to Windows path if in WSL2
            let file_path = if utils::is_wsl2() {
                if let Some(windows_path) = Self::wsl_to_windows_path(&file_path).await {
                    windows_path
                } else {
                    file_path.to_string_lossy().to_string()
                }
            } else {
                file_path.to_string_lossy().to_string()
            };

            let _ = tokio::process::Command::new("powershell.exe")
                .arg("-c")
                .arg(format!(
                    r#"(New-Object Media.SoundPlayer "{file_path}").PlaySync()"#
                ))
                .spawn();
        }
    }

    /// Convert WSL path to Windows UNC path for PowerShell
    async fn wsl_to_windows_path(wsl_path: &std::path::Path) -> Option<String> {
        let path_str = wsl_path.to_string_lossy();

        // Relative paths work fine as-is in PowerShell
        if !path_str.starts_with('/') {
            tracing::debug!("Using relative path as-is: {}", path_str);
            return Some(path_str.to_string());
        }

        // Get WSL root path via PowerShell
        match tokio::process::Command::new("powershell.exe")
            .arg("-c")
            .arg("(Get-Location).Path -replace '^.*::', ''")
            .current_dir("/")
            .output()
            .await
        {
            Ok(output) => {
                match String::from_utf8(output.stdout) {
                    Ok(pwd_str) => {
                        let wsl_root = pwd_str.trim();
                        // Simply concatenate WSL root with the absolute path - PowerShell doesn't mind /
                        let windows_path = format!("{wsl_root}{path_str}");
                        tracing::debug!("WSL path converted: {} -> {}", path_str, windows_path);
                        Some(windows_path)
                    }
                    Err(e) => {
                        tracing::error!("Failed to parse PowerShell pwd output as UTF-8: {}", e);
                        None
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to execute PowerShell pwd command: {}", e);
                None
            }
        }
    }
}
