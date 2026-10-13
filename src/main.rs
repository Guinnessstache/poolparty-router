//! PoolParty router: one OpenAI-compatible endpoint in front of the Mac
//! Studio, the RTX 4070 Ti and a Claude Code handoff.

mod classify;
mod config;
mod escalation;
mod handoff;
mod nodes;

use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use classify::{decide, Decision, Kind, Target};
use config::{Config, NodeCfg};
use escalation::Session;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct App {
    cfg: Config,
    http: reqwest::Client,
    nodes: nodes::Registry,
    sessions: Mutex<HashMap<String, Session>>,
    /// Session ids with a manual escalation pending.
    forced: Mutex<Vec<String>>,
    activity: Mutex<VecDeque<Activity>>,
    /// Runtime switch from the dashboard; starts from escalation.mode.
    handoffs_paused: AtomicBool,
}

#[derive(Clone, serde::Serialize)]
struct Activity {
    time: String,
    session: Option<String>,
    kind: Option<Kind>,
    est_tokens: usize,
    route: String,
    rule: String,
    status: u16,
    ms: u128,
}

type Shared = Arc<App>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("router.toml"));
    if !path.exists() {
        std::fs::write(&path, include_str!("../router.example.toml"))?;
        println!("Wrote a starter config to {}. Edit the node URLs, then run again.", path.display());
        return Ok(());
    }
    let cfg = Config::load(&path)?;
    init_logging(cfg.router.log_file.as_deref())?;

    let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(3)).build()?;
    let registry = nodes::Registry::new(&cfg.nodes.mac, &cfg.nodes.jayspc);
    registry.spawn_poller(
        http.clone(),
        [(Target::Mac, cfg.nodes.mac.clone()), (Target::Jayspc, cfg.nodes.jayspc.clone())],
        Duration::from_secs(cfg.router.poll_seconds.max(1)),
    );

    let listen = cfg.router.listen.clone();
    let paused = cfg.escalation.mode == "off";
    let app: Shared = Arc::new(App {
        cfg,
        http,
        nodes: registry,
        sessions: Mutex::new(HashMap::new()),
        forced: Mutex::new(Vec::new()),
        activity: Mutex::new(VecDeque::new()),
        handoffs_paused: AtomicBool::new(paused),
    });

    let router = Router::new()
        .route("/", get(dashboard))
        .route("/health", get(|| async { "ok" }))
        .route("/router/handoffs", get(list_handoffs))
        .route("/router/mode", post(set_mode))
        .route("/status", get(status))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(proxy))
        .route("/v1/completions", post(proxy))
        .route("/infill", post(proxy))
        .route("/router/escalate", post(escalate))
        .route("/router/reset", post(reset))
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
        .with_state(app);

    let mut servers = Vec::new();
    for addr in listen {
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| anyhow::anyhow!("can't listen on {addr}: {e}"))?;
        tracing::info!("PoolParty router listening on http://{addr}/v1  (model \"auto\"), dashboard at http://{addr}/");
        let r = router.clone();
        servers.push(tokio::spawn(async move { axum::serve(listener, r).await }));
    }
    for s in servers {
        s.await??;
    }
    Ok(())
}

fn authorized(app: &App, headers: &HeaderMap) -> bool {
    let Some(key) = &app.cfg.router.api_key else { return true };
    let got = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|h| h.to_str().ok()));
    got == Some(key.as_str())
}

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    let msg = msg.into();
    (status, Json(json!({"error": {"message": msg, "type": "poolparty_router"}}))).into_response()
}

async fn models(State(app): State<Shared>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong router api key");
    }
    let data: Vec<Value> = ["auto", "mac", "jayspc"]
        .iter()
        .map(|id| json!({"id": id, "object": "model", "owned_by": "poolparty"}))
        .collect();
    Json(json!({"object": "list", "data": data})).into_response()
}

