use anyhow;
use axum::{
    Extension, Router,
    extract::{
        Path, Query, State,
        ws::{WebSocket, WebSocketUpgrade},
    },
    middleware::from_fn_with_state,
    response::{IntoResponse, Json as ResponseJson},
    routing::{get, post},
};
use db::models::{
    execution_process::{ExecutionProcess, ExecutionProcessError, ExecutionProcessStatus},
    execution_process_repo_state::ExecutionProcessRepoState,
};
use deployment::Deployment;
use futures_util::{SinkExt, StreamExt, TryStreamExt};
use serde::Deserialize;
use serde_json::json;
use services::services::container::{ContainerService, StreamedNormalizedLogEvent};
use utils::{log_msg::LogMsg, response::ApiResponse};
use uuid::Uuid;

use crate::{DeploymentImpl, error::ApiError, middleware::load_execution_process_middleware};

#[derive(Debug, Deserialize)]
pub struct SessionExecutionProcessQuery {
    pub session_id: Uuid,
    /// If true, include soft-deleted (dropped) processes in results/stream
    #[serde(default)]
    pub show_soft_deleted: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
pub struct LogStreamCursorQuery {
    pub after_seq: Option<i64>,
    pub before_seq: Option<i64>,
    pub limit: Option<u32>,
}

const DEFAULT_LOG_STREAM_LIMIT: u32 = 400;
const MAX_LOG_STREAM_LIMIT: u32 = 1000;

impl LogStreamCursorQuery {
    fn normalized_limit(&self) -> u32 {
        self.limit
            .unwrap_or(DEFAULT_LOG_STREAM_LIMIT)
            .clamp(1, MAX_LOG_STREAM_LIMIT)
    }
}

fn patch_to_ws_message(patch: serde_json::Value, seq: Option<i64>) -> axum::extract::ws::Message {
    let payload = if let Some(seq) = seq {
        json!({
            "JsonPatch": patch,
            "seq": seq,
        })
    } else {
        json!({
            "JsonPatch": patch,
        })
    };

    axum::extract::ws::Message::Text(payload.to_string().into())
}

fn normalized_event_to_ws_message(
    event: serde_json::Value,
    seq: Option<i64>,
) -> axum::extract::ws::Message {
    let payload = if let Some(seq) = seq {
        json!({
            "event": event,
            "seq": seq,
        })
    } else {
        json!({
            "event": event,
        })
    };

    axum::extract::ws::Message::Text(payload.to_string().into())
}

fn raw_log_to_patch(msg: LogMsg) -> Option<serde_json::Value> {
    match msg {
        LogMsg::Stdout(content) => Some(json!([{
            "op": "add",
            "path": "/entries/-",
            "value": {
                "type": "STDOUT",
                "content": content,
            }
        }])),
        LogMsg::Stderr(content) => Some(json!([{
            "op": "add",
            "path": "/entries/-",
            "value": {
                "type": "STDERR",
                "content": content,
            }
        }])),
        _ => None,
    }
}

pub async fn get_execution_process_by_id(
    Extension(execution_process): Extension<ExecutionProcess>,
    State(_deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<ExecutionProcess>>, ApiError> {
    Ok(ResponseJson(ApiResponse::success(execution_process)))
}

pub async fn stream_raw_logs_ws(
    ws: WebSocketUpgrade,
    State(deployment): State<DeploymentImpl>,
    Path(exec_id): Path<Uuid>,
    Query(query): Query<LogStreamCursorQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let after_seq = query.after_seq;
    let before_seq = query.before_seq;
    let limit = query.normalized_limit();

    Ok(ws.on_upgrade(move |socket| async move {
        if let Err(e) =
            handle_raw_logs_ws(socket, deployment, exec_id, after_seq, before_seq, limit).await
        {
            tracing::warn!("raw logs WS closed: {}", e);
        }
    }))
}

async fn handle_raw_logs_ws(
    socket: WebSocket,
    deployment: DeploymentImpl,
    exec_id: Uuid,
    after_seq: Option<i64>,
    before_seq: Option<i64>,
    limit: u32,
) -> anyhow::Result<()> {
    // Get the raw stream and convert to JSON patches on-the-fly
    let raw_stream = deployment
        .container()
        .stream_raw_logs(&exec_id, after_seq, before_seq, limit)
        .await
        .ok_or_else(|| anyhow::anyhow!("Execution process not found"))?;

    let mut stream = raw_stream.map_ok(|m| match m.msg {
        LogMsg::Finished => LogMsg::Finished.to_ws_message_unchecked(),
        other => raw_log_to_patch(other)
            .map(|patch| patch_to_ws_message(patch, m.seq))
            .unwrap_or_else(|| LogMsg::Finished.to_ws_message_unchecked()),
    });

    // Split socket into sender and receiver
    let (mut sender, mut receiver) = socket.split();

    // Drain (and ignore) any client->server messages so pings/pongs work
    tokio::spawn(async move { while let Some(Ok(_)) = receiver.next().await {} });

    // Forward server messages
    while let Some(item) = stream.next().await {
        match item {
            Ok(msg) => {
                if sender.send(msg).await.is_err() {
                    break; // client disconnected
                }
            }
            Err(e) => {
                tracing::error!("stream error: {}", e);
                break;
            }
        }
    }
    Ok(())
}

pub async fn stream_normalized_logs_ws(
    ws: WebSocketUpgrade,
    State(deployment): State<DeploymentImpl>,
    Path(exec_id): Path<Uuid>,
    Query(query): Query<LogStreamCursorQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let stream = deployment
        .container()
        .stream_normalized_logs(
            &exec_id,
            query.after_seq,
            query.before_seq,
            query.normalized_limit(),
        )
        .await
        .ok_or_else(|| {
            ApiError::ExecutionProcess(ExecutionProcessError::ExecutionProcessNotFound)
        })?;

    // Convert the error type to anyhow::Error and turn TryStream -> Stream<Result<_, _>>
    let stream = stream.err_into::<anyhow::Error>().into_stream();

    Ok(ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle_normalized_logs_ws(socket, stream).await {
            tracing::warn!("normalized logs WS closed: {}", e);
        }
    }))
}

async fn handle_normalized_logs_ws(
    socket: WebSocket,
    stream: impl futures_util::Stream<Item = anyhow::Result<StreamedNormalizedLogEvent>>
    + Unpin
    + Send
    + 'static,
) -> anyhow::Result<()> {
    // Assign a monotonic counter to live events that have no DB sequence number, so the
    // frontend can use `after_seq` for cursor-based pagination even on in-memory streams.
    let mut next_synthetic_seq: i64 = 0;
    let mut stream = stream.map_ok(move |msg| {
        let is_finished = matches!(msg.event, executors::logs::NormalizedLogEvent::Finished);
        let effective_seq = msg.seq.or_else(|| {
            let s = next_synthetic_seq;
            next_synthetic_seq += 1;
            Some(s)
        });
        let message = normalized_event_to_ws_message(
            serde_json::to_value(msg.event).unwrap_or_else(|_| json!({"type":"finished"})),
            effective_seq,
        );
        (message, is_finished)
    });
    let (mut sender, mut receiver) = socket.split();
    tokio::spawn(async move { while let Some(Ok(_)) = receiver.next().await {} });
    while let Some(item) = stream.next().await {
        match item {
            Ok((msg, is_finished)) => {
                if sender.send(msg).await.is_err() {
                    break;
                }
                if is_finished {
                    break;
                }
            }
            Err(e) => {
                tracing::error!("stream error: {}", e);
                break;
            }
        }
    }
    Ok(())
}

pub async fn stop_execution_process(
    Extension(execution_process): Extension<ExecutionProcess>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<()>>, ApiError> {
    deployment
        .container()
        .stop_execution(&execution_process, ExecutionProcessStatus::Killed)
        .await?;

    Ok(ResponseJson(ApiResponse::success(())))
}

pub async fn stream_execution_processes_by_session_ws(
    ws: WebSocketUpgrade,
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<SessionExecutionProcessQuery>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle_execution_processes_by_session_ws(
            socket,
            deployment,
            query.session_id,
            query.show_soft_deleted.unwrap_or(false),
        )
        .await
        {
            tracing::warn!("execution processes by session WS closed: {}", e);
        }
    })
}

