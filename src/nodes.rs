//! Machine registry: checks this PC's model and the remote machine every few
//! seconds so routing and the app's status lights use live state.

use crate::classify::{NodeView, Target};
use crate::config::{Config, NodeCfg};
use serde_json::Value;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct NodeStatus {
    pub id: &'static str,
    pub name: String,
    pub url: String,
    /// False for the remote machine until someone sets it up.
    pub configured: bool,
    pub online: bool,
    pub n_ctx: Option<usize>,
    pub model: Option<String>,
    /// Round trip of the last health check.
    pub latency_ms: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Clone)]
pub struct Registry {
    local: Arc<RwLock<NodeStatus>>,
    remote: Arc<RwLock<NodeStatus>>,
}

impl Registry {
    pub fn new() -> Self {
        let mk = |id| Arc::new(RwLock::new(NodeStatus { id, ..Default::default() }));
        Self { local: mk("local"), remote: mk("remote") }
    }

    fn slot(&self, t: Target) -> &Arc<RwLock<NodeStatus>> {
        match t {
            Target::Local => &self.local,
            Target::Remote => &self.remote,
        }
    }

    pub fn view(&self, t: Target) -> NodeView {
        let s = self.slot(t).read().unwrap();
        NodeView { online: s.online, n_ctx: s.n_ctx }
    }

    pub fn get(&self, t: Target) -> NodeStatus {
        self.slot(t).read().unwrap().clone()
    }

    pub fn snapshot(&self) -> Vec<NodeStatus> {
        vec![self.get(Target::Local), self.get(Target::Remote)]
    }

    /// Mark a machine offline right away when a request can't reach it,
    /// instead of waiting for the next check.
    pub fn mark_down(&self, t: Target, err: String) {
        let mut s = self.slot(t).write().unwrap();
        s.online = false;
        s.last_error = Some(err);
    }

    /// Check both machines now (also called after settings change).
    pub async fn refresh(&self, http: &reqwest::Client, cfg: &Config) {
        let local = NodeCfg { url: cfg.local_url(), ..cfg.nodes.local.clone() };
        let (a, b) = tokio::join!(
            self.check(http, Target::Local, &local, true),
            self.check(http, Target::Remote, &cfg.nodes.remote, cfg.remote_configured())
        );
        let _ = (a, b);
    }

    async fn check(&self, http: &reqwest::Client, t: Target, cfg: &NodeCfg, configured: bool) {
        let fresh = if configured {
            probe(http, cfg).await
        } else {
            Probe::default()
        };
        let mut s = self.slot(t).write().unwrap();
        if s.online != fresh.online && configured {
            if fresh.online {
                tracing::info!(target: "router", "{} is online ({})", cfg.name, cfg.url);
            } else {
                tracing::warn!(target: "router", "{} went offline: {}", cfg.name,
                    fresh.last_error.as_deref().unwrap_or("no answer"));
            }
        }
        *s = NodeStatus {
            id: s.id,
            name: cfg.name.clone(),
            url: cfg.url.clone(),
            configured,
            online: fresh.online,
            n_ctx: fresh.n_ctx,
            model: fresh.model,
            latency_ms: fresh.latency_ms,
            last_error: fresh.last_error,
        };
    }

    pub fn spawn_poller(&self, http: reqwest::Client, cfg: Arc<RwLock<Config>>) {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                let c = cfg.read().unwrap().clone();
                me.refresh(&http, &c).await;
                tokio::time::sleep(Duration::from_secs(c.router.poll_seconds.max(1))).await;
            }
        });
    }
}

#[derive(Default)]
struct Probe {
    online: bool,
    n_ctx: Option<usize>,
    model: Option<String>,
    latency_ms: Option<u64>,
    last_error: Option<String>,
}

/// llama-server exposes /props at its root (the URL without /v1). Other
/// OpenAI-compatible servers count as online if /v1/models answers.
async fn probe(http: &reqwest::Client, cfg: &NodeCfg) -> Probe {
    let root = cfg.url.strip_suffix("/v1").unwrap_or(&cfg.url).to_string();
    let get = |url: String| {
        let mut r = http.get(url).timeout(Duration::from_secs(3));
        if let Some(k) = &cfg.api_key {
            r = r.bearer_auth(k);
        }
        r.send()
    };

    let t0 = Instant::now();
    let models = get(format!("{}/models", cfg.url)).await;
    let latency_ms = Some(t0.elapsed().as_millis() as u64);
    let (online, model, last_error) = match models {
        Ok(r) if r.status().is_success() => {
            let v: Value = r.json().await.unwrap_or(Value::Null);
            let id = v["data"][0]["id"].as_str().map(friendly_model_name);
            (true, id, None)
        }
        Ok(r) if r.status().as_u16() == 401 => (false, None, Some("wrong or missing API key".into())),
        Ok(r) if r.status().as_u16() == 503 => (false, None, Some("still loading the model".into())),
        Ok(r) => (false, None, Some(format!("answered with error {}", r.status().as_u16()))),
        Err(e) => (false, None, Some(short_err(&e))),
    };
    if !online {
        return Probe { online, n_ctx: None, model, latency_ms: None, last_error };
    }

    let n_ctx = match get(format!("{root}/props")).await {
        Ok(r) if r.status().is_success() => {
            let v: Value = r.json().await.unwrap_or(Value::Null);
            v["default_generation_settings"]["n_ctx"]
                .as_u64()
                .or_else(|| v["n_ctx"].as_u64())
                .map(|n| n as usize)
        }
        _ => None,
    };
    Probe { online, n_ctx, model, latency_ms, last_error }
}

/// "C:\models\Qwen3.8-27B-UD-IQ2_S.gguf" -> "Qwen3.8-27B-UD-IQ2_S"
pub fn friendly_model_name(id: &str) -> String {
    let base = id.rsplit(['/', '\\']).next().unwrap_or(id);
    base.strip_suffix(".gguf").unwrap_or(base).to_string()
}

pub fn short_err(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "not answering (timed out)".into()
    } else if e.is_connect() {
        "can't connect (is it turned on and on the network?)".into()
    } else {
        e.to_string()
    }
}
