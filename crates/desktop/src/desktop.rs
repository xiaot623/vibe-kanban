//! Desktop-specific functionality for running embedded servers.

use std::{net::Ipv4Addr, sync::Arc, time::Duration};

use deployment::Deployment;
use nosleep::{NoSleep, NoSleepType};
use server::{
    startup::{self, ServerConfig},
    DeploymentImpl,
};
use services::services::config::{Config as AppConfig, PowerMode};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use tokio::{net::UdpSocket, sync::Mutex};

use crate::{DiscoveryBeacon, DISCOVERY_MULTICAST_ADDR, DISCOVERY_PORT, DISCOVERY_SERVICE};

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
    let initial_power_mode = deployment.config().read().await.power_mode;
    let keep_awake_state = Arc::new(Mutex::new(KeepAwakeState::new()));
    {
        let mut state = keep_awake_state.lock().await;
        if let Err(err) = state.apply_mode(initial_power_mode) {
            tracing::warn!("Failed to apply initial power mode: {}", err);
        }
    }
    spawn_keep_awake_watcher(deployment.clone(), keep_awake_state);

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

fn spawn_keep_awake_watcher(deployment: DeploymentImpl, state: Arc<Mutex<KeepAwakeState>>) {
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            let config_mode = deployment.config().read().await.power_mode;
            let mut state = state.lock().await;
            if config_mode != state.current_mode {
                if let Err(err) = state.apply_mode(config_mode) {
                    tracing::error!("Failed to apply power mode: {}", err);
                }
            }
        }
    });
}
