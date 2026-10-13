//! Settings, stored as TOML. The app edits and saves this file itself, so a
//! non-technical user never has to open it.
//!
//! Two model machines: `local` (this PC's GPU) and `remote` (a big-memory
//! machine such as a Mac Studio). Claude is reached through a Claude Code
//! handoff. Older files that used `jayspc` / `mac` still load.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub router: RouterCfg,
    pub nodes: Nodes,
    pub local_model: LocalModelCfg,
    pub escalation: EscalationCfg,
    pub claude: ClaudeCfg,
    pub apps: AppsCfg,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            router: RouterCfg::default(),
            nodes: Nodes::default(),
            local_model: LocalModelCfg::default(),
            escalation: EscalationCfg::default(),
            claude: ClaudeCfg::default(),
            apps: AppsCfg::default(),
        }
    }
}

/// Claude Code, used as the architect (blueprints) and for rescues.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeCfg {
    /// Path to the claude command. Empty = find it automatically.
    pub command: String,
    /// Model for blueprints: "" (your Claude Code default) or e.g. "opus".
    pub blueprint_model: String,
    /// The last project folder planned, offered again next time.
    pub last_project: String,
}

/// Unreal, Unity and Blender: where their MCP servers listen.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppsCfg {
    /// Unreal's built-in ModelContextProtocol plugin (HTTP).
    pub unreal_url: String,
    /// Blender's MCP add-on (TCP).
    pub blender_port: u16,
}

