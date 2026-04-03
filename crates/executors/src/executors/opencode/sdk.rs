use std::{
    collections::HashSet,
    io,
    path::Path,
    sync::{Arc, Once},
    time::Duration,
};

use eventsource_stream::Eventsource;
use futures::{FutureExt, StreamExt};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    fs,
    io::{AsyncWrite, AsyncWriteExt, BufWriter},
    sync::{Mutex, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;
use workspace_utils::approvals::ApprovalStatus;

use super::{
    EXIT_PLAN_MODE_NAME,
    plan_mode::{PlanExitQuestion, REQUEST_USER_INPUT_TOOL_NAME, detect_plan_exit_question},
    types::{OpencodeExecutorEvent, QuestionAskedEvent},
};
use crate::{
    approvals::{ExecutorApprovalError, ExecutorApprovalService},
    executors::ExecutorError,
};

fn ensure_rustls_crypto_provider() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if let Err(err) = rustls::crypto::aws_lc_rs::default_provider().install_default() {
            tracing::debug!("rustls crypto provider install failed: {err:?}");
        }
    });
}

#[derive(Clone)]
pub struct LogWriter {
    writer: Arc<Mutex<BufWriter<Box<dyn AsyncWrite + Send + Unpin>>>>,
}

impl LogWriter {
    pub fn new(writer: impl AsyncWrite + Send + Unpin + 'static) -> Self {
        Self {
            writer: Arc::new(Mutex::new(BufWriter::new(Box::new(writer)))),
        }
    }

    pub async fn log_event(&self, event: &OpencodeExecutorEvent) -> Result<(), ExecutorError> {
        let raw =
            serde_json::to_string(event).map_err(|err| ExecutorError::Io(io::Error::other(err)))?;
        self.log_raw(&raw).await
    }

    pub async fn log_error(&self, message: String) -> Result<(), ExecutorError> {
        self.log_event(&OpencodeExecutorEvent::Error { message })
            .await
    }

    async fn log_raw(&self, raw: &str) -> Result<(), ExecutorError> {
        let mut guard = self.writer.lock().await;
        guard
            .write_all(raw.as_bytes())
            .await
            .map_err(ExecutorError::Io)?;
        guard.write_all(b"\n").await.map_err(ExecutorError::Io)?;
        guard.flush().await.map_err(ExecutorError::Io)?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct RunConfig {
    pub base_url: String,
    pub directory: String,
    pub prompt: String,
    pub resume_session_id: Option<String>,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub permission_approvals: Option<Arc<dyn ExecutorApprovalService>>,
    pub plan_approvals: Option<Arc<dyn ExecutorApprovalService>>,
}

#[derive(Debug, Deserialize)]
struct HealthResponse {
    healthy: bool,
    version: String,
}

#[derive(Debug, Deserialize)]
struct SessionResponse {
    id: String,
}

#[derive(Debug, Serialize)]
struct PromptRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<ModelSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    parts: Vec<TextPartInput>,
}

#[derive(Debug, Serialize, Clone)]
struct ModelSpec {
    #[serde(rename = "providerID")]
    provider_id: String,
    #[serde(rename = "modelID")]
    model_id: String,
}

#[derive(Debug, Serialize)]
struct TextPartInput {
    r#type: &'static str,
    text: String,
}

#[derive(Debug, Clone)]
enum ControlEvent {
    Idle,
    AuthRequired { message: String },
    SessionError { message: String },
    Disconnected,
}

pub async fn run_session(
    config: RunConfig,
    log_writer: LogWriter,
    interrupt_rx: oneshot::Receiver<()>,
) -> Result<(), ExecutorError> {
    ensure_rustls_crypto_provider();
    let cancel = CancellationToken::new();

    let client = reqwest::Client::builder()
        .default_headers(build_default_headers(&config.directory))
        .build()
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    let mut interrupted = false;
    let interrupt_rx = interrupt_rx.fuse();
    let session_fut = run_session_inner(config, log_writer, client, cancel.clone()).fuse();

    tokio::pin!(interrupt_rx);
    tokio::pin!(session_fut);

    loop {
        tokio::select! {
            biased;
            _ = &mut interrupt_rx => {
                interrupted = true;
                cancel.cancel();
            }
            res = &mut session_fut => {
                if interrupted {
                    return Ok(());
                }
                return res;
            }
        }
    }
}

async fn run_session_inner(
    config: RunConfig,
    log_writer: LogWriter,
    client: reqwest::Client,
    cancel: CancellationToken,
) -> Result<(), ExecutorError> {
    tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        res = wait_for_health(&client, &config.base_url) => res?,
    }

