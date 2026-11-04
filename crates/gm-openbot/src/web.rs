use crate::bridges::{BridgeInboundMessage, BridgeRouter, verify_slack_signature};
use crate::supervisor::OpenBotSupervisor;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::{get, post};
use gm_core::ipc::QueueControlAction;
use gm_safety::AuditCategory;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

const COMPANION_UI_HTML: &str = include_str!("companion_ui.html");

#[derive(Clone)]
pub struct CompanionWebState {
    pub supervisor: OpenBotSupervisor,
    pub bridge_router: BridgeRouter,
    pub bearer_token: Option<String>,
    pub slack_signing_secret: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct EnqueueRequest {
    pub bot_id: Option<String>,
    pub channel_id: Option<String>,
    pub instruction: String,
    #[serde(default)]
    pub priority: i32,
}

#[derive(Debug, Deserialize)]
pub struct WheelResumeBody {
    pub note: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReassignBody {
    pub target_bot_id: String,
}

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    pub category: Option<String>,
    pub bot_id: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ChannelPostBody {
    pub author: Option<String>,
    pub content: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AgUiRunRequest {
    pub bot_id: Option<String>,
    pub instruction: String,
}

fn check_auth(
    headers: &HeaderMap,
    expected_token: Option<&str>,
) -> std::result::Result<(), StatusCode> {
    let Some(expected) = expected_token else {
        return Ok(());
    };
    if expected.trim().is_empty() {
        return Ok(());
    }
    let auth_val = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if auth_val == format!("Bearer {expected}") {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

pub fn build_companion_router(state: CompanionWebState) -> Router {
    Router::new()
        .route("/", get(serve_index))
        .route("/admin/audit", get(serve_index))
        .route("/.well-known/agent.json", get(serve_agent_card))
        .route("/a2a/card", get(serve_agent_card))
        .route("/a2a/tasks", post(create_a2a_task))
        .route("/a2a/tasks/:id", get(get_a2a_task))
        .route("/api/bots", get(list_bots))
        .route("/api/queue", get(list_queue).post(enqueue_task))
        .route("/api/tasks/:id/:action", post(control_task_route))
        .route("/api/channels", get(list_channels))
        .route(
            "/api/channels/:id/messages",
            get(list_channel_messages).post(post_channel_message),
        )
        .route("/api/audit", get(query_audit))
        .route("/ag-ui/runs", post(start_agui_run))
        .route("/ag-ui/events", get(stream_agui_events))
        .route("/bridges/inbound", post(handle_bridge_inbound))
        .route("/bridges/slack/events", post(handle_slack_webhook))
        .with_state(state)
}

pub struct CompanionWebServer {
    state: CompanionWebState,
}

impl CompanionWebServer {
    pub fn new(
        supervisor: OpenBotSupervisor,
        bridge_router: BridgeRouter,
        bearer_token: Option<String>,
        slack_signing_secret: Option<String>,
    ) -> Self {
        Self {
            state: CompanionWebState {
                supervisor,
                bridge_router,
                bearer_token,
                slack_signing_secret,
            },
        }
    }

    pub async fn bind_and_serve(
        self,
        bind_addr: &str,
    ) -> gm_core::error::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
        let listener = TcpListener::bind(bind_addr).await?;
        let local_addr = listener.local_addr()?;
        let router = build_companion_router(self.state);
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok((local_addr, handle))
    }
}

async fn serve_index() -> Html<&'static str> {
    Html(COMPANION_UI_HTML)
}

async fn serve_agent_card(State(state): State<CompanionWebState>) -> impl IntoResponse {
    Json(state.supervisor.a2a().agent_card())
}

async fn list_bots(
    headers: HeaderMap,
    State(state): State<CompanionWebState>,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    check_auth(&headers, state.bearer_token.as_deref())?;
    Ok(Json(state.supervisor.roster().list_bots()))
}

async fn list_queue(
    headers: HeaderMap,
    State(state): State<CompanionWebState>,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    check_auth(&headers, state.bearer_token.as_deref())?;
    let tasks = state
        .supervisor
        .queue()
        .list_tasks(None)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(tasks))
}

async fn enqueue_task(
    headers: HeaderMap,
    State(state): State<CompanionWebState>,
    Json(req): Json<EnqueueRequest>,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    check_auth(&headers, state.bearer_token.as_deref())?;
    let task = state
        .supervisor
        .submit_task(
            req.bot_id.as_deref(),
            req.channel_id.as_deref(),
            &req.instruction,
            req.priority,
        )
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok((StatusCode::CREATED, Json(task)))
}

async fn create_a2a_task(
    headers: HeaderMap,
    State(state): State<CompanionWebState>,
    Json(req): Json<EnqueueRequest>,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    check_auth(&headers, state.bearer_token.as_deref())?;
    let task = state
        .supervisor
        .submit_task(
            req.bot_id.as_deref(),
            req.channel_id.as_deref(),
            &req.instruction,
            req.priority,
        )
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok((StatusCode::CREATED, Json(task)))
}

async fn get_a2a_task(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(state): State<CompanionWebState>,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    check_auth(&headers, state.bearer_token.as_deref())?;
    let task = state
        .supervisor
        .queue()
        .get_task(&id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(task))
}

async fn control_task_route(
    headers: HeaderMap,
    Path((id, action)): Path<(String, String)>,
    State(state): State<CompanionWebState>,
    body: Option<Json<serde_json::Value>>,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    check_auth(&headers, state.bearer_token.as_deref())?;
    let ok = match action.as_str() {
        "pause" => state
            .supervisor
            .control_task(&id, &QueueControlAction::Pause)
            .map_err(|_| StatusCode::BAD_REQUEST)?,
        "resume" => state
            .supervisor
            .control_task(&id, &QueueControlAction::Resume)
            .map_err(|_| StatusCode::BAD_REQUEST)?,
        "cancel" => state
            .supervisor
            .control_task(&id, &QueueControlAction::Cancel)
            .map_err(|_| StatusCode::BAD_REQUEST)?,
        "wheel-resume" => {
            let note = body
                .as_ref()
                .and_then(|b| b.get("note"))
                .and_then(|v| v.as_str());
            state
                .supervisor
                .queue()
                .resume_from_human(&id, note)
                .map_err(|_| StatusCode::BAD_REQUEST)?
        }
        "reassign" => {
            let target = body
                .as_ref()
                .and_then(|b| b.get("target_bot_id"))
                .and_then(|v| v.as_str())
                .ok_or(StatusCode::BAD_REQUEST)?;
            state
                .supervisor
                .control_task(
                    &id,
                    &QueueControlAction::Reassign {
                        target_bot_id: target.to_string(),
                    },
                )
                .map_err(|_| StatusCode::BAD_REQUEST)?
        }
        _ => return Err(StatusCode::BAD_REQUEST),
    };
    Ok(Json(serde_json::json!({ "ok": ok, "task_id": id })))
}

async fn list_channels(
    headers: HeaderMap,
    State(state): State<CompanionWebState>,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    check_auth(&headers, state.bearer_token.as_deref())?;
    let chans = state
        .supervisor
        .channels()
        .list_channels(false)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(chans))
}

async fn list_channel_messages(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(state): State<CompanionWebState>,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    check_auth(&headers, state.bearer_token.as_deref())?;
    let msgs = state
        .supervisor
        .channels()
        .list_messages(&id, 100)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(msgs))
}

