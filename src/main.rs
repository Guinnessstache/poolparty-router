//! PoolParty Router: one OpenAI-compatible endpoint in front of this PC's GPU,
//! a big remote machine (e.g. a Mac Studio) and a Claude Code handoff, with
//! a desktop app UI to run it all.

// Release builds on Windows are a windowed app: no console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod apps;
mod blueprint;
mod claude;
mod classify;
mod config;
mod escalation;
mod handoff;
mod nodes;
mod supervisor;
mod sys;
mod telemetry;

use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, DefaultBodyLimit, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use classify::{decide, Decision, Kind, Target};
use config::{Config, NodeCfg};
use escalation::Session;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use telemetry::{events, Traffic};

struct App {
    cfg: Arc<RwLock<Config>>,
    cfg_path: PathBuf,
    http: reqwest::Client,
    nodes: nodes::Registry,
    model: supervisor::Supervisor,
    traffic: Arc<Traffic>,
    gpu: Mutex<Option<sys::Gpu>>,
    models: Mutex<Vec<sys::ModelFile>>,
    sessions: Mutex<HashMap<String, Session>>,
    /// Session ids with a manual handoff pending.
    forced: Mutex<Vec<String>>,
    activity: Mutex<VecDeque<Activity>>,
    handoffs_paused: AtomicBool,
    started: Instant,
    claude: Mutex<claude::ClaudeStatus>,
    apps: Mutex<Vec<apps::AppStatus>>,
    planner: blueprint::Planner,
    /// The apps we paused this PC's model for (empty = not paused by us).
    paused_for: Mutex<Vec<&'static str>>,
}

impl App {
    fn cfg(&self) -> Config {
        self.cfg.read().unwrap().clone()
    }
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

struct Args {
    config: Option<PathBuf>,
    background: bool,
}

fn parse_args() -> Args {
    let mut a = Args { config: None, background: false };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--background" | "--no-window" => a.background = true,
            "--config" => a.config = it.next().map(PathBuf::from),
            other if !other.starts_with("--") => a.config = Some(PathBuf::from(other)),
            _ => {}
        }
    }
    a
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = parse_args();
    // Settings: the path given, else router.toml here (for developers), else
    // the app's own folder.
    let cfg_path = args.config.clone().unwrap_or_else(|| {
        let here = PathBuf::from("router.toml");
        if here.is_file() { here } else { sys::app_dir().join("router.toml") }
    });
    let cfg = Config::load_or_create(&cfg_path)?;
    init_logging(cfg.router.log_file.as_deref())?;

    let listen = cfg.router.listen.clone();
    let ui_url = format!("http://{}/", listen.first().cloned().unwrap_or_else(|| "127.0.0.1:4000".into()));

    // Bind first: if another copy is already running, just show its window.
    let mut listeners = Vec::new();
    for addr in &listen {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => listeners.push((addr.clone(), l)),
            Err(e) => {
                let health = format!("http://{addr}/health");
                let running = reqwest::get(&health).await.map(|r| r.status().is_success()).unwrap_or(false);
                if running {
                    tracing::info!(target: "router", "already running; opening its window");
                    if !args.background {
                        sys::open_window(&ui_url);
                    }
                    return Ok(());
                }
                anyhow::bail!("can't listen on {addr}: {e}");
            }
        }
    }

    let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(3)).build()?;
    let paused = cfg.escalation.mode == "off";
    let model = supervisor::Supervisor::new(&cfg.local_model);
    let app: Shared = Arc::new(App {
        cfg: Arc::new(RwLock::new(cfg.clone())),
        cfg_path: cfg_path.clone(),
        http: http.clone(),
        nodes: nodes::Registry::new(),
        model,
        traffic: Traffic::new(),
        gpu: Mutex::new(None),
        models: Mutex::new(Vec::new()),
        sessions: Mutex::new(HashMap::new()),
        forced: Mutex::new(Vec::new()),
        activity: Mutex::new(VecDeque::new()),
        handoffs_paused: AtomicBool::new(paused),
        started: Instant::now(),
        claude: Mutex::new(claude::ClaudeStatus::default()),
        apps: Mutex::new(Vec::new()),
        planner: blueprint::Planner::default(),
        paused_for: Mutex::new(Vec::new()),
    });
    tracing::info!(target: "router", "PoolParty Router {} starting (settings: {})",
        env!("CARGO_PKG_VERSION"), cfg_path.display());

    app.nodes.spawn_poller(http, app.cfg.clone());
    spawn_ticker(app.clone());
    spawn_claude_watch(app.clone());
    spawn_apps_watch(app.clone());
    {
        let app = app.clone();
        tokio::task::spawn_blocking(move || rescan(&app));
    }
    if cfg.local_model.autostart && !cfg.local_model.model.is_empty() && cfg.nodes.local.url.is_empty() {
        if let Err(e) = app.model.start(&cfg.local_model).await {
            tracing::warn!(target: "router", "couldn't start this PC's model: {e}");
        }
    }

    let router = Router::new()
        .route("/", get(ui))
        .route("/health", get(|| async { "ok" }))
        .route("/status", get(status))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(proxy))
        .route("/v1/completions", post(proxy))
        .route("/infill", post(proxy))
        .route("/router/events", get(sse))
        .route("/router/setup", get(setup_info))
        .route("/router/settings", post(save_settings))
        .route("/router/discover", post(discover))
        .route("/router/model/start", post(model_start))
        .route("/router/model/stop", post(model_stop))
        .route("/router/test", post(test_node))
        .route("/router/open", post(open_thing))
        .route("/router/autostart", post(autostart))
        .route("/router/quit", post(quit))
        .route("/router/claude", get(claude_info))
        .route("/router/claude/refresh", post(claude_refresh))
        .route("/router/apps/connect", post(app_connect))
        .route("/router/apps/unreal-setup", post(unreal_setup))
        .route("/router/blueprint", get(blueprint_get).post(blueprint_start))
        .route("/router/blueprint/cancel", post(blueprint_cancel))
        .route("/router/pick-folder", post(pick_folder))
        .route("/router/escalate", post(escalate))
        .route("/router/reset", post(reset))
        .route("/router/mode", post(set_mode))
        .route("/router/handoffs", get(list_handoffs))
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
        .with_state(app.clone());

    let mut servers = Vec::new();
    for (addr, listener) in listeners {
        tracing::info!(target: "router", "listening on http://{addr}/v1 (model \"auto\")");
        let r = router.clone().into_make_service_with_connect_info::<SocketAddr>();
        servers.push(tokio::spawn(async move { axum::serve(listener, r).await }));
    }
    if !args.background {
        sys::open_window(&ui_url);
        tokio::task::spawn_blocking(sys::ensure_start_menu_shortcut);
    }

    let app2 = app.clone();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        r = futures_util::future::select_all(servers) => { r.0??; }
    }
    app2.model.stop().await;
    Ok(())
}