    let session_id = match config.resume_session_id.as_deref() {
        Some(existing) => {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                res = fork_session(&client, &config.base_url, &config.directory, existing) => res?,
            }
        }
        None => tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            res = create_session(&client, &config.base_url, &config.directory) => res?,
        },
    };

    log_writer
        .log_event(&OpencodeExecutorEvent::SessionStart {
            session_id: session_id.clone(),
        })
        .await?;

    let model = config.model.as_deref().and_then(parse_model);

    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<ControlEvent>();

    let event_resp = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        res = connect_event_stream(&client, &config.base_url, &config.directory, None) => res?,
    };
    let event_handle = tokio::spawn(spawn_event_listener(
        EventListenerConfig {
            client: client.clone(),
            base_url: config.base_url.clone(),
            directory: config.directory.clone(),
            session_id: session_id.clone(),
            log_writer: log_writer.clone(),
            permission_approvals: config.permission_approvals.clone(),
            plan_approvals: config.plan_approvals.clone(),
            control_tx,
        },
        event_resp,
    ));

    let prompt_result = run_prompt_with_control(
        SessionRequestContext {
            client: &client,
            base_url: &config.base_url,
            directory: &config.directory,
            session_id: &session_id,
        },
        &config.prompt,
        model.clone(),
        config.agent.clone(),
        &mut control_rx,
        cancel.clone(),
    )
    .await;

    if cancel.is_cancelled() {
        send_abort(&client, &config.base_url, &config.directory, &session_id).await;
        event_handle.abort();
        return Ok(());
    }

    event_handle.abort();

    prompt_result?;
    log_writer.log_event(&OpencodeExecutorEvent::Done).await?;

    Ok(())
}

fn build_default_headers(directory: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(directory) {
        headers.insert("x-opencode-directory", value);
    }
    headers
}

struct SessionRequestContext<'a> {
    client: &'a reqwest::Client,
    base_url: &'a str,
    directory: &'a str,
    session_id: &'a str,
}

fn append_session_error(session_error: &mut Option<String>, message: String) {
    match session_error {
        Some(existing) => {
            existing.push('\n');
            existing.push_str(&message);
        }
        None => *session_error = Some(message),
    }
}

async fn run_prompt_with_control(
    ctx: SessionRequestContext<'_>,
    prompt_text: &str,
    model: Option<ModelSpec>,
    agent: Option<String>,
    control_rx: &mut mpsc::UnboundedReceiver<ControlEvent>,
    cancel: CancellationToken,
) -> Result<(), ExecutorError> {
    let mut idle_seen = false;
    let mut session_error: Option<String> = None;

    let mut prompt_fut = Box::pin(prompt(
        ctx.client,
        ctx.base_url,
        ctx.directory,
        ctx.session_id,
        prompt_text,
        model,
        agent,
    ));

    let prompt_result = loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            res = &mut prompt_fut => break res,
            event = control_rx.recv() => match event {
                Some(ControlEvent::AuthRequired { message }) => return Err(ExecutorError::AuthRequired(message)),
                Some(ControlEvent::SessionError { message }) => append_session_error(&mut session_error, message),
                Some(ControlEvent::Disconnected) if !cancel.is_cancelled() => {
                    return Err(ExecutorError::Io(io::Error::other("OpenCode event stream disconnected while prompt was running")));
                }
                Some(ControlEvent::Disconnected) => return Ok(()),
                Some(ControlEvent::Idle) => idle_seen = true,
                None => {}
            }
        }
    };

    if let Err(err) = prompt_result {
        if cancel.is_cancelled() {
            return Ok(());
        }
        return Err(err);
    }

    if !idle_seen {
        // The OpenCode server streams events independently; wait for `session.idle` so we capture
        // tail updates reliably (e.g. final tool completion events).
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                event = control_rx.recv() => match event {
                    Some(ControlEvent::Idle) | None => break,
                    Some(ControlEvent::AuthRequired { message }) => return Err(ExecutorError::AuthRequired(message)),
                    Some(ControlEvent::SessionError { message }) => append_session_error(&mut session_error, message),
                    Some(ControlEvent::Disconnected) if !cancel.is_cancelled() => {
                        return Err(ExecutorError::Io(io::Error::other(
                            "OpenCode event stream disconnected while waiting for session to go idle",
                        )));
                    }
                    Some(ControlEvent::Disconnected) => return Ok(()),
                }
            }
        }
    }

    if let Some(message) = session_error {
        if cancel.is_cancelled() {
            return Ok(());
        }
        return Err(ExecutorError::Io(io::Error::other(message)));
    }

    Ok(())
}