async fn status(State(app): State<Shared>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong router api key");
    }
    let mut sessions: Vec<_> =
        app.sessions.lock().unwrap().values().map(|s| (s.last_seen, s.summary())).collect();
    sessions.sort_by(|a, b| b.0.cmp(&a.0));
    let sessions: Vec<_> = sessions.into_iter().take(20).map(|(_, s)| s).collect();
    let activity: Vec<_> = app.activity.lock().unwrap().iter().rev().cloned().collect();
    Json(json!({
        "nodes": app.nodes.snapshot(),
        "version": env!("CARGO_PKG_VERSION"),
        "handoffs": handoff_files(&app, 10),
        "escalation": {
            "paused": app.handoffs_paused.load(Ordering::Relaxed),
            "mode": app.cfg.escalation.mode,
            "strikes_to_escalate": app.cfg.escalation.strikes_to_escalate,
            "handoff_dir": app.cfg.handoff_dir(),
        },
        "sessions": sessions,
        "activity": activity,
    }))
    .into_response()
}

/// Body: {"session": "<id>"}; without one, the most recently active session.
async fn escalate(State(app): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    if !authorized(&app, &headers) {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong router api key");
    }
    let Some(id) = pick_session(&app, &body) else {
        return error(StatusCode::NOT_FOUND, "no active session to escalate");
    };
    app.forced.lock().unwrap().push(id.clone());
    Json(json!({"session": id, "note": "the next request in this session hands off to Claude Code"}))
        .into_response()
}

async fn reset(State(app): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    if !authorized(&app, &headers) {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong router api key");
    }
    let Some(id) = pick_session(&app, &body) else {
        return error(StatusCode::NOT_FOUND, "no such session");
    };
    if let Some(s) = app.sessions.lock().unwrap().get_mut(&id) {
        s.reset();
    }
    Json(json!({"session": id, "strikes": 0})).into_response()
}

fn pick_session(app: &App, body: &[u8]) -> Option<String> {
    let wanted: Option<String> = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|b| b["session"].as_str().map(str::to_string));
    let sessions = app.sessions.lock().unwrap();
    match wanted {
        Some(id) => sessions.contains_key(&id).then_some(id),
        None => sessions.values().max_by_key(|s| s.last_seen).map(|s| s.id.clone()),
    }
}

fn node_cfg(app: &App, t: Target) -> &NodeCfg {
    match t {
        Target::Mac => &app.cfg.nodes.mac,
        Target::Jayspc => &app.cfg.nodes.jayspc,
    }
}

