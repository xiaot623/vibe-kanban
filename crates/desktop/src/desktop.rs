//! Desktop-specific functionality for running embedded servers.

use std::{net::Ipv4Addr, sync::Arc, time::Duration};

use db::{
    models::task::{Task, TaskStatus},
    task_state::{
        dispatcher::shared_dispatcher,
        handler::{fn_handler, TransitionFilter},
    },
};
use deployment::Deployment;
use nosleep::{NoSleep, NoSleepType};
use serde::Deserialize;
use server::{
    startup::{self, ServerConfig},
    DeploymentImpl,
};
use services::services::config::{Config as AppConfig, PowerMode};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use tokio::{net::UdpSocket, sync::Mutex};

use crate::{DiscoveryBeacon, DISCOVERY_MULTICAST_ADDR, DISCOVERY_PORT, DISCOVERY_SERVICE};

const TAURI_SAVE_CANCELLED_ERROR: &str = "TAURI_SAVE_CANCELLED";

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportContextFormat {
    Md,
    Pdf,
}

#[tauri::command]
pub fn save_export_context_file(
    suggested_file_name: String,
    format: ExportContextFormat,
    bytes: Vec<u8>,
) -> Result<(), String> {
    let (filter_name, extension) = match format {
        ExportContextFormat::Md => ("Markdown Document", "md"),
        ExportContextFormat::Pdf => ("PDF Document", "pdf"),
    };

    let selected_path = rfd::FileDialog::new()
        .set_file_name(&suggested_file_name)
        .add_filter(filter_name, &[extension])
        .save_file()
        .ok_or_else(|| TAURI_SAVE_CANCELLED_ERROR.to_string())?;

    let output_path = if selected_path.extension().is_none() {
        selected_path.with_extension(extension)
    } else {
        selected_path
    };

    std::fs::write(&output_path, bytes).map_err(|err| format!("Failed to save file: {err}"))
}

/// State for managing the LAN server lifecycle.
struct LanServerState {
    handle: Option<tokio::task::JoinHandle<()>>,
    broadcast_handle: Option<tokio::task::JoinHandle<()>>,
    enabled: bool,
}

struct KeepAwakeState {
    nosleep: Option<NoSleep>,
    current_mode: PowerMode,
}

impl KeepAwakeState {
    fn new() -> Self {
        Self {
            nosleep: None,
            current_mode: PowerMode::SystemDefault,
        }
    }

    fn apply_mode(&mut self, mode: PowerMode) -> Result<(), Box<dyn std::error::Error>> {
        if mode == self.current_mode {
            return Ok(());
        }
        self.stop();

        match mode {
            PowerMode::SystemDefault => {
                tracing::info!("Power mode: System default");
            }
            PowerMode::KeepAwake => {
                let mut nosleep = NoSleep::new()?;
                nosleep.start(NoSleepType::PreventUserIdleSystemSleep)?;
                self.nosleep = Some(nosleep);
                tracing::info!("Power mode: Keep awake (preventing system sleep)");
            }
            PowerMode::KeepScreenOn => {
                let mut nosleep = NoSleep::new()?;
                nosleep.start(NoSleepType::PreventUserIdleDisplaySleep)?;
                self.nosleep = Some(nosleep);
                tracing::info!("Power mode: Keep screen on");
            }
        }

        self.current_mode = mode;
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(nosleep) = self.nosleep.take() {
            let _ = nosleep.stop();
        }
    }
}

/// Run desktop server mode (`kanban server`) without launching Tauri windows.
pub fn run_server_mode_blocking(port_override: Option<u16>) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_server_mode(port_override))
}

/// Start the backend in headless mode and wait for Ctrl+C / SIGTERM.
pub async fn run_server_mode(port_override: Option<u16>) -> anyhow::Result<()> {
    let deployment = startup::initialize_deployment().await?;
    startup::spawn_background_services(&deployment, "desktop-server").await;

    let config = server_mode_config(port_override);

    let (_port, server_handle) = startup::start_server(deployment.clone(), config)
        .await
        .map_err(anyhow::Error::from)?;

    startup::wait_for_shutdown_signal().await;
    server_handle.abort();
    startup::cleanup(&deployment).await;

    Ok(())
}

fn server_mode_config(port_override: Option<u16>) -> ServerConfig {
    ServerConfig {
        port: resolve_server_mode_port(port_override),
        host: "127.0.0.1".to_string(),
        open_browser: false,
        write_port_file: true,
        local_network_auth: false,
    }
}