async fn wait_for_health(client: &reqwest::Client, base_url: &str) -> Result<(), ExecutorError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut last_err: Option<String> = None;

    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(ExecutorError::Io(io::Error::other(format!(
                "Timed out waiting for OpenCode server health: {}",
                last_err.unwrap_or_else(|| "unknown error".to_string())
            ))));
        }

        let resp = client.get(format!("{base_url}/global/health")).send().await;
        match resp {
            Ok(resp) => {
                if !resp.status().is_success() {
                    last_err = Some(format!("HTTP {}", resp.status()));
                } else if let Ok(body) = resp.json::<HealthResponse>().await {
                    if body.healthy {
                        return Ok(());
                    }
                    last_err = Some(format!("unhealthy server (version {})", body.version));
                } else {
                    last_err = Some("failed to parse health response".to_string());
                }
            }
            Err(err) => {
                last_err = Some(err.to_string());
            }
        }

        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn create_session(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
) -> Result<String, ExecutorError> {
    let resp = client
        .post(format!("{base_url}/session"))
        .query(&[("directory", directory)])
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    if !resp.status().is_success() {
        return Err(ExecutorError::Io(io::Error::other(format!(
            "OpenCode session.create failed: HTTP {}",
            resp.status()
        ))));
    }

    let session = resp
        .json::<SessionResponse>()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;
    Ok(session.id)
}

async fn fork_session(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
    session_id: &str,
) -> Result<String, ExecutorError> {
    let resp = client
        .post(format!("{base_url}/session/{session_id}/fork"))
        .query(&[("directory", directory)])
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    if !resp.status().is_success() {
        return Err(ExecutorError::Io(io::Error::other(format!(
            "OpenCode session.fork failed: HTTP {}",
            resp.status()
        ))));
    }

    let session = resp
        .json::<SessionResponse>()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;
    Ok(session.id)
}

async fn prompt(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
    session_id: &str,
    prompt: &str,
    model: Option<ModelSpec>,
    agent: Option<String>,
) -> Result<(), ExecutorError> {
    let req = PromptRequest {
        model,
        agent,
        parts: vec![TextPartInput {
            r#type: "text",
            text: prompt.to_string(),
        }],
    };

    let resp = client
        .post(format!("{base_url}/session/{session_id}/message"))
        .query(&[("directory", directory)])
        .json(&req)
        .send()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    // The OpenCode server uses streaming responses and may set the HTTP status early; validate
    // success using the response body shape as well.
    if !status.is_success() {
        return Err(ExecutorError::Io(io::Error::other(format!(
            "OpenCode session.prompt failed: HTTP {status} {body}"
        ))));
    }

    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(ExecutorError::Io(io::Error::other(
            "OpenCode session.prompt returned empty response body",
        )));
    }

    let parsed: Value =
        serde_json::from_str(trimmed).map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    // Success response: { info, parts }
    if parsed.get("info").is_some() && parsed.get("parts").is_some() {
        return Ok(());
    }

    // Error response: { name, data }
    if let Some(name) = parsed.get("name").and_then(Value::as_str) {
        let message = parsed
            .pointer("/data/message")
            .and_then(Value::as_str)
            .unwrap_or(trimmed);
        return Err(ExecutorError::Io(io::Error::other(format!(
            "OpenCode session.prompt failed: {name}: {message}"
        ))));
    }

    Err(ExecutorError::Io(io::Error::other(format!(
        "OpenCode session.prompt returned unexpected response: {trimmed}"
    ))))
}

async fn send_abort(client: &reqwest::Client, base_url: &str, directory: &str, session_id: &str) {
    let request = client
        .post(format!("{base_url}/session/{session_id}/abort"))
        .query(&[("directory", directory)]);

    let _ = tokio::time::timeout(Duration::from_millis(800), async move {
        let resp = request.send().await;
        if let Ok(resp) = resp {
            // Drain body
            let _ = resp.bytes().await;
        }
    })
    .await;
}

fn parse_model(model: &str) -> Option<ModelSpec> {
    let (provider_id, model_id) = match model.split_once('/') {
        Some((provider, rest)) => (provider.to_string(), rest.to_string()),
        None => (model.to_string(), String::new()),
    };

    Some(ModelSpec {
        provider_id,
        model_id,
    })
}

async fn connect_event_stream(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
    last_event_id: Option<&str>,
) -> Result<reqwest::Response, ExecutorError> {
    let mut req = client
        .get(format!("{base_url}/event"))
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .query(&[("directory", directory)]);

    if let Some(last_event_id) = last_event_id {
        req = req.header("Last-Event-ID", last_event_id);
    }

    let resp = req
        .send()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read response body>".to_string());
        return Err(ExecutorError::Io(io::Error::other(format!(
            "OpenCode event stream failed: HTTP {status} {body}"
        ))));
    }

    Ok(resp)
}

struct EventListenerConfig {
    client: reqwest::Client,
    base_url: String,
    directory: String,
    session_id: String,
    log_writer: LogWriter,
    permission_approvals: Option<Arc<dyn ExecutorApprovalService>>,
    plan_approvals: Option<Arc<dyn ExecutorApprovalService>>,
    control_tx: mpsc::UnboundedSender<ControlEvent>,
}

