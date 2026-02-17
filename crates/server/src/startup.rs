use std::net::SocketAddr;

use axum::{
    ServiceExt,
    body::Body,
    extract::{ConnectInfo, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::Response,
};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use deployment::{Deployment, DeploymentError};
use services::services::container::ContainerService;
use sqlx::Error as SqlxError;
use thiserror::Error;
use tokio::task::JoinHandle;
use tracing_subscriber::{EnvFilter, prelude::*};
use utils::{
    assets::asset_dir,
    browser::open_browser,
    port_file::{set_shared_port, write_port_file},
    sentry::{self as sentry_utils, SentrySource, sentry_layer},
};

use crate::{DeploymentImpl, routes};

#[derive(Debug, Error)]
pub enum StartupError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sqlx(#[from] SqlxError),
    #[error(transparent)]
    Deployment(#[from] DeploymentError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Configuration for starting the server
pub struct ServerConfig {
    /// Port to bind to (0 for auto-assign)
    pub port: u16,
    /// Host to bind to
    pub host: String,
    /// Whether to open the browser after starting
    pub open_browser: bool,
    /// Whether to write the port file for discovery
    pub write_port_file: bool,
    /// Require password auth for non-loopback requests (local network access)
    pub local_network_auth: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 0,
            host: "127.0.0.1".to_string(),
            open_browser: false,
            write_port_file: true,
            local_network_auth: false,
        }
    }
}

/// Initialize logging with the standard configuration
pub fn init_logging() {
    let log_level = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    let filter_string = format!(
        "warn,server={level},services={level},db={level},executors={level},deployment={level},local_deployment={level},utils={level}",
        level = log_level
    );
    let env_filter = EnvFilter::try_new(filter_string).expect("Failed to create tracing filter");
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(env_filter))
        .with(sentry_layer())
        .init();
}

/// Initialize logging for desktop (without sentry layer initially)
pub fn init_logging_desktop() {
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

/// Initialize the deployment (database, services, etc.)
pub async fn initialize_deployment() -> Result<DeploymentImpl, StartupError> {
    sentry_utils::init_once(SentrySource::Backend);

    // Create asset directory if it doesn't exist
    if !asset_dir().exists() {
        std::fs::create_dir_all(asset_dir())?;
    }

    let deployment = DeploymentImpl::new().await?;
    deployment.update_sentry_scope().await?;

    deployment
        .container()
        .cleanup_orphan_executions()
        .await
        .map_err(DeploymentError::from)?;
    deployment
        .container()
        .backfill_before_head_commits()
        .await
        .map_err(DeploymentError::from)?;
    deployment
        .container()
        .backfill_repo_names()
        .await
        .map_err(DeploymentError::from)?;

    Ok(deployment)
}

/// Spawn background services (cache warming, task verification)
pub async fn spawn_background_services(deployment: &DeploymentImpl, platform: &str) {
    deployment.spawn_telegram_bot_service().await;
    crate::mcp::http_service::McpHttpService::start(
        deployment.config().clone(),
        deployment.mcp_server_handle().clone(),
    )
    .await;

    deployment
        .track_if_analytics_allowed("session_start", serde_json::json!({ "platform": platform }))
        .await;

    // Pre-warm file search cache for most active projects
    let deployment_for_cache = deployment.clone();
    tokio::spawn(async move {
        if let Err(e) = deployment_for_cache
            .file_search_cache()
            .warm_most_active(&deployment_for_cache.db().pool, 3)
            .await
        {
            tracing::warn!("Failed to warm file search cache: {}", e);
        }
    });
}

/// Start the HTTP server and return the actual port and a handle to the server task
pub async fn start_server(
    deployment: DeploymentImpl,
    config: ServerConfig,
) -> Result<(u16, JoinHandle<()>), StartupError> {
    let app_router = routes::router(deployment.clone());

    let app_router = if config.local_network_auth {
        app_router.layer(middleware::from_fn_with_state(
            deployment.clone(),
            local_network_auth,
        ))
    } else {
        app_router
    };

    let make_service = app_router
        .into_service::<Body>()
        .into_make_service_with_connect_info::<SocketAddr>();

    let listener =
        tokio::net::TcpListener::bind(format!("{}:{}", config.host, config.port)).await?;
    let actual_port = listener.local_addr()?.port();

    // Share port in-process for components like the telegram bot
    set_shared_port(actual_port);

    // Write port file for discovery if requested
    if config.write_port_file {
        if let Err(e) = write_port_file(actual_port).await {
            tracing::warn!("Failed to write port file: {}", e);
        }
    }

    tracing::info!("Server running on http://{}:{}", config.host, actual_port);

    if config.open_browser {
        let port = actual_port;
        tokio::spawn(async move {
            if let Err(e) = open_browser(&format!("http://127.0.0.1:{port}")).await {
                tracing::warn!(
                    "Failed to open browser automatically: {}. Please open http://127.0.0.1:{} manually.",
                    e,
                    port
                );
            }
        });
    }

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, make_service).await {
            tracing::error!("Server error: {}", e);
        }
    });

    Ok((actual_port, handle))
}

async fn local_network_auth(
    State(deployment): State<DeploymentImpl>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: axum::http::Request<Body>,
    next: Next,
) -> Response {
    if addr.ip().is_loopback() {
        return next.run(request).await;
    }

    let config = deployment.config().read().await;
    if !config.local_network_access {
        return forbidden_response();
    }

    let password = match config.local_network_password.as_deref() {
        Some(value) if !value.is_empty() => value,
        _ => return forbidden_response(),
    };

    if let Some(provided) = extract_basic_password(request.headers()) {
        if provided == password {
            return next.run(request).await;
        }
    }

    unauthorized_response()
}

fn extract_basic_password(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let encoded = value.strip_prefix("Basic ")?;
    let decoded = BASE64.decode(encoded).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let mut parts = decoded.splitn(2, ':');
    let _username = parts.next()?;
    let password = parts.next().unwrap_or_default();
    Some(password.to_string())
}

fn unauthorized_response() -> Response {
    let mut response = Response::new(Body::from("Unauthorized"));
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"Vibe Kanban\""),
    );
    response
}

fn forbidden_response() -> Response {
    let mut response = Response::new(Body::from("Forbidden"));
    *response.status_mut() = StatusCode::FORBIDDEN;
    response
}

/// Wait for Ctrl+C or SIGTERM, whichever arrives first.
pub async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!("Failed to install Ctrl+C handler: {err}");
        }
    };

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let terminate = async {
            if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
                sigterm.recv().await;
            } else {
                tracing::error!("Failed to install SIGTERM handler");
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
        ctrl_c.await;
    }
}

/// Perform cleanup actions (kill running processes)
pub async fn cleanup(deployment: &DeploymentImpl) {
    deployment
        .container()
        .kill_all_running_processes()
        .await
        .expect("Failed to cleanly kill running execution processes");
}
