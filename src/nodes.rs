//! Node registry: polls the Mac and jayspc so routing uses live state.

use crate::classify::{NodeView, Target};
use crate::config::NodeCfg;
use serde_json::Value;
use std::sync::{Arc, RwLock};
use std::time::Duration;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct NodeStatus {
    pub name: &'static str,
    pub url: String,
    pub online: bool,
    pub n_ctx: Option<usize>,
    pub model: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone)]
pub struct Registry {
    mac: Arc<RwLock<NodeStatus>>,
    jay: Arc<RwLock<NodeStatus>>,
}

impl Registry {
    pub fn new(mac: &NodeCfg, jay: &NodeCfg) -> Self {
        let mk = |name, n: &NodeCfg| {
            Arc::new(RwLock::new(NodeStatus { name, url: n.url.clone(), ..Default::default() }))
        };
        Self { mac: mk("mac", mac), jay: mk("jayspc", jay) }
    }

    fn slot(&self, t: Target) -> &Arc<RwLock<NodeStatus>> {
        match t {
            Target::Mac => &self.mac,
            Target::Jayspc => &self.jay,
        }
    }

    pub fn view(&self, t: Target) -> NodeView {
        let s = self.slot(t).read().unwrap();
        NodeView { online: s.online, n_ctx: s.n_ctx }
    }

    pub fn snapshot(&self) -> Vec<NodeStatus> {
        vec![self.mac.read().unwrap().clone(), self.jay.read().unwrap().clone()]
    }

    /// Mark a node offline right away when a proxied request can't reach it,
    /// instead of waiting for the next poll.
    pub fn mark_down(&self, t: Target, err: String) {
        let mut s = self.slot(t).write().unwrap();
        s.online = false;
        s.last_error = Some(err);
    }

    pub fn spawn_poller(&self, http: reqwest::Client, cfgs: [(Target, NodeCfg); 2], every: Duration) {
        for (t, cfg) in cfgs {
            let slot = self.slot(t).clone();
            let http = http.clone();
            tokio::spawn(async move {
                loop {
                    let fresh = probe(&http, &cfg).await;
                    {
                        let mut s = slot.write().unwrap();
                        if s.online != fresh.online {
                            tracing::info!(node = s.name, online = fresh.online, "node state changed");
                        }
                        s.online = fresh.online;
                        s.n_ctx = fresh.n_ctx;
                        s.model = fresh.model;
                        s.last_error = fresh.last_error;
                    }
                    tokio::time::sleep(every).await;
                }
            });
        }
    }
}

struct Probe {
    online: bool,
    n_ctx: Option<usize>,
    model: Option<String>,
    last_error: Option<String>,
}

/// llama-server exposes /health and /props at its root (the URL without /v1).
/// Other OpenAI-compatible servers are treated as online if /v1/models answers.
async fn probe(http: &reqwest::Client, cfg: &NodeCfg) -> Probe {
    let root = cfg.url.strip_suffix("/v1").unwrap_or(&cfg.url).to_string();
    let get = |url: String| {
        let mut r = http.get(url).timeout(Duration::from_secs(3));
        if let Some(k) = &cfg.api_key {
            r = r.bearer_auth(k);
        }
        r.send()
    };

    let models = get(format!("{}/models", cfg.url)).await;
    let (online, model, last_error) = match models {
        Ok(r) if r.status().is_success() => {
            let v: Value = r.json().await.unwrap_or(Value::Null);
            let id = v["data"][0]["id"].as_str().map(str::to_string);
            (true, id, None)
        }
        Ok(r) => (false, None, Some(format!("/models returned {}", r.status()))),
        Err(e) => (false, None, Some(short_err(&e))),
    };
    if !online {
        return Probe { online, n_ctx: None, model, last_error };
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
    Probe { online, n_ctx, model, last_error }
}

pub fn short_err(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".into()
    } else if e.is_connect() {
        "connection refused or host unreachable".into()
    } else {
        e.to_string()
    }
}
