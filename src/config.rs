//! Router configuration, loaded from a TOML file.
//!
//! The triangle is fixed: one `mac` node (big models), one `jayspc` node
//! (small, fast models) and Claude, reached through a Claude Code handoff.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub router: RouterCfg,
    pub nodes: Nodes,
    #[serde(default)]
    pub escalation: EscalationCfg,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterCfg {
    /// Addresses to listen on, e.g. ["127.0.0.1:4000", "100.101.102.103:4000"].
    #[serde(default = "default_listen")]
    pub listen: Vec<String>,
    /// If set, clients must send `Authorization: Bearer <api_key>`.
    #[serde(default)]
    pub api_key: Option<String>,
    /// How often to poll each node's health, in seconds.
    #[serde(default = "default_poll")]
    pub poll_seconds: u64,
    /// Append logs to this file as well as the console (useful when the router
    /// runs hidden at logon).
    #[serde(default)]
    pub log_file: Option<PathBuf>,
}

impl Default for RouterCfg {
    fn default() -> Self {
        Self { listen: default_listen(), api_key: None, poll_seconds: default_poll(), log_file: None }
    }
}

fn default_listen() -> Vec<String> {
    vec!["127.0.0.1:4000".into()]
}
fn default_poll() -> u64 {
    5
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Nodes {
    pub mac: NodeCfg,
    pub jayspc: NodeCfg,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeCfg {
    /// OpenAI-compatible base URL ending in /v1, e.g. "http://mac-studio:8080/v1".
    pub url: String,
    /// Sent to the node as `Authorization: Bearer ...` (llama-server --api-key).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Model name to send upstream when the client asked for "auto".
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EscalationCfg {
    /// "handoff" (Claude Code, covered by Pro/Max) or "off".
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_strikes")]
    pub strikes_to_escalate: u32,
    /// Where handoff files are written. Defaults to ./handoffs next to the config.
    #[serde(default)]
    pub handoff_dir: Option<PathBuf>,
    /// Ask the Mac's model to write the "what was tried" summary.
    #[serde(default = "default_true")]
    pub summarize_with_local_model: bool,
}

impl Default for EscalationCfg {
    fn default() -> Self {
        Self {
            mode: default_mode(),
            strikes_to_escalate: default_strikes(),
            handoff_dir: None,
            summarize_with_local_model: true,
        }
    }
}

fn default_mode() -> String {
    "handoff".into()
}
fn default_strikes() -> u32 {
    3
}
fn default_true() -> bool {
    true
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        // Absolute, so the Claude command printed in a handoff works from any folder.
        let base = std::path::absolute(path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")))
            .context("resolving config folder")?;
        let base = base.as_path();
        if cfg.escalation.handoff_dir.is_none() {
            cfg.escalation.handoff_dir = Some(base.join("handoffs"));
        }
        if let Some(f) = &cfg.router.log_file {
            if f.is_relative() {
                cfg.router.log_file = Some(base.join(f));
            }
        }
        for n in [&mut cfg.nodes.mac, &mut cfg.nodes.jayspc] {
            n.url = n.url.trim_end_matches('/').to_string();
        }
        match cfg.escalation.mode.as_str() {
            "handoff" | "off" => {}
            other => anyhow::bail!("escalation.mode must be \"handoff\" or \"off\", got {other:?}"),
        }
        Ok(cfg)
    }

    pub fn handoff_dir(&self) -> PathBuf {
        self.escalation.handoff_dir.clone().unwrap_or_else(|| PathBuf::from("handoffs"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_config() {
        let dir = std::env::temp_dir().join("pp-cfg-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("router.toml");
        std::fs::write(&p, include_str!("../router.example.toml")).unwrap();
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.escalation.strikes_to_escalate, 3);
        assert!(!cfg.nodes.mac.url.ends_with('/'));
        assert_eq!(cfg.handoff_dir(), dir.join("handoffs"));
    }
}