async fn handle_execution_processes_by_session_ws(
    socket: WebSocket,
    deployment: DeploymentImpl,
    session_id: uuid::Uuid,
    show_soft_deleted: bool,
) -> anyhow::Result<()> {
    // Get the raw stream and convert LogMsg to WebSocket messages
    let mut stream = deployment
        .events()
        .stream_execution_processes_for_session_raw(session_id, show_soft_deleted)
        .await?
        .map_ok(|msg| msg.to_ws_message_unchecked());

    // Split socket into sender and receiver
    let (mut sender, mut receiver) = socket.split();

    // Drain (and ignore) any client->server messages so pings/pongs work
    tokio::spawn(async move { while let Some(Ok(_)) = receiver.next().await {} });

    // Forward server messages
    while let Some(item) = stream.next().await {
        match item {
            Ok(msg) => {
                if sender.send(msg).await.is_err() {
                    break; // client disconnected
                }
            }
            Err(e) => {
                tracing::error!("stream error: {}", e);
                break;
            }
        }
    }
    Ok(())
}

pub async fn get_execution_process_repo_states(
    Extension(execution_process): Extension<ExecutionProcess>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<Vec<ExecutionProcessRepoState>>>, ApiError> {
    let pool = &deployment.db().pool;
    let repo_states =
        ExecutionProcessRepoState::find_by_execution_process_id(pool, execution_process.id).await?;
    Ok(ResponseJson(ApiResponse::success(repo_states)))
}

pub fn router(deployment: &DeploymentImpl) -> Router<DeploymentImpl> {
    let workspace_id_router = Router::new()
        .route("/", get(get_execution_process_by_id))
        .route("/stop", post(stop_execution_process))
        .route("/repo-states", get(get_execution_process_repo_states))
        .route("/raw-logs/ws", get(stream_raw_logs_ws))
        .route("/normalized-logs/ws", get(stream_normalized_logs_ws))
        .layer(from_fn_with_state(
            deployment.clone(),
            load_execution_process_middleware,
        ));

    let workspaces_router = Router::new()
        .route(
            "/stream/session/ws",
            get(stream_execution_processes_by_session_ws),
        )
        .nest("/{id}", workspace_id_router);

    Router::new().nest("/execution-processes", workspaces_router)
}
