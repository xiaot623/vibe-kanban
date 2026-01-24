use server::startup::{self, ServerConfig, StartupError};
use strip_ansi_escapes::strip;

#[tokio::main]
async fn main() -> Result<(), StartupError> {
    // Install rustls crypto provider before any TLS operations
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    startup::init_logging();

    let deployment = startup::initialize_deployment().await?;
    startup::spawn_background_services(&deployment, "cli").await;

    let config = ServerConfig {
        port: resolve_port(),
        host: resolve_host(),
        open_browser: !cfg!(debug_assertions),
        write_port_file: true,
    };

    let (_port, server_handle) = startup::start_server(deployment.clone(), config).await?;

    // Wait for shutdown signal
    shutdown_signal().await;

    // Abort the server task
    server_handle.abort();

    startup::cleanup(&deployment).await;

    Ok(())
}

pub async fn shutdown_signal() {
    // Always wait for Ctrl+C
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("Failed to install Ctrl+C handler: {e}");
        }
    };

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        // Try to install SIGTERM handler, but don't panic if it fails
        let terminate = async {
            if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
                sigterm.recv().await;
            } else {
                tracing::error!("Failed to install SIGTERM handler");
                // Fallback: never resolves
                std::future::pending::<()>().await;
            }
        };

        tokio::select! {
            _ = ctrl_c => {},
            _ = terminate => {},
        }
    }

    #[cfg(not(unix))]
    {
        // Only ctrl_c is available, so just await it
        ctrl_c.await;
    }
}

fn resolve_port() -> u16 {
    let cli_port = parse_cli_port();
    std::env::var("BACKEND_PORT")
        .ok()
        .and_then(|s| parse_port_value(&s))
        .or_else(|| cli_port)
        .or_else(|| std::env::var("PORT").ok().and_then(|s| parse_port_value(&s)))
        .unwrap_or_else(|| {
            tracing::info!("No PORT environment variable set, using port 0 for auto-assignment");
            0
        })
}

fn resolve_host() -> String {
    std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string())
}

fn parse_cli_port() -> Option<u16> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--port=") {
            return parse_port_value(value);
        }

        if arg == "--port" {
            let value = args.next();
            return value.as_deref().and_then(parse_port_value);
        }
    }

    None
}

fn parse_port_value(raw: &str) -> Option<u16> {
    let cleaned = String::from_utf8(strip(raw.as_bytes())).ok()?;
    cleaned.trim().parse::<u16>().ok()
}