// ------------------------------------------------------------ background work

fn rescan(app: &App) {
    let g = sys::gpu();
    let gb = g.as_ref().map(|g| g.memory_total_gb);
    *app.gpu.lock().unwrap() = g;
    let extra = app.cfg.read().unwrap().local_model.model_dirs.clone();
    let found = sys::find_models(&extra, gb);
    let ls = supervisor::resolve_llama_server(&app.cfg.read().unwrap().local_model.llama_server);
    tracing::info!(target: "router", "found {} model file(s); llama-server: {}", found.len(),
        ls.map(|p| p.display().to_string()).unwrap_or_else(|| "not found".into()));
    *app.models.lock().unwrap() = found;
}

/// Once a second: traffic rates, machine states and GPU load to the UI.
fn spawn_ticker(app: Shared) {
    tokio::spawn(async move {
        let mut n: u64 = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            n += 1;
            if n % 2 == 0 {
                let a = app.clone();
                if let Ok(g) = tokio::task::spawn_blocking(sys::gpu).await {
                    *a.gpu.lock().unwrap() = g;
                }
            }
            app.traffic.sample();
            events().send("tick", live_state(&app));
        }
    });
}

fn claude_summary(app: &App) -> Value {
    let c = app.claude.lock().unwrap();
    json!({
        "installed": c.installed, "signed_in": c.signed_in, "auth_method": c.auth_method,
        "version": c.version, "checked": c.checked, "servers": c.servers.len(), "error": c.error,
    })
}

fn live_state(app: &App) -> Value {
    let t = app.traffic.last();
    let sessions = app.sessions.lock().unwrap();
    let stuck = sessions.values().filter(|s| s.strikes > 0).count();
    json!({
        "nodes": app.nodes.snapshot(),
        "model": app.model.status(),
        "traffic": {"local": t[telemetry::LOCAL], "remote": t[telemetry::REMOTE], "claude": t[telemetry::CLAUDE]},
        "gpu": *app.gpu.lock().unwrap(),
        "paused": app.handoffs_paused.load(Ordering::Relaxed),
        "sessions": sessions.len(),
        "sessions_with_strikes": stuck,
        "uptime_secs": app.started.elapsed().as_secs(),
        "apps": *app.apps.lock().unwrap(),
        "claude": claude_summary(app),
        "planner": app.planner.snapshot(),
        "model_paused_for": *app.paused_for.lock().unwrap(),
    })
}

// ------------------------------------------------------------ access control

fn authorized(app: &App, headers: &HeaderMap, query_key: Option<&str>) -> bool {
    let cfg = app.cfg.read().unwrap();
    let Some(key) = &cfg.router.api_key else { return true };
    let got = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|h| h.to_str().ok()))
        .or(query_key);
    got == Some(key.as_str())
}

/// The buttons that start programs or open folders only work from this PC
/// itself, unless an API key is set.
fn control_ok(app: &App, headers: &HeaderMap, peer: SocketAddr) -> Result<(), Response> {
    if !authorized(app, headers, None) {
        return Err(error(StatusCode::UNAUTHORIZED, "missing or wrong router api key"));
    }
    let has_key = app.cfg.read().unwrap().router.api_key.is_some();
    if !peer.ip().is_loopback() && !has_key {
        return Err(error(StatusCode::FORBIDDEN, "controls only work on this PC (set an api_key to allow others)"));
    }
    Ok(())
}

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    let msg = msg.into();
    (status, Json(json!({"error": {"message": msg, "type": "poolparty_router"}}))).into_response()
}

fn ok(v: Value) -> Response {
    Json(v).into_response()
}

// ------------------------------------------------------------ UI endpoints

async fn ui() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("app.html"))
}

#[derive(serde::Deserialize)]
struct KeyQuery {
    key: Option<String>,
}