async fn spawn_event_listener(config: EventListenerConfig, initial_resp: reqwest::Response) {
    let EventListenerConfig {
        client,
        base_url,
        directory,
        session_id,
        log_writer,
        permission_approvals,
        plan_approvals,
        control_tx,
    } = config;

    let mut seen_permissions: HashSet<String> = HashSet::new();
    let mut seen_questions: HashSet<String> = HashSet::new();
    let mut last_event_id: Option<String> = None;
    let mut base_retry_delay = Duration::from_millis(3000);
    let mut attempt: u32 = 0;
    let max_attempts: u32 = 20;
    let mut resp: Option<reqwest::Response> = Some(initial_resp);

    loop {
        let current_resp = match resp.take() {
            Some(r) => {
                attempt = 0;
                r
            }
            None => {
                match connect_event_stream(&client, &base_url, &directory, last_event_id.as_deref())
                    .await
                {
                    Ok(r) => {
                        attempt = 0;
                        r
                    }
                    Err(err) => {
                        let _ = log_writer
                            .log_error(format!("OpenCode event stream reconnect failed: {err}"))
                            .await;
                        attempt += 1;
                        if attempt >= max_attempts {
                            let _ = control_tx.send(ControlEvent::Disconnected);
                            return;
                        }

                        tokio::time::sleep(exponential_backoff(base_retry_delay, attempt)).await;
                        continue;
                    }
                }
            }
        };

        let outcome = process_event_stream(
            EventStreamContext {
                seen_permissions: &mut seen_permissions,
                client: &client,
                base_url: &base_url,
                directory: &directory,
                session_id: &session_id,
                log_writer: &log_writer,
                permission_approvals: permission_approvals.clone(),
                plan_approvals: plan_approvals.clone(),
                control_tx: &control_tx,
                base_retry_delay: &mut base_retry_delay,
                last_event_id: &mut last_event_id,
                seen_questions: &mut seen_questions,
            },
            current_resp,
        )
        .await;

        match outcome {
            Ok(EventStreamOutcome::Idle) | Ok(EventStreamOutcome::Terminal) => return,
            Ok(EventStreamOutcome::Disconnected) | Err(_) => {
                attempt += 1;
                if attempt >= max_attempts {
                    let _ = control_tx.send(ControlEvent::Disconnected);
                    return;
                }
            }
        }

        tokio::time::sleep(exponential_backoff(base_retry_delay, attempt)).await;
        resp = None;
    }
}

fn exponential_backoff(base: Duration, attempt: u32) -> Duration {
    let exp = attempt.saturating_sub(1).min(10);
    let mult = 1u32 << exp;
    base.checked_mul(mult)
        .unwrap_or(Duration::from_secs(30))
        .min(Duration::from_secs(30))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventStreamOutcome {
    Idle,
    Terminal,
    Disconnected,
}

struct EventStreamContext<'a> {
    seen_permissions: &'a mut HashSet<String>,
    seen_questions: &'a mut HashSet<String>,
    client: &'a reqwest::Client,
    base_url: &'a str,
    directory: &'a str,
    session_id: &'a str,
    log_writer: &'a LogWriter,
    permission_approvals: Option<Arc<dyn ExecutorApprovalService>>,
    plan_approvals: Option<Arc<dyn ExecutorApprovalService>>,
    control_tx: &'a mpsc::UnboundedSender<ControlEvent>,
    base_retry_delay: &'a mut Duration,
    last_event_id: &'a mut Option<String>,
}