/// Start the embedded server and set up the desktop application.
pub async fn start_embedded_server(app_handle: &tauri::AppHandle) -> anyhow::Result<()> {
    // Initialize deployment
    let deployment = startup::initialize_deployment().await?;
    startup::spawn_background_services(&deployment, "desktop").await;

    // Desktop server: always on 127.0.0.1, no auth, for local desktop use only
    let (port, _desktop_handle) = start_desktop_server(&deployment).await?;
    tracing::info!("Desktop server running on 127.0.0.1:{}", port);

    // Navigate the existing window to the embedded server
    let url = format!("http://127.0.0.1:{}", port);
    if let Some(window) = app_handle.get_webview_window("main") {
        window.navigate(url.parse()?)?;
        window.show()?;
    } else {
        // Fallback: create window if it doesn't exist
        WebviewWindowBuilder::new(app_handle, "main", WebviewUrl::External(url.parse()?))
            .title("Vibe Kanban")
            .inner_size(1400.0, 900.0)
            .min_inner_size(800.0, 600.0)
            .center()
            .visible(true)
            .build()?;
    }

    // Optionally start the LAN server if local network access is enabled
    let local_network = local_network_enabled(&deployment.config().read().await);
    let (lan_handle, broadcast_handle) = if local_network {
        let (lan_port, handle) = start_lan_server(&deployment).await?;
        tracing::info!("LAN server running on 0.0.0.0:{}", lan_port);
        let broadcast = start_discovery_multicast(lan_port, true);
        tracing::info!(
            "Discovery multicast started on {}:{}",
            DISCOVERY_MULTICAST_ADDR,
            DISCOVERY_PORT
        );
        (Some(handle), Some(broadcast))
    } else {
        (None, None)
    };

    let lan_state = Arc::new(Mutex::new(LanServerState {
        handle: lan_handle,
        broadcast_handle,
        enabled: local_network,
    }));

    spawn_local_network_watcher(deployment.clone(), lan_state);

    // Initialize keep-awake state
    let initial_power_mode = {
        let config_mode = deployment.config().read().await.power_mode;
        if config_mode != PowerMode::SystemDefault {
            config_mode
        } else {
            match Task::has_in_progress_tasks(&deployment.db().pool).await {
                Ok(true) => PowerMode::KeepAwake,
                _ => PowerMode::SystemDefault,
            }
        }
    };
    let keep_awake_state = Arc::new(Mutex::new(KeepAwakeState::new()));
    {
        let mut state = keep_awake_state.lock().await;
        if let Err(err) = state.apply_mode(initial_power_mode) {
            tracing::warn!("Failed to apply initial power mode: {}", err);
        }
    }
    register_keep_awake_handlers(deployment.clone(), keep_awake_state).await;

    // Store deployment for cleanup on exit
    app_handle.manage(Arc::new(Mutex::new(Some(deployment))));

    Ok(())
}

/// Check if local network access is enabled in the config.
fn local_network_enabled(config: &impl std::ops::Deref<Target = AppConfig>) -> bool {
    config.local_network_access
        && config
            .local_network_password
            .as_deref()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
}

/// Desktop server: binds to 127.0.0.1, no auth, for local desktop use only.
async fn start_desktop_server(
    deployment: &DeploymentImpl,
) -> anyhow::Result<(u16, tokio::task::JoinHandle<()>)> {
    let config = ServerConfig {
        port: 0,
        host: "127.0.0.1".to_string(),
        open_browser: false,
        write_port_file: false,
        local_network_auth: false,
    };

    startup::start_server(deployment.clone(), config)
        .await
        .map_err(anyhow::Error::from)
}

fn resolve_server_mode_port(cli_port: Option<u16>) -> u16 {
    cli_port
        .or_else(|| parse_port_from_env("BACKEND_PORT"))
        .or_else(|| parse_port_from_env("PORT"))
        .unwrap_or_else(|| {
            tracing::info!(
                "No --port, BACKEND_PORT, or PORT provided; using port 0 for auto-assignment"
            );
            0
        })
}