/// Live events for the app: recent log lines first, then everything new.
async fn sse(State(app): State<Shared>, headers: HeaderMap, Query(q): Query<KeyQuery>) -> Response {
    if !authorized(&app, &headers, q.key.as_deref()) {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong router api key");
    }
    let rx = events().tx.subscribe();
    let mut first: Vec<String> = events()
        .recent_log(300)
        .into_iter()
        .map(|l| json!({"type": "log", "data": l}).to_string())
        .collect();
    first.push(json!({"type": "tick", "data": live_state(&app)}).to_string());
    let backlog = futures_util::stream::iter(first.into_iter().map(|m| Ok::<_, Infallible>(Event::default().data(m))));
    let live = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(m) => return Some((Ok(Event::default().data(m.to_string())), rx)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(backlog.chain(live)).keep_alive(KeepAlive::default()).into_response()
}

async fn status(State(app): State<Shared>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers, None) {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong router api key");
    }
    let cfg = app.cfg();
    let mut sessions: Vec<_> =
        app.sessions.lock().unwrap().values().map(|s| (s.last_seen, s.summary())).collect();
    sessions.sort_by(|a, b| b.0.cmp(&a.0));
    let sessions: Vec<_> = sessions.into_iter().take(20).map(|(_, s)| s).collect();
    let activity: Vec<_> = app.activity.lock().unwrap().iter().rev().cloned().collect();
    let mut live = live_state(&app);
    live["version"] = json!(env!("CARGO_PKG_VERSION"));
    live["handoffs"] = json!(handoff_files(&cfg, 10));
    live["escalation"] = json!({
        "paused": app.handoffs_paused.load(Ordering::Relaxed),
        "mode": cfg.escalation.mode,
        "strikes_to_escalate": cfg.escalation.strikes_to_escalate,
        "handoff_dir": cfg.handoff_dir(),
    });
    live["session_list"] = json!(sessions);
    live["activity"] = json!(activity);
    live["endpoint"] = json!(format!("http://{}/v1", cfg.router.listen.first().cloned().unwrap_or_default()));
    ok(live)
}

async fn setup_info(State(app): State<Shared>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers, None) {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong router api key");
    }
    let cfg = app.cfg();
    ok(json!({
        "settings_file": app.cfg_path.display().to_string(),
        "remote": {"name": cfg.nodes.remote.name, "url": cfg.nodes.remote.url,
                   "has_api_key": cfg.nodes.remote.api_key.is_some()},
        "local": {"name": cfg.nodes.local.name, "external_url": cfg.nodes.local.url},
        "local_model": cfg.local_model,
        "escalation": {"strikes_to_escalate": cfg.escalation.strikes_to_escalate},
        "models": *app.models.lock().unwrap(),
        "llama_servers": sys::find_llama_servers().iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "llama_server_in_use": supervisor::resolve_llama_server(&cfg.local_model.llama_server).map(|p| p.display().to_string()),
        "gpu": *app.gpu.lock().unwrap(),
        "autostart": sys::autostart_enabled(),
        "handoff_dir": cfg.handoff_dir(),
        "log_file": cfg.router.log_file,
    }))
}

async fn discover(State(app): State<Shared>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Err(r) = control_ok(&app, &headers, peer) {
        return r;
    }
    let a = app.clone();
    let _ = tokio::task::spawn_blocking(move || rescan(&a)).await;
    setup_info(State(app), headers).await
}

/// Partial update: only the fields sent are changed.
async fn save_settings(State(app): State<Shared>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>, body: Bytes) -> Response {
    if let Err(r) = control_ok(&app, &headers, peer) {
        return r;
    }
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return error(StatusCode::BAD_REQUEST, "settings must be JSON");
    };
    let (before, after) = {
        let mut cfg = app.cfg.write().unwrap();
        let before = cfg.local_model.clone();
        let r = &v["remote"];
        if let Some(u) = r["url"].as_str() {
            cfg.nodes.remote.url = config::normalize_url(u, Some(8080));
        }
        if let Some(n) = r["name"].as_str().filter(|n| !n.trim().is_empty()) {
            cfg.nodes.remote.name = n.trim().to_string();
        }
        if let Some(k) = r["api_key"].as_str() {
            cfg.nodes.remote.api_key = (!k.trim().is_empty()).then(|| k.trim().to_string());
        }
        if let Some(n) = v["local"]["name"].as_str().filter(|n| !n.trim().is_empty()) {
            cfg.nodes.local.name = n.trim().to_string();
        }
        let m = &v["local_model"];
        if let Some(x) = m["model"].as_str() {
            cfg.local_model.model = x.trim().to_string();
        }
        if let Some(x) = m["llama_server"].as_str() {
            cfg.local_model.llama_server = x.trim().to_string();
        }
        if let Some(x) = m["context"].as_u64() {
            cfg.local_model.context = x.clamp(2048, 1_048_576) as u32;
        }
        if let Some(x) = m["autostart"].as_bool() {
            cfg.local_model.autostart = x;
        }
        if let Some(x) = v["escalation"]["strikes_to_escalate"].as_u64() {
            cfg.escalation.strikes_to_escalate = x.clamp(1, 20) as u32;
        }
        (before, cfg.local_model.clone())
    };
    let cfg = app.cfg();
    if let Err(e) = cfg.save(&app.cfg_path) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, format!("couldn't save settings: {e}"));
    }
    tracing::info!(target: "router", "settings saved");

    let model_changed = before.model != after.model
        || before.llama_server != after.llama_server
        || before.context != after.context;
    if after.model.is_empty() {
        app.model.stop().await;
        app.model.mark_not_set_up();
    } else if model_changed && app.model.is_active() {
        if let Err(e) = app.model.start(&after).await {
            return error(StatusCode::BAD_REQUEST, e);
        }
    } else if model_changed {
        app.model.stop().await; // leaves it "stopped" with the new model chosen
    }
    app.nodes.refresh(&app.http, &cfg).await;
    ok(json!({"saved": true}))
}

