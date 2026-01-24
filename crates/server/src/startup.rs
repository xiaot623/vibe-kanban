use crate::routes;
use crate::DeploymentImpl;
use deployment::{Deployment, DeploymentError};
use services::services::container::ContainerService;
use sqlx::Error as SqlxError;
use thiserror::Error;
use tokio::task::JoinHandle;
use tracing_subscriber::{EnvFilter, prelude::*};
use utils::{
    assets::asset_dir,
    browser::open_browser,
    port_file::write_port_file,
    sentry::{self as sentry_utils, SentrySource, sentry_layer},
};

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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 0,
            host: "127.0.0.1".to_string(),
            open_browser: false,
            write_port_file: true,
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

/// Spawn background services (cache warming, task verification, PR monitor)
pub async fn spawn_background_services(deployment: &DeploymentImpl, platform: &str) {
    deployment.spawn_pr_monitor_service().await;

    deployment
        .track_if_analytics_allowed(
            "session_start",
            serde_json::json!({ "platform": platform }),
        )
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

    // Verify shared tasks in background
    let deployment_for_verification = deployment.clone();
    tokio::spawn(async move {
        if let Some(publisher) = deployment_for_verification.container().share_publisher()
            && let Err(e) = publisher.cleanup_shared_tasks().await
        {
            tracing::warn!("Failed to verify shared tasks: {}", e);
        }
    });
}

/// Start the HTTP server and return the actual port and a handle to the server task
pub async fn start_server(
    deployment: DeploymentImpl,
    config: ServerConfig,
) -> Result<(u16, JoinHandle<()>), StartupError> {
    let app_router = routes::router(deployment.clone());

    let listener =
        tokio::net::TcpListener::bind(format!("{}:{}", config.host, config.port)).await?;
    let actual_port = listener.local_addr()?.port();

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
        if let Err(e) = axum::serve(listener, app_router).await {
            tracing::error!("Server error: {}", e);
        }
    });

    Ok((actual_port, handle))
}

/// Perform cleanup actions (kill running processes)
pub async fn cleanup(deployment: &DeploymentImpl) {
    deployment
        .container()
        .kill_all_running_processes()
        .await
        .expect("Failed to cleanly kill running execution processes");
}