fn parse_port_from_env(name: &str) -> Option<u16> {
    let raw = std::env::var(name).ok()?;
    let trimmed = raw.trim();
    match trimmed.parse::<u16>() {
        Ok(port) => Some(port),
        Err(err) => {
            tracing::warn!("Ignoring invalid {name} value '{trimmed}': {err}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::server_mode_config;

    #[test]
    fn server_subcommand_does_not_open_browser() {
        let config = server_mode_config(Some(8080));
        assert_eq!(config.port, 8080);
        assert_eq!(config.host, "127.0.0.1");
        assert!(!config.open_browser);
        assert!(config.write_port_file);
    }
}

/// LAN server: binds to 0.0.0.0, requires Basic auth for non-loopback requests.
async fn start_lan_server(
    deployment: &DeploymentImpl,
) -> anyhow::Result<(u16, tokio::task::JoinHandle<()>)> {
    let config = ServerConfig {
        port: 0,
        host: "0.0.0.0".to_string(),
        open_browser: false,
        write_port_file: false,
        local_network_auth: true,
    };

    startup::start_server(deployment.clone(), config)
        .await
        .map_err(anyhow::Error::from)
}

/// Start the discovery multicast beacon to advertise the LAN server.
fn start_discovery_multicast(lan_port: u16, requires_auth: bool) -> tokio::task::JoinHandle<()> {
    let hostname = gethostname::gethostname().to_string_lossy().to_string();
    let fingerprint = uuid::Uuid::new_v4().to_string();
    let beacon = DiscoveryBeacon {
        service: DISCOVERY_SERVICE.to_string(),
        version: 1,
        port: lan_port,
        hostname,
        requires_auth,
        fingerprint,
    };

    tokio::spawn(async move {
        let multicast_addr: Ipv4Addr = match DISCOVERY_MULTICAST_ADDR.parse() {
            Ok(addr) => addr,
            Err(err) => {
                tracing::error!("Invalid multicast address: {}", err);
                return;
            }
        };

        let socket = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(socket) => socket,
            Err(err) => {
                tracing::error!("Failed to bind discovery multicast socket: {}", err);
                return;
            }
        };

        if let Err(err) = socket.join_multicast_v4(multicast_addr, Ipv4Addr::UNSPECIFIED) {
            tracing::error!("Failed to join discovery multicast group: {}", err);
            return;
        }

        if let Err(err) = socket.set_multicast_ttl_v4(1) {
            tracing::warn!("Failed to set discovery multicast TTL: {}", err);
        }

        let payload = match serde_json::to_vec(&beacon) {
            Ok(payload) => payload,
            Err(err) => {
                tracing::error!("Failed to serialize discovery beacon: {}", err);
                return;
            }
        };

        let target = format!("{}:{}", DISCOVERY_MULTICAST_ADDR, DISCOVERY_PORT);
        let mut interval = tokio::time::interval(Duration::from_secs(2));

        loop {
            interval.tick().await;
            if let Err(err) = socket.send_to(&payload, &target).await {
                tracing::warn!("Failed to send discovery beacon: {}", err);
            }
        }
    })
}

/// Watch for changes to local network settings and start/stop the LAN server accordingly.
fn spawn_local_network_watcher(deployment: DeploymentImpl, lan_state: Arc<Mutex<LanServerState>>) {
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            let current_enabled = local_network_enabled(&deployment.config().read().await);
            let mut state = lan_state.lock().await;
            if current_enabled == state.enabled {
                continue;
            }

            tracing::info!(
                "Local network access changed: {}",
                if current_enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            );

            if current_enabled {
                // Start LAN server
                match start_lan_server(&deployment).await {
                    Ok((lan_port, handle)) => {
                        tracing::info!("LAN server started on 0.0.0.0:{}", lan_port);
                        state.handle = Some(handle);
                        let broadcast = start_discovery_multicast(lan_port, true);
                        tracing::info!(
                            "Discovery multicast started on {}:{}",
                            DISCOVERY_MULTICAST_ADDR,
                            DISCOVERY_PORT
                        );
                        state.broadcast_handle = Some(broadcast);
                        state.enabled = true;
                    }
                    Err(err) => {
                        tracing::error!("Failed to start LAN server: {}", err);
                    }
                }
            } else {
                // Stop LAN server
                if let Some(handle) = state.handle.take() {
                    handle.abort();
                    tracing::info!("LAN server stopped");
                }
                if let Some(handle) = state.broadcast_handle.take() {
                    handle.abort();
                    tracing::info!("Discovery multicast stopped");
                }
                state.enabled = false;
            }
        }
    });
}

async fn register_keep_awake_handlers(
    deployment: DeploymentImpl,
    state: Arc<Mutex<KeepAwakeState>>,
) {
    let dispatcher = shared_dispatcher();

    {
        let state = state.clone();
        let deployment = deployment.clone();
        let handler = fn_handler(
            "KeepAwakeOnStart",
            TransitionFilter::new().to(vec![TaskStatus::InProgress]),
            move |_ctx, _transition| {
                let state = state.clone();
                let deployment = deployment.clone();
                Box::pin(async move {
                    let config_mode = deployment.config().read().await.power_mode;
                    if config_mode != PowerMode::SystemDefault {
                        return;
                    }

                    let mut state = state.lock().await;
                    if let Err(err) = state.apply_mode(PowerMode::KeepAwake) {
                        tracing::error!("Failed to enable keep-awake: {}", err);
                    }
                })
            },
        );
        dispatcher.register_handler(handler).await;
    }

    {
        let handler = fn_handler(
            "KeepAwakeOnFinish",
            TransitionFilter::new()
                .from(vec![TaskStatus::InProgress])
                .to(vec![
                    TaskStatus::Todo,
                    TaskStatus::InReview,
                    TaskStatus::Done,
                    TaskStatus::Cancelled,
                ]),
            move |ctx, _transition| {
                let state = state.clone();
                let deployment = deployment.clone();
                Box::pin(async move {
                    let config_mode = deployment.config().read().await.power_mode;
                    if config_mode != PowerMode::SystemDefault {
                        return;
                    }

                    let has_running = match Task::has_in_progress_tasks(&ctx.pool).await {
                        Ok(v) => v,
                        Err(err) => {
                            tracing::warn!("Failed to check in-progress tasks: {}", err);
                            return;
                        }
                    };

                    if !has_running {
                        let mut state = state.lock().await;
                        if let Err(err) = state.apply_mode(PowerMode::SystemDefault) {
                            tracing::error!("Failed to disable keep-awake: {}", err);
                        }
                    }
                })
            },
        );
        dispatcher.register_handler(handler).await;
    }
}