async fn model_start(State(app): State<Shared>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Err(r) = control_ok(&app, &headers, peer) {
        return r;
    }
    match app.model.start(&app.cfg().local_model).await {
        Ok(()) => ok(json!({"model": app.model.status()})),
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}

async fn model_stop(State(app): State<Shared>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Err(r) = control_ok(&app, &headers, peer) {
        return r;
    }
    app.model.stop().await;
    let cfg = app.cfg();
    app.nodes.refresh(&app.http, &cfg).await;
    ok(json!({"model": app.model.status()}))
}

/// "Test" button: reach the machine, then time a tiny generation.
/// Body: {"node": "local"|"remote", "url"?: "...", "api_key"?: "..."} — url
/// lets the app test an address before saving it.
async fn test_node(State(app): State<Shared>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>, body: Bytes) -> Response {
    if let Err(r) = control_ok(&app, &headers, peer) {
        return r;
    }
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let cfg = app.cfg();
    let remote = v["node"].as_str() == Some("remote");
    let mut node = if remote {
        cfg.nodes.remote.clone()
    } else {
        NodeCfg { url: cfg.local_url(), ..cfg.nodes.local.clone() }
    };
    if let Some(u) = v["url"].as_str().filter(|u| !u.trim().is_empty()) {
        node.url = config::normalize_url(u, Some(8080));
    }
    if let Some(k) = v["api_key"].as_str().filter(|k| !k.trim().is_empty()) {
        node.api_key = Some(k.trim().to_string());
    }
    if node.url.is_empty() {
        return ok(json!({"ok": false, "message": "Enter the machine's address first."}));
    }
    let auth = |r: reqwest::RequestBuilder| match &node.api_key {
        Some(k) => r.bearer_auth(k),
        None => r,
    };
    let t0 = Instant::now();
    let reach = auth(app.http.get(format!("{}/models", node.url)).timeout(Duration::from_secs(5))).send().await;
    let latency = t0.elapsed().as_millis() as u64;
    let model = match reach {
        Ok(r) if r.status().is_success() => {
            let j: Value = r.json().await.unwrap_or(Value::Null);
            j["data"][0]["id"].as_str().map(nodes::friendly_model_name)
        }
        Ok(r) if r.status().as_u16() == 401 => {
            return ok(json!({"ok": false, "message": "Reached it, but the API key is wrong or missing.", "latency_ms": latency}))
        }
        Ok(r) if r.status().as_u16() == 503 => {
            return ok(json!({"ok": false, "message": "Reached it; the model is still loading. Try again in a minute.", "latency_ms": latency}))
        }
        Ok(r) => return ok(json!({"ok": false, "message": format!("It answered with error {}.", r.status().as_u16())})),
        Err(e) => {
            return ok(json!({"ok": false, "message": format!("Couldn't reach {}: {}", node.url, nodes::short_err(&e))}))
        }
    };
    let gen_body = json!({
        "model": model.clone().unwrap_or_else(|| "auto".into()),
        "messages": [{"role": "user", "content": "Reply with just the word OK."}],
        "max_tokens": 24, "temperature": 0,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    let t1 = Instant::now();
    let answer = auth(app.http.post(format!("{}/chat/completions", node.url)).json(&gen_body).timeout(Duration::from_secs(120)))
        .send()
        .await;
    let secs = t1.elapsed().as_secs_f64();
    let (tps, reply) = match answer {
        Ok(r) if r.status().is_success() => {
            let j: Value = r.json().await.unwrap_or(Value::Null);
            let tps = j["timings"]["predicted_per_second"].as_f64().or_else(|| {
                j["usage"]["completion_tokens"].as_f64().map(|t| t / secs.max(0.001))
            });
            let reply = j["choices"][0]["message"]["content"].as_str().unwrap_or("").trim().to_string();
            (tps, reply)
        }
        _ => (None, String::new()),
    };
    let who = model.clone().unwrap_or_else(|| "The model".into());
    let answered = tps.is_some() || !reply.is_empty();
    let msg = match tps {
        Some(t) => format!("Connected in {latency} ms. {who} answered at {t:.0} tokens/sec."),
        None if answered => format!("Connected in {latency} ms. {who} answered in {secs:.1}s."),
        None => format!("Connected in {latency} ms, but the test answer failed."),
    };
    tracing::info!(target: "router", "test {}: {msg}", node.name);
    ok(json!({"ok": answered, "message": msg, "latency_ms": latency, "tokens_per_sec": tps,
              "model": model, "reply": reply, "url": node.url}))
}

/// Body: {"what": "handoffs"|"logs"|"settings"|"handoff", "path"?: "..."}
async fn open_thing(State(app): State<Shared>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>, body: Bytes) -> Response {
    if let Err(r) = control_ok(&app, &headers, peer) {
        return r;
    }
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let cfg = app.cfg();
    let path = match v["what"].as_str() {
        Some("handoffs") => {
            let d = cfg.handoff_dir();
            std::fs::create_dir_all(&d).ok();
            d
        }
        Some("logs") => cfg.router.log_file.clone().unwrap_or_else(|| app.cfg_path.clone()),
        Some("settings") => app.cfg_path.clone(),
        Some("blueprint") => match app.planner.snapshot().file {
            Some(f) => PathBuf::from(f),
            None => return error(StatusCode::NOT_FOUND, "no blueprint yet"),
        },
        Some("handoff") => {
            // Only files inside the handoff folder.
            let Some(p) = v["path"].as_str().map(PathBuf::from) else {
                return error(StatusCode::BAD_REQUEST, "path missing");
            };
            if !p.starts_with(cfg.handoff_dir()) {
                return error(StatusCode::FORBIDDEN, "not a handoff file");
            }
            p
        }
        _ => return error(StatusCode::BAD_REQUEST, "what must be handoffs, logs, settings or handoff"),
    };
    sys::reveal(&path);
    ok(json!({"opened": path}))
}

async fn autostart(State(app): State<Shared>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>, body: Bytes) -> Response {
    if let Err(r) = control_ok(&app, &headers, peer) {
        return r;
    }
    let on = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v["enabled"].as_bool()).unwrap_or(true);
    match sys::set_autostart(on) {
        Ok(()) => {
            tracing::info!(target: "router", "start with Windows: {}", if on { "on" } else { "off" });
            ok(json!({"autostart": on}))
        }
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}

async fn quit(State(app): State<Shared>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Err(r) = control_ok(&app, &headers, peer) {
        return r;
    }
    tracing::info!(target: "router", "quitting");
    app.model.stop().await;
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::process::exit(0);
    });
    ok(json!({"quitting": true}))
}

// ------------------------------------------------------------ model API

async fn models(State(app): State<Shared>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers, None) {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong router api key");
    }
    let data: Vec<Value> = ["auto", "local", "remote"]
        .iter()
        .map(|id| json!({"id": id, "object": "model", "owned_by": "poolparty"}))
        .collect();
    ok(json!({"object": "list", "data": data}))
}

