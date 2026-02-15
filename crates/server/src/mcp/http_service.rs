use std::sync::Arc;

use axum::Router;
use local_deployment::McpServerHandle;
use rmcp::transport::{
    StreamableHttpServerConfig,
    streamable_http_server::{session::local::LocalSessionManager, tower::StreamableHttpService},
};
use services::services::config::Config;
use tokio::sync::RwLock;

use crate::mcp::task_server::TaskServer;

pub struct McpHttpService;

impl McpHttpService {
    /// Start the MCP HTTP server if enabled in config.
    /// Stops any previously running instance first.
    pub async fn start(config: Arc<RwLock<Config>>, handle_store: McpServerHandle) {
        // Stop any existing instance first
        Self::stop(handle_store.clone()).await;

        let mcp_config = config.read().await.mcp_server.clone();

        if !mcp_config.enabled {
            tracing::info!("MCP server disabled");
            return;
        }

        let port = mcp_config.port;
        let handle = tokio::spawn(async move {
            let service: StreamableHttpService<TaskServer, LocalSessionManager> =
                StreamableHttpService::new(
                    || Ok(TaskServer::new(&resolve_backend_base_url())),
                    Default::default(),
                    StreamableHttpServerConfig {
                        stateful_mode: true,
                        sse_keep_alive: None,
                    },
                );

            let router = Router::new().nest_service("/mcp", service);
            let bind_addr = format!("127.0.0.1:{port}");
            let listener = match tokio::net::TcpListener::bind(&bind_addr).await {
                Ok(listener) => listener,
                Err(err) => {
                    tracing::error!("Failed to bind MCP server on {}: {}", bind_addr, err);
                    return;
                }
            };

            tracing::info!("MCP server started on http://{bind_addr}/mcp");
            if let Err(err) = axum::serve(listener, router).await {
                tracing::error!("MCP server error: {}", err);
            }
        });

        handle_store.lock().await.replace(handle);
    }

    /// Stop a running MCP HTTP server, if any.
    pub async fn stop(handle_store: McpServerHandle) {
        let handle = handle_store.lock().await.take();
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
            tracing::info!("MCP server stopped");
        }
    }
}

fn resolve_backend_base_url() -> String {
    if let Ok(url) = std::env::var("VIBE_BACKEND_URL") {
        return url;
    }

    if let Some(port) = utils::port_file::get_shared_port()
        .or_else(|| parse_port_env("BACKEND_PORT"))
        .or_else(|| parse_port_env("PORT"))
    {
        return format!("http://127.0.0.1:{port}");
    }

    tracing::warn!(
        "Unable to resolve backend port for MCP server, defaulting to 127.0.0.1:0 until available"
    );
    "http://127.0.0.1:0".to_string()
}

fn parse_port_env(key: &str) -> Option<u16> {
    std::env::var(key).ok()?.parse::<u16>().ok()
}
