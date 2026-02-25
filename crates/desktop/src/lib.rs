//! Vibe Kanban Desktop Application
//!
//! This crate provides the Tauri-based desktop and mobile application.
//! Platform-specific code is split into separate modules:
//! - `desktop`: Embedded server, LAN server, and discovery multicast (non-Android)
//! - `android`: Server discovery via multicast (Android only)

#[cfg(target_os = "android")]
mod android;
#[cfg(not(target_os = "android"))]
mod cli;
#[cfg(not(target_os = "android"))]
mod cli_install;
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

#[cfg(not(target_os = "android"))]
#[derive(Debug)]
pub enum LaunchError {
    Cli(cli::CliError),
    Runtime(anyhow::Error),
}

#[cfg(not(target_os = "android"))]
impl LaunchError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Cli(_) => 2,
            Self::Runtime(_) => 1,
        }
    }
}

#[cfg(not(target_os = "android"))]
impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cli(err) => write!(f, "{err}"),
            Self::Runtime(err) => write!(f, "{err}"),
        }
    }
}

#[cfg(not(target_os = "android"))]
impl std::error::Error for LaunchError {}

/// Entry point for desktop binaries that support `--server` mode.
#[cfg(not(target_os = "android"))]
pub fn run_from_cli_env() -> Result<(), LaunchError> {
    let args = cli::CliArgs::parse_env().map_err(LaunchError::Cli)?;

    if args.server {
        install_rustls_provider();
        init_logging();
        return desktop::run_server_mode_blocking(args.port).map_err(LaunchError::Runtime);
    }

    run();
    Ok(())
}

/// Entry point for the Tauri application.
#[cfg_attr(
    any(target_os = "android", target_os = "ios"),
    tauri::mobile_entry_point
)]
pub fn run() {
    install_rustls_provider();

    // Initialize logging
    init_logging();

    #[cfg(not(target_os = "android"))]
    if let Err(err) = cli_install::bootstrap_cli_path() {
        tracing::warn!("Failed to bootstrap desktop CLI in PATH: {err}");
    }

    run_tauri_app();
}

fn run_tauri_app() {
    let builder = tauri::Builder::default();

    #[cfg(not(target_os = "android"))]
    let builder =
        builder.invoke_handler(tauri::generate_handler![desktop::save_export_context_file]);

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

fn install_rustls_provider() {
    // Install rustls crypto provider before any TLS operations
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
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