/// Body: {"session": "<id>"}; without one, the most recently active session.
async fn escalate(State(app): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    if !authorized(&app, &headers, None) {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong router api key");
    }
    let Some(id) = pick_session(&app, &body) else {
        return error(StatusCode::NOT_FOUND, "no active session to hand off");
    };
    app.forced.lock().unwrap().push(id.clone());
    tracing::info!(target: "handoff", "session {id} will hand off on its next request");
    ok(json!({"session": id, "note": "the next request in this session hands off to Claude Code"}))
}

async fn reset(State(app): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    if !authorized(&app, &headers, None) {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong router api key");
    }
    let Some(id) = pick_session(&app, &body) else {
        return error(StatusCode::NOT_FOUND, "no such session");
    };
    if let Some(s) = app.sessions.lock().unwrap().get_mut(&id) {
        s.reset();
    }
    ok(json!({"session": id, "strikes": 0}))
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

fn node_cfg(cfg: &Config, t: Target) -> NodeCfg {
    match t {
        Target::Local => NodeCfg { url: cfg.local_url(), ..cfg.nodes.local.clone() },
        Target::Remote => cfg.nodes.remote.clone(),
    }
}

fn link(t: Target) -> usize {
    match t {
        Target::Local => telemetry::LOCAL,
        Target::Remote => telemetry::REMOTE,
    }
}

async fn proxy(State(app): State<Shared>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let started = Instant::now();
    if !authorized(&app, &headers, None) {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong router api key");
    }
    let cfg = app.cfg();
    let path = uri.path().to_string();
    let mut req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_REQUEST, format!("request body is not JSON: {e}")),
    };
    let shape = classify::classify(&path, &req);
    let stream = req["stream"].as_bool().unwrap_or(false);
    let asked = req["model"].as_str().unwrap_or("auto").to_string();
    let pinned = Target::from_model_name(&asked).or_else(|| {
        if Some(asked.as_str()) == cfg.nodes.remote.model.as_deref() {
            Some(Target::Remote)
        } else if Some(asked.as_str()) == cfg.nodes.local.model.as_deref() {
            Some(Target::Local)
        } else {
            None
        }
    });

    // Session tracking and handoffs only apply to chat-style requests.
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
            let s = sessions.entry(id.clone()).or_insert_with(|| Session::new(id.clone()));
            let before = s.strikes;
            s.observe(&messages);
            if s.strikes > before {
                tracing::info!(target: "handoff", "session \"{}\": strike {} of {} ({})",
                    escalation::truncate(&s.task, 50), s.strikes, cfg.escalation.strikes_to_escalate,
                    s.reasons.last().cloned().unwrap_or_default());
            }
            if forced {
                s.reasons.push("handed off by hand".into());
            }
            forced || s.strikes >= cfg.escalation.strikes_to_escalate
        };
        if handoff_on && stuck {
            return do_handoff(&app, &cfg, id, &messages, stream, &asked, &shape, started).await;
        }
    }

    let remote_set_up = cfg.remote_configured();
    let view = |a: &App| (a.nodes.view(Target::Remote), a.nodes.view(Target::Local));
    let (r, l) = view(&app);
    let mut decision = decide(&shape, pinned, r, l, remote_set_up);
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
                    return do_handoff(&app, &cfg, id, &messages, stream, &asked, &shape, started).await;
                }
                record(&app, &session_id, Some(&shape), "none", &reason, 503, started);
                return error(StatusCode::SERVICE_UNAVAILABLE, format!("PoolParty: {reason}"));
            }
        };

        let node = node_cfg(&cfg, target);
        if pinned.is_some() || asked == "auto" {
            let model = node.model.clone().or_else(|| app.nodes.get(target).model);
            if let Some(m) = model {
                req["model"] = json!(m);
            }
        }
        let upstream = if path == "/infill" {
            format!("{}/infill", node.url.strip_suffix("/v1").unwrap_or(&node.url))
        } else {
            format!("{}{}", node.url, path.strip_prefix("/v1").unwrap_or(&path))
        };

        let guard = app.traffic.start(link(target));
        let mut r = app.http.post(&upstream).json(&req);
        if let Some(k) = &node.api_key {
            r = r.bearer_auth(k);
        }
        match r.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                record(&app, &session_id, Some(&shape), target.name(), rule, status, started);
                let streamed = resp
                    .headers()
                    .get("content-type")
                    .and_then(|c| c.to_str().ok())
                    .is_some_and(|c| c.contains("event-stream"));
                let mut out = Response::builder()
                    .status(status)
                    .header("x-poolparty-node", HeaderValue::from_static(target.name()))
                    .header("x-poolparty-rule", HeaderValue::from_static(rule));
                if let Some(ct) = resp.headers().get("content-type") {
                    out = out.header("content-type", ct.clone());
                }
                let traffic = app.traffic.clone();
                let l = link(target);
                let body = resp.bytes_stream().map(move |chunk| {
                    let _keep = &guard; // in flight until the stream ends
                    if let Ok(b) = &chunk {
                        traffic.chunk(l, b, streamed);
                    }
                    chunk
                });
                return out
                    .body(Body::from_stream(body))
                    .unwrap_or_else(|e| error(StatusCode::BAD_GATEWAY, format!("building response: {e}")));
            }
            Err(e) => {
                drop(guard);
                let msg = nodes::short_err(&e);
                tracing::warn!(target: "router", "{} didn't answer: {msg}", node.name);
                app.nodes.mark_down(target, msg.clone());
                if retried || pinned.is_some() {
                    record(&app, &session_id, Some(&shape), target.name(), &msg, 502, started);
                    return error(StatusCode::BAD_GATEWAY, format!("PoolParty: {} unreachable: {msg}", node.name));
                }
                retried = true;
                let (r, l) = view(&app);
                decision = decide(&shape, None, r, l, remote_set_up);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn do_handoff(
    app: &App,
    cfg: &Config,
    id: &str,
    messages: &[Value],
    stream: bool,
    model: &str,
    shape: &classify::Shape,
    started: Instant,
) -> Response {
    // The strongest machine that's up writes the "what was tried" summary.
    let writer = [Target::Remote, Target::Local].into_iter().find(|t| app.nodes.view(*t).online);
    let summary = match (cfg.escalation.summarize_with_local_model, writer) {
        (true, Some(t)) => {
            tracing::info!(target: "handoff", "asking {} to summarize what was tried", node_cfg(cfg, t).name);
            let n = node_cfg(cfg, t);
            let m = n.model.clone().or_else(|| app.nodes.get(t).model);
            handoff::summarize(&app.http, &n.url, n.api_key.as_deref(), m.as_deref(), messages).await
        }
        _ => None,
    };
    let written = {
        let mut sessions = app.sessions.lock().unwrap();
        let Some(s) = sessions.get_mut(id) else {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "session vanished");
        };
        let res = handoff::write(&cfg.handoff_dir(), s, messages, summary);
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
            return error(StatusCode::INTERNAL_SERVER_ERROR, format!("PoolParty: couldn't write handoff file: {e}"))
        }
    };
    tracing::info!(target: "handoff", "handed off to Claude Code: {}", h.path.display());
    app.traffic.pulse(telemetry::CLAUDE);
    events().send("handoff", json!({"session": id, "path": h.path, "reasons": h.reasons}));
    record(app, &Some(id.to_string()), Some(shape), "claude", "handoff", 200, started);

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
    let kind = a.kind.map(|k| format!("{k:?}").to_lowercase()).unwrap_or_default();
    if status < 400 {
        tracing::info!(target: "request", "{kind} (~{} tokens) -> {}  [{}]", a.est_tokens, a.route, a.rule);
    } else {
        tracing::warn!(target: "request", "{kind} failed ({status}): {}", a.rule);
    }
    events().send("request", &a);
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
    tracing_subscriber::registry()
        .with(filter)
        .with(console)
        .with(file_layer)
        .with(telemetry::UiLayer)
        .init();
    Ok(())
}