async fn proxy(State(app): State<Shared>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let started = Instant::now();
    if !authorized(&app, &headers) {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong router api key");
    }
    let path = uri.path().to_string();
    let mut req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_REQUEST, format!("request body is not JSON: {e}")),
    };
    let shape = classify::classify(&path, &req);
    let stream = req["stream"].as_bool().unwrap_or(false);
    let asked = req["model"].as_str().unwrap_or("auto").to_string();
    let pinned = match asked.as_str() {
        "mac" => Some(Target::Mac),
        "jayspc" => Some(Target::Jayspc),
        m if Some(m) == app.cfg.nodes.mac.model.as_deref() => Some(Target::Mac),
        m if Some(m) == app.cfg.nodes.jayspc.model.as_deref() => Some(Target::Jayspc),
        _ => None,
    };

    // Session tracking and escalation only apply to chat-style requests.
    let messages: Vec<Value> = req["messages"].as_array().cloned().unwrap_or_default();
    let session_id = (!messages.is_empty()).then(|| {
        escalation::session_key(headers.get("x-session-id").and_then(|h| h.to_str().ok()), &messages)
    });
    let handoff_on = !app.handoffs_paused.load(Ordering::Relaxed);

    if let Some(id) = &session_id {
        let forced = {
            let mut f = app.forced.lock().unwrap();
            let hit = f.contains(id);
            f.retain(|x| x != id);
            hit
        };
        let stuck = {
            let mut sessions = app.sessions.lock().unwrap();
            // Forget sessions idle for a day so the map doesn't grow forever.
            if sessions.len() > 200 {
                let day = Duration::from_secs(24 * 3600);
                sessions.retain(|_, s| s.last_seen.is_some_and(|t| t.elapsed() < day));
            }
            let s = sessions
                .entry(id.clone())
                .or_insert_with(|| Session::new(id.clone()));
            s.observe(&messages);
            if forced {
                s.reasons.push("escalated by hand".into());
            }
            forced || s.strikes >= app.cfg.escalation.strikes_to_escalate
        };
        if handoff_on && stuck {
            return do_handoff(&app, id, &messages, stream, &asked, &shape, started).await;
        }
    }

    let mut decision = decide(&shape, pinned, app.nodes.view(Target::Mac), app.nodes.view(Target::Jayspc));
    let mut retried = false;
    loop {
        let (target, rule) = match decision {
            Decision::Route { target, rule } => (target, rule),
            Decision::NoLocal { reason } => {
                // A prompt too big for any local model is an immediate handoff.
                if let (Some(id), true, true) = (&session_id, handoff_on, reason.contains("context")) {
                    if let Some(s) = app.sessions.lock().unwrap().get_mut(id) {
                        s.reasons.push(reason.clone());
                    }
                    return do_handoff(&app, id, &messages, stream, &asked, &shape, started).await;
                }
                record(&app, &session_id, Some(&shape), "none", &reason, 503, started);
                return error(StatusCode::SERVICE_UNAVAILABLE, format!("PoolParty: {reason}"));
            }
        };

        let node = node_cfg(&app, target);
        if pinned.is_some() || asked == "auto" {
            let model = node.model.clone().or_else(|| {
                app.nodes.snapshot().into_iter().find(|n| n.name == target.name()).and_then(|n| n.model)
            });
            if let Some(m) = model {
                req["model"] = json!(m);
            }
        }
        let upstream = if path == "/infill" {
            format!("{}/infill", node.url.strip_suffix("/v1").unwrap_or(&node.url))
        } else {
            format!("{}{}", node.url, path.strip_prefix("/v1").unwrap_or(&path))
        };

        let mut r = app.http.post(&upstream).json(&req);
        if let Some(k) = &node.api_key {
            r = r.bearer_auth(k);
        }
        match r.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                record(&app, &session_id, Some(&shape), target.name(), rule, status, started);
                let mut out = Response::builder()
                    .status(status)
                    .header("x-poolparty-node", HeaderValue::from_static(target.name()))
                    .header("x-poolparty-rule", HeaderValue::from_static(rule));
                if let Some(ct) = resp.headers().get("content-type") {
                    out = out.header("content-type", ct.clone());
                }
                return out
                    .body(Body::from_stream(resp.bytes_stream()))
                    .unwrap_or_else(|e| error(StatusCode::BAD_GATEWAY, format!("building response: {e}")));
            }
            Err(e) => {
                let msg = nodes::short_err(&e);
                tracing::warn!(node = target.name(), %msg, "upstream failed");
                app.nodes.mark_down(target, msg.clone());
                if retried || pinned.is_some() {
                    record(&app, &session_id, Some(&shape), target.name(), &msg, 502, started);
                    return error(
                        StatusCode::BAD_GATEWAY,
                        format!("PoolParty: {} unreachable: {msg}", target.name()),
                    );
                }
                retried = true;
                decision = decide(&shape, None, app.nodes.view(Target::Mac), app.nodes.view(Target::Jayspc));
            }
        }
    }
}

