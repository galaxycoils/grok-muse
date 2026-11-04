use crate::bridges::{BridgeInboundMessage, BridgeRouter, verify_slack_signature};
use crate::supervisor::OpenBotSupervisor;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
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
const COMPANION_UI_JS: &str = include_str!("companion_ui.js");

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

#[derive(Debug, Deserialize)]
pub struct CreateChannelBody {
    pub id: String,
    #[serde(default)]
    pub topic: String,
    pub owner_bot_id: Option<String>,
    #[serde(default)]
    pub participants: Vec<String>,
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
    let Some(expected) = expected_token.filter(|t| !t.trim().is_empty()) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    let auth_val = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if crate::bridges::constant_time_eq(
        auth_val.as_bytes(),
        format!("Bearer {expected}").as_bytes(),
    ) {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

pub fn build_companion_router(state: CompanionWebState) -> Router {
    Router::new()
        .route("/", get(serve_index))
        .route("/companion.js", get(serve_script))
        .route("/admin/audit", get(serve_index))
        .route("/.well-known/agent.json", get(serve_agent_card))
        .route("/a2a/card", get(serve_agent_card))
        .route("/a2a/tasks", post(create_a2a_task))
        .route("/a2a/tasks/:id", get(get_a2a_task))
        .route("/api/runtime", get(runtime_status))
        .route("/api/events", get(replay_events))
        .route("/api/bots", get(list_bots))
        .route("/api/queue", get(list_queue).post(enqueue_task))
        .route("/api/tasks/:id/:action", post(control_task_route))
        .route("/api/channels", get(list_channels).post(create_channel))
        .route(
            "/api/channels/:id/messages",
            get(list_channel_messages).post(post_channel_message),
        )
        .route("/api/audit", get(query_audit))
        .route("/ag-ui/runs", post(start_agui_run))
        .route("/ag-ui/events", get(stream_agui_events))
        .route("/bridges/inbound", post(handle_bridge_inbound))
        .route("/bridges/slack/events", post(handle_slack_webhook))
        .layer(axum::middleware::from_fn(|request:axum::extract::Request,next:axum::middleware::Next| async move {
            let mut response=next.run(request).await;
            for (key,value) in [("x-content-type-options","nosniff"),("referrer-policy","no-referrer"),("cache-control","no-store"),("content-security-policy","default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; object-src 'none'")] {
                response.headers_mut().insert(axum::http::HeaderName::from_static(key),axum::http::HeaderValue::from_static(value));
            }
            response
        }))
        .layer(DefaultBodyLimit::max(1024 * 1024))
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
        if self
            .state
            .bearer_token
            .as_ref()
            .is_none_or(|t| t.trim().is_empty())
        {
            return Err(gm_core::error::GmError::Config(
                "Companion authentication token is required".into(),
            ));
        }
        let requested: SocketAddr = bind_addr.parse().map_err(|_| {
            gm_core::error::GmError::Config("Use an explicit loopback IP and port".into())
        })?;
        if !requested.ip().is_loopback() {
            return Err(gm_core::error::GmError::Config(
                "Remote companion access requires a TLS pairing gateway; bind to loopback".into(),
            ));
        }
        let listener = TcpListener::bind(requested).await?;
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

async fn serve_script() -> impl IntoResponse {
    (
        [("content-type", "text/javascript; charset=utf-8")],
        COMPANION_UI_JS,
    )
}

async fn serve_agent_card(State(state): State<CompanionWebState>) -> impl IntoResponse {
    Json(state.supervisor.a2a().agent_card())
}

async fn runtime_status(
    headers: HeaderMap,
    State(state): State<CompanionWebState>,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    check_auth(&headers, state.bearer_token.as_deref())?;
    Ok(Json(state.supervisor.runtime_status()))
}

#[derive(Deserialize)]
struct EventCursor {
    #[serde(default)]
    after: u64,
}
async fn replay_events(
    headers: HeaderMap,
    Query(cursor): Query<EventCursor>,
    State(state): State<CompanionWebState>,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    check_auth(&headers, state.bearer_token.as_deref())?;
    let events = state
        .supervisor
        .queue()
        .events_after(cursor.after, 1000)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let next = events.last().map(|(id, _)| *id).unwrap_or(cursor.after);
    let events: Vec<_> = events
        .into_iter()
        .map(|(id, mut e)| {
            e["sequence"] = id.into();
            e
        })
        .collect();
    Ok(Json(
        serde_json::json!({"protocol_version":1,"events":events,"next_cursor":next}),
    ))
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

async fn create_channel(
    headers: HeaderMap,
    State(state): State<CompanionWebState>,
    Json(body): Json<CreateChannelBody>,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    check_auth(&headers, state.bearer_token.as_deref())?;
    if state
        .supervisor
        .channels()
        .get_channel(&body.id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .is_some()
    {
        return Err(StatusCode::CONFLICT);
    }
    let channel = state
        .supervisor
        .create_channel(
            &body.id,
            &body.topic,
            body.owner_bot_id.as_deref(),
            &body.participants,
        )
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok((StatusCode::CREATED, Json(channel)))
}

async fn list_channel_messages(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(state): State<CompanionWebState>,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    check_auth(&headers, state.bearer_token.as_deref())?;
    state
        .supervisor
        .channels()
        .get_channel(&id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
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
            Some(Ok(Event::default()
                .id(evt.sequence.to_string())
                .event(evt.event_type)
                .data(json)))
        }
        // A lagged client must reload snapshots rather than silently miss state changes.
        Err(_) => Some(Ok(Event::default().event("RESYNC_REQUIRED").data("{}"))),
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
    {
        let secret = state
            .slack_signing_secret
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
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

    let event = val.get("event").ok_or(StatusCode::BAD_REQUEST)?;
    if event.get("bot_id").is_some() || event.get("subtype").is_some() {
        return Ok(Json(serde_json::json!({"accepted":false,"ignored":true})));
    }
    let event_id = val
        .get("event_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or(StatusCode::BAD_REQUEST)?;
    // Delivery admission is durable across daemon restarts.
    if !state
        .supervisor
        .queue()
        .claim_delivery(&format!("slack:{event_id}"))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        return Ok(Json(serde_json::json!({"accepted":true,"duplicate":true})));
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