#[derive(serde::Serialize)]
struct HandoffFile {
    name: String,
    path: String,
    modified: String,
    command: String,
}

fn handoff_files(cfg: &Config, limit: usize) -> Vec<HandoffFile> {
    let dir = cfg.handoff_dir();
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
    if !authorized(&app, &headers, None) {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong router api key");
    }
    ok(json!({"handoffs": handoff_files(&app.cfg(), 100)}))
}

/// Body: {"paused": true|false}
async fn set_mode(State(app): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    if !authorized(&app, &headers, None) {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong router api key");
    }
    let Some(paused) = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v["paused"].as_bool()) else {
        return error(StatusCode::BAD_REQUEST, "send {\"paused\": true} or {\"paused\": false}");
    };
    app.handoffs_paused.store(paused, Ordering::Relaxed);
    tracing::info!(target: "handoff", "handoffs {}", if paused { "paused" } else { "resumed" });
    ok(json!({"paused": paused}))
}

// ------------------------------------------------------------ Claude Code + creative apps

/// Ask Claude Code for its status and MCP health every 45 seconds (it starts
/// each MCP server to check it, so not more often).
fn spawn_claude_watch(app: Shared) {
    tokio::spawn(async move {
        loop {
            refresh_claude(&app).await;
            tokio::time::sleep(Duration::from_secs(45)).await;
        }
    });
}

/// Recheck the creative apps right away (after a connect or a refresh).
async fn recheck_apps(app: &App) {
    let servers = app.claude.lock().unwrap().servers.clone();
    let ac = app.cfg().apps.clone();
    if let Ok(st) = tokio::task::spawn_blocking(move || apps::check(&ac, &servers, &apps::running_processes())).await {
        *app.apps.lock().unwrap() = st;
    }
}