async fn process_event_stream(
    ctx: EventStreamContext<'_>,
    resp: reqwest::Response,
) -> Result<EventStreamOutcome, ExecutorError> {
    let mut stream = resp.bytes_stream().eventsource();

    while let Some(evt) = stream.next().await {
        let evt = evt.map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

        if !evt.id.trim().is_empty() {
            *ctx.last_event_id = Some(evt.id.trim().to_string());
        }
        if let Some(retry) = evt.retry {
            *ctx.base_retry_delay = retry;
        }

        let trimmed = evt.data.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(data) = serde_json::from_str::<Value>(trimmed) else {
            let _ = ctx
                .log_writer
                .log_error(format!(
                    "OpenCode event stream delivered non-JSON event payload: {trimmed}"
                ))
                .await;
            continue;
        };

        let Some(event_type) = data.get("type").and_then(Value::as_str) else {
            continue;
        };

        if !event_matches_session(event_type, &data, ctx.session_id) {
            continue;
        }

        let _ = ctx
            .log_writer
            .log_event(&OpencodeExecutorEvent::SdkEvent {
                event: data.clone(),
            })
            .await;

        match event_type {
            "session.idle" => {
                let _ = ctx.control_tx.send(ControlEvent::Idle);
                return Ok(EventStreamOutcome::Idle);
            }
            "session.error" => {
                let error_type = data
                    .pointer("/properties/error/name")
                    .or_else(|| data.pointer("/properties/error/type"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let message = data
                    .pointer("/properties/error/data/message")
                    .or_else(|| data.pointer("/properties/error/message"))
                    .and_then(Value::as_str)
                    .unwrap_or("OpenCode session error")
                    .to_string();

                if error_type == "ProviderAuthError" {
                    let _ = ctx.control_tx.send(ControlEvent::AuthRequired { message });
                    return Ok(EventStreamOutcome::Terminal);
                }

                let _ = ctx.control_tx.send(ControlEvent::SessionError { message });
            }
            "permission.asked" => {
                let request_id = data
                    .pointer("/properties/id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();

                if request_id.is_empty() || !ctx.seen_permissions.insert(request_id.clone()) {
                    continue;
                }

                let tool_call_id = data
                    .pointer("/properties/tool/callID")
                    .and_then(Value::as_str)
                    .unwrap_or(&request_id)
                    .to_string();

                let permission = data
                    .pointer("/properties/permission")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();

                let tool_input = data
                    .get("properties")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));

                let approvals = ctx.permission_approvals.clone();
                let client = ctx.client.clone();
                let base_url = ctx.base_url.to_string();
                let directory = ctx.directory.to_string();
                let log_writer = ctx.log_writer.clone();
                tokio::spawn(async move {
                    let status = request_tool_approval(
                        approvals,
                        &permission,
                        tool_input,
                        &tool_call_id,
                        false,
                    )
                    .await;

                    let _ = log_writer
                        .log_event(&OpencodeExecutorEvent::ApprovalResponse {
                            tool_call_id: tool_call_id.clone(),
                            status: status.clone(),
                        })
                        .await;

                    let (reply, message) = match status {
                        ApprovalStatus::Approved | ApprovalStatus::ProvidedInput { .. } => {
                            ("once", None)
                        }
                        ApprovalStatus::Denied { reason } => {
                            let msg = reason
                                .unwrap_or_else(|| "User denied this tool use request".to_string())
                                .trim()
                                .to_string();
                            let msg = if msg.is_empty() {
                                "User denied this tool use request".to_string()
                            } else {
                                msg
                            };
                            ("reject", Some(msg))
                        }
                        ApprovalStatus::TimedOut => (
                            "reject",
                            Some(
                                "Approval request timed out; proceed without using this tool call."
                                    .to_string(),
                            ),
                        ),
                        ApprovalStatus::Pending => (
                            "reject",
                            Some(
                                "Approval request could not be completed; proceed without using this tool call."
                                    .to_string(),
                            ),
                        ),
                    };

                    // If we reject without a message, OpenCode treats it as a hard stop.
                    // Provide a message so the agent can continue with guidance.
                    let payload = if reply == "reject" {
                        serde_json::json!({ "reply": reply, "message": message.unwrap_or_else(|| "User denied this tool use request".to_string()) })
                    } else {
                        serde_json::json!({ "reply": reply })
                    };

                    let _ = client
                        .post(format!("{base_url}/permission/{request_id}/reply"))
                        .query(&[("directory", directory.as_str())])
                        .json(&payload)
                        .send()
                        .await;
                });
            }
            "question.asked" => {
                let Some(question) = parse_question_asked_event(&data) else {
                    let _ = ctx
                        .log_writer
                        .log_error(format!(
                            "OpenCode question.asked event had invalid payload: {data}"
                        ))
                        .await;
                    continue;
                };

                let request_id = question.id.trim().to_string();
                if request_id.is_empty() || !ctx.seen_questions.insert(request_id.clone()) {
                    continue;
                }

                let tool_call_id = question_tool_call_id(&question);
                let plan_exit = detect_plan_exit_question(&question.questions).map(|plan_exit| {
                    PlanExitQuestion {
                        plan_relative_path: plan_exit.plan_relative_path,
                    }
                });
                let approvals = if plan_exit.is_some() {
                    ctx.plan_approvals.clone()
                } else {
                    ctx.permission_approvals.clone()
                };

                let client = ctx.client.clone();
                let base_url = ctx.base_url.to_string();
                let directory = ctx.directory.to_string();
                let log_writer = ctx.log_writer.clone();

                tokio::spawn(async move {
                    let is_plan_exit = plan_exit.is_some();
                    let (tool_name, tool_input, strict_approval) =
                        if let Some(plan_exit) = plan_exit {
                            let mut plan_content = if let Some(plan_relative_path) =
                                plan_exit.plan_relative_path.as_deref()
                            {
                                match load_plan_content(&directory, plan_relative_path).await {
                                    Ok(plan) => plan,
                                    Err(err) => {
                                        let _ = log_writer
                                            .log_error(format!(
                                                "Failed to read OpenCode plan `{}`: {err}",
                                                plan_relative_path
                                            ))
                                            .await;
                                        String::new()
                                    }
                                }
                            } else {
                                String::new()
                            };

                            if plan_content.trim().is_empty()
                                && let Some(fallback) = question
                                    .questions
                                    .first()
                                    .map(|q| q.question.trim())
                                    .filter(|q| !q.is_empty())
                            {
                                plan_content = fallback.to_string();
                            }

                            (EXIT_PLAN_MODE_NAME, json!({ "plan": plan_content }), true)
                        } else {
                            (
                                REQUEST_USER_INPUT_TOOL_NAME,
                                build_question_approval_input(&question),
                                false,
                            )
                        };

                    let status = request_tool_approval(
                        approvals,
                        tool_name,
                        tool_input,
                        &tool_call_id,
                        strict_approval,
                    )
                    .await;

                    let _ = log_writer
                        .log_event(&OpencodeExecutorEvent::ApprovalResponse {
                            tool_call_id: tool_call_id.clone(),
                            status: status.clone(),
                        })
                        .await;

                    let answers = if is_plan_exit {
                        vec![vec![plan_exit_answer_from_status(&status).to_string()]]
                    } else {
                        build_question_reply_answers(&question, &status)
                    };

                    if let Err(err) =
                        send_question_reply(&client, &base_url, &directory, &request_id, answers)
                            .await
                    {
                        let _ = log_writer
                            .log_error(format!(
                                "Failed to reply to OpenCode question `{request_id}`: {err}"
                            ))
                            .await;
                    }
                });
            }
            _ => {}
        }
    }

    Ok(EventStreamOutcome::Disconnected)
}

fn event_matches_session(event_type: &str, event: &Value, session_id: &str) -> bool {
    let extracted = match event_type {
        "message.updated" => event
            .pointer("/properties/info/sessionID")
            .and_then(Value::as_str),
        "message.part.updated" => event
            .pointer("/properties/part/sessionID")
            .and_then(Value::as_str),
        "permission.asked" | "permission.replied" | "question.asked" | "question.replied"
        | "question.rejected" | "session.idle" | "session.error" => event
            .pointer("/properties/sessionID")
            .and_then(Value::as_str),
        _ => event
            .pointer("/properties/sessionID")
            .and_then(Value::as_str)
            .or_else(|| {
                event
                    .pointer("/properties/info/sessionID")
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                event
                    .pointer("/properties/part/sessionID")
                    .and_then(Value::as_str)
            }),
    };

    extracted == Some(session_id)
}

fn parse_question_asked_event(event: &Value) -> Option<QuestionAskedEvent> {
    event
        .get("properties")
        .and_then(|properties| serde_json::from_value(properties.clone()).ok())
}

fn question_tool_call_id(question: &QuestionAskedEvent) -> String {
    question
        .tool
        .as_ref()
        .map(|tool| tool.call_id.trim().to_string())
        .filter(|call_id| !call_id.is_empty())
        .unwrap_or_else(|| question.id.clone())
}

async fn load_plan_content(directory: &str, relative_path: &str) -> Result<String, io::Error> {
    let full_path = Path::new(directory).join(relative_path);
    fs::read_to_string(full_path).await
}

fn build_question_approval_input(question: &QuestionAskedEvent) -> Value {
    let questions = question
        .questions
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let options = item
                .options
                .iter()
                .map(|option| {
                    let mut payload = serde_json::Map::new();
                    payload.insert("label".to_string(), Value::String(option.label.clone()));
                    payload.insert("value".to_string(), Value::String(option.label.clone()));
                    if let Some(description) = option
                        .description
                        .as_ref()
                        .map(|desc| desc.trim())
                        .filter(|desc| !desc.is_empty())
                    {
                        payload.insert(
                            "description".to_string(),
                            Value::String(description.to_string()),
                        );
                    }
                    Value::Object(payload)
                })
                .collect::<Vec<_>>();

            let mut payload = serde_json::Map::new();
            payload.insert("id".to_string(), Value::String(question_approval_id(index)));
            payload.insert(
                "header".to_string(),
                Value::String(
                    item.header
                        .as_deref()
                        .map(str::trim)
                        .filter(|header| !header.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("Question {}", index + 1)),
                ),
            );
            payload.insert(
                "question".to_string(),
                Value::String(item.question.trim().to_string()),
            );
            payload.insert("options".to_string(), Value::Array(options));

            if item.multiple.unwrap_or(false) {
                payload.insert("multiSelectMin".to_string(), json!(1));
                payload.insert(
                    "multiSelectMax".to_string(),
                    json!(item.options.len().max(1)),
                );
            }

            if let Some(custom) = item.custom {
                payload.insert("custom".to_string(), Value::Bool(custom));
            }

            Value::Object(payload)
        })
        .collect::<Vec<_>>();

    json!({
        "id": question.id,
        "session_id": question.session_id,
        "questions": questions,
    })
}

