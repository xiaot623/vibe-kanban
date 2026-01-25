use std::sync::Arc;

use server::startup::{self, ServerConfig};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::Mutex;
use tracing_subscriber::{prelude::*, EnvFilter};

pub fn run() {
    // Install rustls crypto provider before any TLS operations
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Initialize logging
    init_logging();

    tauri::Builder::default()
        .setup(|app| {
            let app_handle = app.handle().clone();

            // Spawn the embedded server
            tauri::async_runtime::spawn(async move {
                match start_embedded_server(&app_handle).await {
                    Ok(()) => {
                        tracing::info!("Embedded server started successfully");
                    }
                    Err(e) => {
                        tracing::error!("Failed to start embedded server: {}", e);
                    }
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

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

async fn start_embedded_server(app_handle: &tauri::AppHandle) -> anyhow::Result<()> {
    // Initialize deployment
    let deployment = startup::initialize_deployment().await?;
    startup::spawn_background_services(&deployment, "desktop").await;

    // Start embedded server (no browser open, no port file in desktop mode)
    let config = ServerConfig {
        port: 0, // auto-assign
        host: "127.0.0.1".to_string(),
        open_browser: false,
        write_port_file: false,
    };

    let (port, _server_handle) = startup::start_server(deployment.clone(), config).await?;

    tracing::info!("Embedded server running on port {}", port);

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

    // Store deployment for cleanup on exit
    app_handle.manage(Arc::new(Mutex::new(Some(deployment))));

    Ok(())
}