async fn refresh_claude(app: &App) {
    let cmd = app.cfg.read().unwrap().claude.command.clone();
    let fresh = claude::status(&cmd).await;
    let before = std::mem::replace(&mut *app.claude.lock().unwrap(), fresh.clone());
    if before.installed != fresh.installed || before.signed_in != fresh.signed_in {
        match (fresh.installed, fresh.signed_in) {
            (false, _) => tracing::warn!(target: "claude", "Claude Code isn't installed on this PC"),
            (true, Some(false)) => tracing::warn!(target: "claude", "Claude Code is installed but not signed in"),
            (true, _) => tracing::info!(target: "claude", "Claude Code {} is ready ({})",
                fresh.version.as_deref().unwrap_or(""), fresh.auth_method.as_deref().unwrap_or("signed in")),
        }
    }
    for s in &fresh.servers {
        let old = before.servers.iter().find(|o| o.name == s.name).map(|o| o.health.as_str());
        if old != Some(s.health.as_str()) {
            tracing::info!(target: "claude", "Claude Code connection \"{}\": {}", s.name, s.detail);
        }
    }
}

/// Every 3 seconds: which creative apps are open, and pause this PC's model
/// while any of them is (so they get the whole GPU).
fn spawn_apps_watch(app: Shared) {
    tokio::spawn(async move {
        let mut was_running: Vec<&'static str> = Vec::new();
        loop {
            let cfg = app.cfg();
            let servers = app.claude.lock().unwrap().servers.clone();
            let apps_cfg = cfg.apps.clone();
            let status = tokio::task::spawn_blocking(move || {
                let procs = apps::running_processes();
                apps::check(&apps_cfg, &servers, &procs)
            })
            .await
            .unwrap_or_default();
            let running: Vec<&'static str> = status.iter().filter(|a| a.running).map(|a| a.name).collect();
            for a in &status {
                let was = was_running.contains(&a.name);
                if a.running && !was {
                    tracing::info!(target: "apps", "{} opened. {}", a.name, a.summary);
                } else if !a.running && was {
                    tracing::info!(target: "apps", "{} closed", a.name);
                }
            }
            was_running = running.clone();
            *app.apps.lock().unwrap() = status;

            if cfg.local_model.pause_for_apps && cfg.nodes.local.url.is_empty() {
                let paused = !app.paused_for.lock().unwrap().is_empty();
                if !running.is_empty() && !paused && app.model.is_active() {
                    tracing::info!(target: "router", "{} is open, so this PC's model is paused to give it the whole GPU",
                        running.join(" and "));
                    app.model.stop().await;
                    *app.paused_for.lock().unwrap() = running.clone();
                } else if running.is_empty() && paused {
                    app.paused_for.lock().unwrap().clear();
                    tracing::info!(target: "router", "creative apps closed; starting this PC's model again");
                    if let Err(e) = app.model.start(&cfg.local_model).await {
                        tracing::warn!(target: "router", "couldn't restart this PC's model: {e}");
                    }
                } else if paused {
                    *app.paused_for.lock().unwrap() = running.clone();
                }
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });
}

async fn claude_info(State(app): State<Shared>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers, None) {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong router api key");
    }
    let c = app.claude.lock().unwrap().clone();
    ok(json!({"claude": c, "apps": *app.apps.lock().unwrap(), "unity_cli": apps::unity_cli_installed(),
              "desktop_servers": claude::desktop_servers().into_iter().map(|(k, _)| k).collect::<Vec<_>>()}))
}

async fn claude_refresh(State(app): State<Shared>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Err(r) = control_ok(&app, &headers, peer) {
        return r;
    }
    refresh_claude(&app).await;
    recheck_apps(&app).await;
    claude_info(State(app), headers).await
}

/// Body: {"app": "unreal"|"unity"|"blender"}. Adds the app's MCP server to
/// Claude Code (user scope), copying it from the Claude desktop app when
/// that already has one.
async fn app_connect(State(app): State<Shared>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>, body: Bytes) -> Response {
    if let Err(r) = control_ok(&app, &headers, peer) {
        return r;
    }
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let Some(which) = v["app"].as_str().and_then(apps::App::from_id) else {
        return error(StatusCode::BAD_REQUEST, "app must be unreal, unity or blender");
    };
    let cfg = app.cfg();
    let Some(cl) = claude::find(&cfg.claude.command) else {
        return error(StatusCode::BAD_REQUEST, "Claude Code isn't installed. Install it from claude.com/claude-code, sign in once, then press Connect again.");
    };
    let result = connect_app(&app, &cl, which, &cfg).await;
    refresh_claude(&app).await;
    recheck_apps(&app).await;
    match result {
        Ok(msg) => {
            tracing::info!(target: "apps", "{}: {msg}", which.name());
            ok(json!({"ok": true, "message": msg}))
        }
        Err(e) => {
            tracing::warn!(target: "apps", "{}: {e}", which.name());
            ok(json!({"ok": false, "message": e}))
        }
    }
}

async fn connect_app(app: &App, cl: &std::path::Path, which: apps::App, cfg: &Config) -> Result<String, String> {
    let id = which.id();
    // 1. The Claude desktop app may already have it set up: copy that over.
    let from_desktop = claude::desktop_servers().into_iter().find(|(name, v)| {
        name.to_lowercase().contains(id) || v.to_string().to_lowercase().contains(id)
    });
    if let Some((name, mut v)) = from_desktop {
        if v.get("url").is_some() && v.get("type").is_none() {
            v["type"] = json!("http");
        }
        claude::add_server(cl, id, &v).await?;
        return Ok(format!("Copied the \"{name}\" connection from the Claude desktop app into Claude Code."));
    }
    // 2. Otherwise set it up from what we know about each app.
    match which {
        apps::App::Unreal => {
            let base = cfg.apps.unreal_url.trim_end_matches('/').trim_end_matches("/mcp").to_string();
            let mut url = cfg.apps.unreal_url.clone();
            for cand in [cfg.apps.unreal_url.clone(), format!("{base}/mcp"), format!("{base}/"), base.clone()] {
                if claude::probe_http_mcp(&app.http, &cand).await {
                    url = cand;
                    break;
                }
            }
            claude::add_server(cl, "unreal", &json!({"type": "http", "url": url})).await?;
            Ok(format!("Added Unreal ({url}) to Claude Code. It shows as connected while the editor is open with its MCP server running."))
        }
        apps::App::Unity => {
            if !apps::unity_cli_installed() {
                return Err("Unity connects through the Unity command-line tool, which isn't installed. In PowerShell run:  $env:UNITY_CLI_CHANNEL='beta'; irm https://public-cdn.cloud.unity3d.com/hub/prod/cli/install.ps1 | iex   then press Connect again.".into());
            }
            claude::add_server(cl, "unity", &json!({"type": "stdio", "command": "unity", "args": ["mcp"]})).await?;
            Ok("Added Unity to Claude Code (through the Unity command-line tool). Open a project with the Pipeline package to connect.".into())
        }
        apps::App::Blender => Err("Blender connects through its MCP add-on plus a small bridge program that isn't set up yet. Add Blender in the Claude desktop app (Settings > Connectors) or follow blender.org/lab/mcp-server, then press Connect again and it will be copied into Claude Code.".into()),
    }
}

/// Body: {"project": "<folder with a .uproject>"}
async fn unreal_setup(State(app): State<Shared>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>, body: Bytes) -> Response {
    if let Err(r) = control_ok(&app, &headers, peer) {
        return r;
    }
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let Some(dir) = v["project"].as_str().map(PathBuf::from) else {
        return error(StatusCode::BAD_REQUEST, "project folder missing");
    };
    match blueprint::setup_unreal_project(&dir) {
        Ok(done) => {
            for d in &done {
                tracing::info!(target: "apps", "Unreal project {}: {d}", dir.display());
            }
            ok(json!({"ok": true, "done": done}))
        }
        Err(e) => ok(json!({"ok": false, "message": e})),
    }
}

async fn blueprint_get(State(app): State<Shared>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers, None) {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong router api key");
    }
    let job = app.planner.snapshot();
    let text = job.file.as_ref().and_then(|f| std::fs::read_to_string(f).ok());
    let cfg = app.cfg();
    ok(json!({"job": job, "blueprint": text, "last_project": cfg.claude.last_project, "model": cfg.claude.blueprint_model}))
}