fn question_approval_id(index: usize) -> String {
    format!("question_{}", index + 1)
}

fn build_question_reply_answers(
    question: &QuestionAskedEvent,
    status: &ApprovalStatus,
) -> Vec<Vec<String>> {
    let count = question.questions.len();
    if count == 0 {
        return Vec::new();
    }

    let mut answers = vec![Vec::new(); count];
    if let ApprovalStatus::ProvidedInput { input } = status {
        answers = parse_question_reply_answers_from_input(input, count);
    }
    answers
}

fn parse_question_reply_answers_from_input(
    input: &Value,
    question_count: usize,
) -> Vec<Vec<String>> {
    let mut answers = vec![Vec::new(); question_count];
    let source = input
        .get("answers")
        .or_else(|| input.get("updated_input").and_then(|v| v.get("answers")))
        .unwrap_or(input);

    match source {
        Value::Array(values) => {
            for (index, value) in values.iter().take(question_count).enumerate() {
                answers[index] = parse_answer_values(value);
            }
        }
        Value::Object(values) => {
            for index in 0..question_count {
                let key = question_approval_id(index);
                let alt_zero_based = index.to_string();
                let alt_one_based = (index + 1).to_string();
                if let Some(value) = values
                    .get(&key)
                    .or_else(|| values.get(&alt_zero_based))
                    .or_else(|| values.get(&alt_one_based))
                {
                    answers[index] = parse_answer_values(value);
                }
            }
        }
        _ => {}
    }

    answers
}