async fn post_channel_message(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(state): State<CompanionWebState>,
    Json(body): Json<ChannelPostBody>,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    check_auth(&headers, state.bearer_token.as_deref())?;
    let author = body.author.as_deref().unwrap_or("operator");
    let (msg, spawned) = state
        .supervisor
        .post_channel_message(&id, author, None, &body.content)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok(Json(serde_json::json!({
        "message": msg,
        "spawned_tasks": spawned,
    })))
}

async fn query_audit(
    headers: HeaderMap,
    Query(q): Query<AuditQuery>,
    State(state): State<CompanionWebState>,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    check_auth(&headers, state.bearer_token.as_deref())?;
    let cat = match q.category.as_deref() {
        Some("permitted") => Some(AuditCategory::Permitted),
        Some("refused") => Some(AuditCategory::Refused),
        Some("failed") => Some(AuditCategory::Failed),
        _ => None,
    };
    let records = state.supervisor.audit().query_governance_entries(
        cat,
        q.bot_id.as_deref(),
        q.limit.unwrap_or(100),
    );
    Ok(Json(records))
}

async fn start_agui_run(
    headers: HeaderMap,
    State(state): State<CompanionWebState>,
    Json(req): Json<AgUiRunRequest>,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    check_auth(&headers, state.bearer_token.as_deref())?;
    let task = state
        .supervisor
        .submit_task(req.bot_id.as_deref(), Some("general"), &req.instruction, 10)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    state.supervisor.emit_agui(
        "RUN_QUEUED",
        &task.id,
        &task.bot_id,
        serde_json::json!({ "instruction": task.instruction }),
    );
    Ok((StatusCode::CREATED, Json(task)))
}