/// Body: {"project": "...", "idea": "...", "model": "" | "opus"}
async fn blueprint_start(State(app): State<Shared>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>, body: Bytes) -> Response {
    if let Err(r) = control_ok(&app, &headers, peer) {
        return r;
    }
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let project = v["project"].as_str().unwrap_or("").trim().to_string();
    let idea = v["idea"].as_str().unwrap_or("").to_string();
    let model = v["model"].as_str().unwrap_or("").to_string();
    let cfg = app.cfg();
    let Some(cl) = claude::find(&cfg.claude.command) else {
        return error(StatusCode::BAD_REQUEST, "Claude Code isn't installed on this PC.");
    };
    let router_url = format!("http://{}/v1", cfg.router.listen.first().cloned().unwrap_or_default());
    if let Err(e) = app.planner.start(cl, PathBuf::from(&project), idea, model.clone(), router_url) {
        return error(StatusCode::BAD_REQUEST, e);
    }
    {
        let mut c = app.cfg.write().unwrap();
        c.claude.last_project = project;
        c.claude.blueprint_model = model;
    }
    let _ = app.cfg().save(&app.cfg_path);
    ok(json!({"started": true}))
}

async fn blueprint_cancel(State(app): State<Shared>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Err(r) = control_ok(&app, &headers, peer) {
        return r;
    }
    app.planner.cancel();
    ok(json!({"cancelled": true}))
}

/// Windows folder picker, shown on top of the app window.
async fn pick_folder(State(app): State<Shared>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Err(r) = control_ok(&app, &headers, peer) {
        return r;
    }
    if !cfg!(windows) {
        return error(StatusCode::BAD_REQUEST, "type the folder path instead");
    }
    let script = "Add-Type -AssemblyName System.Windows.Forms; \
        $f = New-Object System.Windows.Forms.Form -Property @{TopMost=$true; ShowInTaskbar=$false}; \
        $d = New-Object System.Windows.Forms.FolderBrowserDialog; \
        $d.Description = 'Choose your project folder'; $d.ShowNewFolderButton = $true; \
        if ($d.ShowDialog($f) -eq 'OK') { [Console]::Out.Write($d.SelectedPath) }";
    let out = tokio::task::spawn_blocking(move || {
        let mut c = std::process::Command::new("powershell");
        c.args(["-NoProfile", "-STA", "-Command", script]);
        sys::quiet(&mut c).output()
    })
    .await;
    match out {
        Ok(Ok(o)) => ok(json!({"path": String::from_utf8_lossy(&o.stdout).trim()})),
        _ => error(StatusCode::INTERNAL_SERVER_ERROR, "couldn't open the folder picker"),
    }
}