async fn do_handoff(
    app: &App,
    id: &str,
    messages: &[Value],
    stream: bool,
    model: &str,
    shape: &classify::Shape,
    started: Instant,
) -> Response {
    let summary = if app.cfg.escalation.summarize_with_local_model && app.nodes.view(Target::Mac).online {
        let mac = &app.cfg.nodes.mac;
        handoff::summarize(&app.http, &mac.url, mac.api_key.as_deref(), mac.model.as_deref(), messages).await
    } else {
        None
    };
    let written = {
        let mut sessions = app.sessions.lock().unwrap();
        let Some(s) = sessions.get_mut(id) else {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "session vanished");
        };
        let res = handoff::write(&app.cfg.handoff_dir(), s, messages, summary);
        if let Ok(h) = &res {
            s.escalations += 1;
            s.last_handoff = Some(h.path.display().to_string());
            s.reset();
        }
        res
    };
    let h = match written {
        Ok(h) => h,
        Err(e) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("PoolParty: couldn't write handoff file: {e}"),
            )
        }
    };
    tracing::info!(session = id, file = %h.path.display(), "handed off to Claude Code");
    record(app, &Some(id.to_string()), Some(shape), "claude-code", "handoff", 200, started);

    let text = handoff::stop_message(&h);
    if stream {
        Response::builder()
            .header("content-type", "text/event-stream")
            .header("x-poolparty-node", "claude-code")
            .body(Body::from(handoff::completion_sse(model, &text)))
            .unwrap()
    } else {
        ([("x-poolparty-node", "claude-code")], Json(handoff::completion_json(model, &text))).into_response()
    }
}

fn record(
    app: &App,
    session: &Option<String>,
    shape: Option<&classify::Shape>,
    route: &str,
    rule: &str,
    status: u16,
    started: Instant,
) {
    let a = Activity {
        time: chrono::Local::now().format("%H:%M:%S").to_string(),
        session: session.clone(),
        kind: shape.map(|s| s.kind),
        est_tokens: shape.map_or(0, |s| s.est_tokens),
        route: route.to_string(),
        rule: rule.to_string(),
        status,
        ms: started.elapsed().as_millis(),
    };
    tracing::info!(route = %a.route, rule = %a.rule, status, ms = a.ms, "request");
    let mut q = app.activity.lock().unwrap();
    q.push_back(a);
    while q.len() > 50 {
        q.pop_front();
    }
}

fn init_logging(file: Option<&std::path::Path>) -> anyhow::Result<()> {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let console = fmt::layer();
    let file_layer = match file {
        Some(p) => {
            if let Some(dir) = p.parent() {
                std::fs::create_dir_all(dir).ok();
            }
            let f = std::fs::OpenOptions::new().create(true).append(true).open(p)?;
            Some(fmt::layer().with_ansi(false).with_writer(Mutex::new(f)))
        }
        None => None,
    };
    tracing_subscriber::registry().with(filter).with(console).with(file_layer).init();
    Ok(())
}

async fn dashboard() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("dashboard.html"))
}

#[derive(serde::Serialize)]
struct HandoffFile {
    name: String,
    path: String,
    modified: String,
    command: String,
}

fn handoff_files(app: &App, limit: usize) -> Vec<HandoffFile> {
    let dir = app.cfg.handoff_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut files: Vec<_> = rd
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".md"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files
        .into_iter()
        .take(limit)
        .map(|(t, p)| {
            let modified: chrono::DateTime<chrono::Local> = t.into();
            let path = p.display().to_string();
            HandoffFile {
                name: p.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                command: format!("claude \"Read {path} and take over the task it describes.\""),
                modified: modified.format("%b %-d, %H:%M").to_string(),
                path,
            }
        })
        .collect()
}

async fn list_handoffs(State(app): State<Shared>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong router api key");
    }
    Json(json!({"handoffs": handoff_files(&app, 100)})).into_response()
}

/// Body: {"paused": true|false}
async fn set_mode(State(app): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    if !authorized(&app, &headers) {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong router api key");
    }
    let Some(paused) = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v["paused"].as_bool()) else {
        return error(StatusCode::BAD_REQUEST, "send {\"paused\": true} or {\"paused\": false}");
    };
    app.handoffs_paused.store(paused, Ordering::Relaxed);
    tracing::info!(paused, "handoffs switched");
    Json(json!({"paused": paused})).into_response()
}