fn parse_answer_values(value: &Value) -> Vec<String> {
    match value {
        Value::Array(values) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|answer| !answer.is_empty())
            .map(str::to_string)
            .collect(),
        Value::Object(record) => record
            .get("answers")
            .map(parse_answer_values)
            .unwrap_or_default(),
        Value::String(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![trimmed.to_string()]
            }
        }
        _ => Vec::new(),
    }
}

fn plan_exit_answer_from_status(status: &ApprovalStatus) -> &'static str {
    match status {
        ApprovalStatus::Approved | ApprovalStatus::ProvidedInput { .. } => "Yes",
        ApprovalStatus::Denied { .. } | ApprovalStatus::TimedOut | ApprovalStatus::Pending => "No",
    }
}

async fn send_question_reply(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
    request_id: &str,
    answers: Vec<Vec<String>>,
) -> Result<(), ExecutorError> {
    client
        .post(format!("{base_url}/question/{request_id}/reply"))
        .query(&[("directory", directory)])
        .json(&json!({ "answers": answers }))
        .send()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?
        .error_for_status()
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;
    Ok(())
}

async fn request_tool_approval(
    approvals: Option<Arc<dyn ExecutorApprovalService>>,
    tool_name: &str,
    tool_input: Value,
    tool_call_id: &str,
    strict: bool,
) -> ApprovalStatus {
    let Some(approvals) = approvals else {
        return ApprovalStatus::Approved;
    };

    match approvals
        .request_tool_approval(tool_name, tool_input, tool_call_id)
        .await
    {
        Ok(status) => status,
        Err(
            ExecutorApprovalError::ServiceUnavailable | ExecutorApprovalError::SessionNotRegistered,
        ) if !strict => ApprovalStatus::Approved,
        Err(
            err @ (ExecutorApprovalError::ServiceUnavailable
            | ExecutorApprovalError::SessionNotRegistered),
        ) => {
            tracing::warn!(
                tool_name,
                tool_call_id,
                error = %err,
                "OpenCode strict approval failed; denying plan-exit request"
            );
            ApprovalStatus::Denied {
                reason: Some(format!("Approval request failed: {err}")),
            }
        }
        Err(err) => ApprovalStatus::Denied {
            reason: Some(format!("Approval request failed: {err}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::json;
    use workspace_utils::approvals::ApprovalStatus;

    use super::*;
    use crate::approvals::{ExecutorApprovalError, ExecutorApprovalService};

    enum StubApprovalResult {
        ServiceUnavailable,
        SessionNotRegistered,
    }

    struct StubApprovalService {
        result: StubApprovalResult,
    }

    #[async_trait]
    impl ExecutorApprovalService for StubApprovalService {
        async fn request_tool_approval(
            &self,
            _tool_name: &str,
            _tool_input: Value,
            _tool_call_id: &str,
        ) -> Result<ApprovalStatus, ExecutorApprovalError> {
            match &self.result {
                StubApprovalResult::ServiceUnavailable => {
                    Err(ExecutorApprovalError::ServiceUnavailable)
                }
                StubApprovalResult::SessionNotRegistered => {
                    Err(ExecutorApprovalError::SessionNotRegistered)
                }
            }
        }
    }

    fn sample_question_event(question_count: usize) -> QuestionAskedEvent {
        let questions = (0..question_count)
            .map(|index| {
                json!({
                    "question": format!("Question {}", index + 1),
                    "header": format!("Header {}", index + 1),
                    "options": [
                        { "label": "Yes", "description": "approve" },
                        { "label": "No", "description": "reject" }
                    ],
                    "custom": false
                })
            })
            .collect::<Vec<_>>();

        serde_json::from_value(json!({
            "id": "question-1",
            "sessionID": "session-1",
            "questions": questions,
            "tool": {
                "callID": "call-1"
            }
        }))
        .expect("question payload should deserialize")
    }

    fn sample_plan_question_event(with_tool_call: bool) -> QuestionAskedEvent {
        let mut payload = json!({
            "id": "question-plan-1",
            "sessionID": "session-1",
            "questions": [{
                "question": "Plan generated at /tmp/worktree/.opencode/plans/plan-1.md; switch to build mode?",
                "header": "Plan Review",
                "options": [
                    { "label": "Yes", "description": "Switch to build agent" },
                    { "label": "No", "description": "Keep refining plan" }
                ],
                "custom": false
            }]
        });

        if with_tool_call {
            payload["tool"] = json!({ "callID": "call-plan-1" });
        }

        serde_json::from_value(payload).expect("plan question payload should deserialize")
    }

    fn detect_plan_exit_for_test(question: &QuestionAskedEvent) -> Option<PlanExitQuestion> {
        detect_plan_exit_question(&question.questions)
    }

    #[test]
    fn build_question_reply_answers_parses_nested_answer_shapes() {
        let question = sample_question_event(3);
        let status = ApprovalStatus::ProvidedInput {
            input: json!({
                "updated_input": {
                    "answers": {
                        "question_1": [" yes ", ""],
                        "1": " no ",
                        "question_3": { "answers": ["Option A", " Option B "] }
                    }
                }
            }),
        };

        let answers = build_question_reply_answers(&question, &status);
        assert_eq!(
            answers,
            vec![
                vec!["yes".to_string()],
                vec!["no".to_string()],
                vec!["Option A".to_string(), "Option B".to_string()]
            ]
        );
    }

    #[test]
    fn detect_plan_exit_question_parses_absolute_plan_path() {
        let question = sample_plan_question_event(true);
        let detected = detect_plan_exit_for_test(&question).expect("plan exit should be detected");
        assert_eq!(question_tool_call_id(&question), "call-plan-1");
        assert_eq!(
            detected.plan_relative_path.as_deref(),
            Some(".opencode/plans/plan-1.md")
        );
    }

    #[test]
    fn detect_plan_exit_question_falls_back_to_question_id_when_tool_missing() {
        let question = sample_plan_question_event(false);
        let detected = detect_plan_exit_for_test(&question).expect("plan exit should be detected");
        assert_eq!(question_tool_call_id(&question), "question-plan-1");
        assert_eq!(
            detected.plan_relative_path.as_deref(),
            Some(".opencode/plans/plan-1.md")
        );
    }

    #[test]
    fn detect_plan_exit_question_requires_plan_keyword_in_title() {
        let mut question = sample_plan_question_event(true);
        question.questions[0].header = Some("Build Agent".to_string());
        assert!(detect_plan_exit_for_test(&question).is_none());
    }

    #[tokio::test]
    async fn request_tool_approval_non_strict_service_errors_auto_approve() {
        let service = Arc::new(StubApprovalService {
            result: StubApprovalResult::ServiceUnavailable,
        });

        let status = request_tool_approval(
            Some(service),
            REQUEST_USER_INPUT_TOOL_NAME,
            json!({}),
            "call-1",
            false,
        )
        .await;
        assert!(matches!(status, ApprovalStatus::Approved));
    }

    #[tokio::test]
    async fn request_tool_approval_strict_service_errors_deny() {
        let service = Arc::new(StubApprovalService {
            result: StubApprovalResult::SessionNotRegistered,
        });

        let status = request_tool_approval(
            Some(service),
            EXIT_PLAN_MODE_NAME,
            json!({ "plan": "# Plan" }),
            "call-plan-1",
            true,
        )
        .await;

        match status {
            ApprovalStatus::Denied { reason } => {
                let reason = reason.expect("strict denial should include reason");
                assert!(reason.contains("session not registered"));
            }
            other => panic!("expected denied status, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_tool_approval_none_defaults_to_approved() {
        let status = request_tool_approval(
            None,
            REQUEST_USER_INPUT_TOOL_NAME,
            json!({}),
            "call-none",
            true,
        )
        .await;
        assert!(matches!(status, ApprovalStatus::Approved));
    }
}
