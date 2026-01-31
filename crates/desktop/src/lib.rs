//! Vibe Kanban Desktop Application
//!
//! This crate provides the Tauri-based desktop and mobile application.
//! Platform-specific code is split into separate modules:
//! - `desktop`: Embedded server, LAN server, and discovery multicast (non-Android)
//! - `android`: Server discovery via multicast (Android only)

#[cfg(target_os = "android")]
mod android;
#[cfg(not(target_os = "android"))]
mod desktop;

use serde::{Deserialize, Serialize};
use tracing_subscriber::{prelude::*, EnvFilter};

/// Port used for server discovery multicast.
pub(crate) const DISCOVERY_PORT: u16 = 57810;

/// Multicast address for server discovery.
pub(crate) const DISCOVERY_MULTICAST_ADDR: &str = "224.0.0.167";

/// Service identifier for discovery beacons.
pub(crate) const DISCOVERY_SERVICE: &str = "vibe-kanban";

/// Beacon payload broadcast by desktop servers for discovery.
#[derive(Serialize, Deserialize)]
pub(crate) struct DiscoveryBeacon {
    pub service: String,
    pub version: u8,
    pub port: u16,
    pub hostname: String,
    pub requires_auth: bool,
    pub fingerprint: String,
}

/// Entry point for the Tauri application.
#[cfg_attr(
    any(target_os = "android", target_os = "ios"),
    tauri::mobile_entry_point
)]
pub fn run() {
    // Install rustls crypto provider before any TLS operations
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Initialize logging
    init_logging();

    let builder = tauri::Builder::default();

    #[cfg(target_os = "android")]
    let builder = builder.invoke_handler(tauri::generate_handler![android::discover_servers]);

    builder
        .setup(|_app| {
            #[cfg(not(target_os = "android"))]
            {
                let app_handle = _app.handle().clone();
                // Spawn the embedded server
                tauri::async_runtime::spawn(async move {
                    match desktop::start_embedded_server(&app_handle).await {
                        Ok(()) => {
                            tracing::info!("Embedded server started successfully");
                        }
                        Err(e) => {
                            tracing::error!("Failed to start embedded server: {}", e);
                        }
                    }
                });
            }

            // Android: No server needed, mobile-ui handles connection to remote server
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Initialize the logging/tracing subsystem.
fn init_logging() {
    let log_level = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    let filter_string = format!(
        "warn,server={level},services={level},db={level},executors={level},deployment={level},local_deployment={level},utils={level},vibe_kanban_desktop={level}",
        level = log_level
    );
    let env_filter = EnvFilter::try_new(filter_string).expect("Failed to create tracing filter");
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(env_filter))
        .init();
}