async fn stream_agui_events(
    headers: HeaderMap,
    State(state): State<CompanionWebState>,
) -> std::result::Result<
    Sse<impl tokio_stream::Stream<Item = std::result::Result<Event, Infallible>>>,
    StatusCode,
> {
    check_auth(&headers, state.bearer_token.as_deref())?;
    let rx = state.supervisor.agui_sender().subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|item| match item {
        Ok(evt) => {
            let json = serde_json::to_string(&evt).unwrap_or_else(|_| "{}".into());
            Some(Ok(Event::default().event(evt.event_type).data(json)))
        }
        Err(_) => None,
    });
    Ok(Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}

async fn handle_bridge_inbound(
    headers: HeaderMap,
    State(state): State<CompanionWebState>,
    Json(msg): Json<BridgeInboundMessage>,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    check_auth(&headers, state.bearer_token.as_deref())?;
    let reply = state
        .bridge_router
        .handle_inbound(&msg)
        .map_err(|_| StatusCode::FORBIDDEN)?;
    Ok(Json(reply))
}

async fn handle_slack_webhook(
    headers: HeaderMap,
    State(state): State<CompanionWebState>,
    raw_body: String,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    if let Some(ref secret) = state.slack_signing_secret {
        let ts = headers
            .get("x-slack-request-timestamp")
            .and_then(|v| v.to_str().ok())
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let sig = headers
            .get("x-slack-signature")
            .and_then(|v| v.to_str().ok())
            .ok_or(StatusCode::UNAUTHORIZED)?;
        if !verify_slack_signature(secret, ts, &raw_body, sig) {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    let val: serde_json::Value =
        serde_json::from_str(&raw_body).map_err(|_| StatusCode::BAD_REQUEST)?;

    // Handle Slack URL verification challenge
    if val.get("type").and_then(|v| v.as_str()) == Some("url_verification") {
        let challenge = val.get("challenge").and_then(|v| v.as_str()).unwrap_or("");
        return Ok(Json(serde_json::json!({ "challenge": challenge })));
    }

    let channel_id = val
        .get("event")
        .and_then(|e| e.get("channel"))
        .and_then(|v| v.as_str())
        .unwrap_or("general")
        .to_string();
    let user = val
        .get("event")
        .and_then(|e| e.get("user"))
        .and_then(|v| v.as_str())
        .unwrap_or("slack_user")
        .to_string();
    let text = val
        .get("event")
        .and_then(|e| e.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let inbound = BridgeInboundMessage {
        platform: "slack".into(),
        external_channel_id: channel_id,
        sender_name: user,
        text,
    };
    let reply = state
        .bridge_router
        .handle_inbound(&inbound)
        .map_err(|_| StatusCode::FORBIDDEN)?;
    Ok(Json(serde_json::json!(reply)))
}