impl Default for AppsCfg {
    fn default() -> Self {
        Self { unreal_url: "http://127.0.0.1:8000/mcp".into(), blender_port: 9876 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RouterCfg {
    /// Addresses to listen on, e.g. ["127.0.0.1:4000", "100.101.102.103:4000"].
    pub listen: Vec<String>,
    /// If set, clients must send `Authorization: Bearer <api_key>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// How often to check each machine, in seconds.
    pub poll_seconds: u64,
    /// Also write logs here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_file: Option<PathBuf>,
}

impl Default for RouterCfg {
    fn default() -> Self {
        Self { listen: vec!["127.0.0.1:4000".into()], api_key: None, poll_seconds: 3, log_file: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Nodes {
    #[serde(alias = "jayspc")]
    pub local: NodeCfg,
    #[serde(alias = "mac")]
    pub remote: NodeCfg,
}

impl Default for Nodes {
    fn default() -> Self {
        Self {
            local: NodeCfg { name: "This PC".into(), url: String::new(), ..Default::default() },
            remote: NodeCfg { name: "Mac Studio".into(), ..Default::default() },
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeCfg {
    /// Shown in the app.
    pub name: String,
    /// OpenAI-compatible base URL ending in /v1. Empty = not set up.
    /// For `local`, empty means "the model this app runs" (local_model.port).
    pub url: String,
    /// Sent to the machine as `Authorization: Bearer ...` (llama-server --api-key).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Model name to send upstream when the client asked for "auto".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// The model this PC serves, run by the app itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalModelCfg {
    /// GGUF file. Empty = not chosen yet.
    pub model: String,
    /// llama-server executable. Empty = find it automatically.
    pub llama_server: String,
    pub context: u32,
    pub port: u16,
    pub gpu_layers: u32,
    /// Start the model when the app starts.
    pub autostart: bool,
    /// Extra folders to look in for models.
    pub model_dirs: Vec<PathBuf>,
    /// Stop this model while Unreal, Unity or Blender is open, so they get
    /// the whole GPU; start it again when they close.
    pub pause_for_apps: bool,
}

impl Default for LocalModelCfg {
    fn default() -> Self {
        Self {
            model: String::new(),
            llama_server: String::new(),
            context: 16384,
            port: 8081,
            gpu_layers: 99,
            autostart: true,
            model_dirs: Vec::new(),
            pause_for_apps: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EscalationCfg {
    /// "handoff" (Claude Code, covered by a Pro or Max plan) or "off".
    pub mode: String,
    pub strikes_to_escalate: u32,
    /// Where handoff briefs are written. Defaults to ./handoffs next to this file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handoff_dir: Option<PathBuf>,
    /// Ask a local model to write the "what was tried" summary.
    pub summarize_with_local_model: bool,
}

impl Default for EscalationCfg {
    fn default() -> Self {
        Self {
            mode: "handoff".into(),
            strikes_to_escalate: 3,
            handoff_dir: None,
            summarize_with_local_model: true,
        }
    }
}

impl Config {
    /// Load `path`, creating it with defaults if it doesn't exist yet.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if !path.exists() {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).ok();
            }
            let mut cfg = Config::default();
            cfg.router.log_file = Some(PathBuf::from("router.log"));
            if let Ok(pc) = std::env::var("COMPUTERNAME") {
                cfg.nodes.local.name = title_case(&pc);
            }
            cfg.save(path)?;
        }
        Self::load(path)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading settings {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
        // Absolute, so the Claude command printed in a handoff works from any folder.
        let base = std::path::absolute(
            path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")),
        )
        .context("resolving settings folder")?;
        if cfg.escalation.handoff_dir.is_none() {
            cfg.escalation.handoff_dir = Some(base.join("handoffs"));
        }
        if let Some(f) = &cfg.router.log_file {
            if f.is_relative() {
                cfg.router.log_file = Some(base.join(f));
            }
        }
        cfg.normalize();
        match cfg.escalation.mode.as_str() {
            "handoff" | "off" => {}
            other => anyhow::bail!("escalation.mode must be \"handoff\" or \"off\", got {other:?}"),
        }
        Ok(cfg)
    }

    pub fn normalize(&mut self) {
        for n in [&mut self.nodes.local, &mut self.nodes.remote] {
            n.url = normalize_url(&n.url, None);
        }
        if self.nodes.local.name.is_empty() {
            self.nodes.local.name = "This PC".into();
        }
        if self.nodes.remote.name.is_empty() {
            self.nodes.remote.name = "Remote machine".into();
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let body = toml::to_string_pretty(self).context("writing settings")?;
        let text = format!(
            "# PoolParty Router settings. The app writes this file; you can also edit it\n\
             # by hand while the app is closed.\n\n{body}"
        );
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn handoff_dir(&self) -> PathBuf {
        self.escalation.handoff_dir.clone().unwrap_or_else(|| PathBuf::from("handoffs"))
    }

    /// The URL requests for this PC go to: the configured one, or the model the
    /// app runs itself.
    pub fn local_url(&self) -> String {
        if self.nodes.local.url.is_empty() {
            format!("http://127.0.0.1:{}/v1", self.local_model.port)
        } else {
            self.nodes.local.url.clone()
        }
    }

    pub fn remote_configured(&self) -> bool {
        !self.nodes.remote.url.is_empty()
    }
}

/// Turn what a person types ("mac-studio", "100.64.1.2:8080",
/// "http://box:8080") into a full base URL ending in /v1.
pub fn normalize_url(input: &str, default_port: Option<u16>) -> String {
    let s = input.trim().trim_end_matches('/');
    if s.is_empty() {
        return String::new();
    }
    let with_scheme = if s.contains("://") { s.to_string() } else { format!("http://{s}") };
    let (scheme, rest) = with_scheme.split_once("://").unwrap();
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let host = match (host.rsplit_once(':'), default_port) {
        (Some((_, p)), _) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => host.to_string(),
        (_, Some(port)) if scheme == "http" => format!("{host}:{port}"),
        _ => host.to_string(),
    };
    let path = if path.is_empty() { "/v1" } else { path };
    format!("{scheme}://{host}{path}")
}

fn title_case(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut c = lower.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
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
        assert_eq!(cfg.nodes.remote.url, "http://mac-studio:8080/v1");
        assert_eq!(cfg.handoff_dir(), dir.join("handoffs"));
    }

    #[test]
    fn old_names_still_load() {
        let dir = std::env::temp_dir().join("pp-cfg-old");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("router.toml");
        std::fs::write(&p, "[nodes.mac]\nurl = \"http://m:8080/v1/\"\n[nodes.jayspc]\nurl = \"http://127.0.0.1:8081/v1\"\n").unwrap();
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.nodes.remote.url, "http://m:8080/v1");
        assert_eq!(cfg.local_url(), "http://127.0.0.1:8081/v1");
    }

    #[test]
    fn creates_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("pp-cfg-new-{}", std::process::id()));
        let p = dir.join("router.toml");
        let _ = std::fs::remove_file(&p);
        let mut cfg = Config::load_or_create(&p).unwrap();
        assert!(!cfg.remote_configured());
        assert_eq!(cfg.local_url(), "http://127.0.0.1:8081/v1");
        cfg.nodes.remote.url = normalize_url("mac-studio", Some(8080));
        cfg.save(&p).unwrap();
        let again = Config::load(&p).unwrap();
        assert_eq!(again.nodes.remote.url, "http://mac-studio:8080/v1");
    }

    #[test]
    fn urls_people_type() {
        assert_eq!(normalize_url("mac-studio", Some(8080)), "http://mac-studio:8080/v1");
        assert_eq!(normalize_url("100.64.1.2:9000", Some(8080)), "http://100.64.1.2:9000/v1");
        assert_eq!(normalize_url("http://box:8080/v1/", None), "http://box:8080/v1");
        assert_eq!(normalize_url("https://x.example.com/api/v1", Some(8080)), "https://x.example.com/api/v1");
        assert_eq!(normalize_url("  ", Some(8080)), "");
    }
}
