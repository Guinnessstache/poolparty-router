//! Unreal, Unity and Blender: which are open, whether their MCP server is
//! up, and whether Claude Code is connected to it.

use crate::claude::McpServer;
use crate::config::AppsCfg;
use crate::sys;
use serde::Serialize;
use std::net::{SocketAddr, TcpStream};
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum App {
    Unreal,
    Unity,
    Blender,
}

pub const ALL: [App; 3] = [App::Unreal, App::Unity, App::Blender];

impl App {
    pub fn id(self) -> &'static str {
        match self {
            App::Unreal => "unreal",
            App::Unity => "unity",
            App::Blender => "blender",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            App::Unreal => "Unreal Engine",
            App::Unity => "Unity",
            App::Blender => "Blender",
        }
    }
    pub fn from_id(s: &str) -> Option<App> {
        ALL.into_iter().find(|a| a.id() == s)
    }
    /// Process names (lowercase, without .exe).
    fn processes(self) -> &'static [&'static str] {
        match self {
            App::Unreal => &["unrealeditor", "ue4editor", "unrealeditor-cmd"],
            App::Unity => &["unity"],
            App::Blender => &["blender"],
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AppStatus {
    pub id: &'static str,
    pub name: &'static str,
    pub running: bool,
    /// The app's MCP server answers (None = can't tell from here).
    pub bridge: Option<bool>,
    /// Claude Code's own health check for this app's connection: "connected",
    /// "failed", "needs_auth", "pending", or None if it isn't added yet.
    pub claude: Option<String>,
    /// The Claude Code server name we matched, if any.
    pub claude_name: Option<String>,
    /// One plain-English sentence and a light colour for the app.
    pub summary: String,
    pub light: &'static str,
    /// What to do next, when something's missing.
    pub hint: Option<String>,
}

/// Lowercase process names currently running, without ".exe".
pub fn running_processes() -> Vec<String> {
    let out = if cfg!(windows) {
        sys::quiet(Command::new("tasklist").args(["/FO", "CSV", "/NH"])).output()
    } else {
        Command::new("ps").args(["-A", "-o", "comm="]).output()
    };
    let Ok(out) = out else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let first = if cfg!(windows) { l.split("\",").next()?.trim_matches('"') } else { l.trim() };
            let base = first.rsplit(['/', '\\']).next()?;
            let lower = base.to_lowercase();
            Some(lower.strip_suffix(".exe").unwrap_or(&lower).to_string())
        })
        .collect()
}

fn port_open(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

fn url_port(url: &str) -> Option<u16> {
    let after = url.split("://").nth(1)?;
    let host = after.split('/').next()?;
    host.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).or(Some(80))
}

/// Unity's MCP bridge goes through the Unity CLI: ready if it sees an Editor.
fn unity_bridge() -> Option<bool> {
    let out = sys::quiet(Command::new(if cfg!(windows) { "unity.exe" } else { "unity" }).args([
        "status",
        "--format",
        "json",
        "--no-banner",
    ]))
    .output()
    .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let inst = v["data"]["instances"].as_array()?;
    Some(inst.iter().any(|i| i["state"].as_str().map_or(true, |s| s == "ready")))
}

pub fn unity_cli_installed() -> bool {
    sys::quiet(Command::new(if cfg!(windows) { "unity.exe" } else { "unity" }).arg("--version"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Which Claude Code server belongs to which app: by name first, then by
/// what it runs (e.g. an http URL on Unreal's port).
pub fn match_server<'a>(app: App, servers: &'a [McpServer], cfg: &AppsCfg) -> Option<&'a McpServer> {
    let id = app.id();
    servers
        .iter()
        .find(|s| s.name.to_lowercase().contains(id))
        .or_else(|| {
            servers.iter().find(|s| {
                let t = s.target.to_lowercase();
                match app {
                    App::Unreal => url_port(&cfg.unreal_url).is_some_and(|p| t.contains(&format!(":{p}"))),
                    App::Unity => t.contains("unity mcp") || t.contains("unity.exe mcp"),
                    App::Blender => t.contains("blender"),
                }
            })
        })
}

pub fn check(cfg: &AppsCfg, servers: &[McpServer], processes: &[String]) -> Vec<AppStatus> {
    ALL.into_iter()
        .map(|app| {
            let running = app.processes().iter().any(|p| processes.iter().any(|r| r == p));
            let bridge = if !running {
                None
            } else {
                match app {
                    App::Unreal => url_port(&cfg.unreal_url).map(port_open),
                    App::Blender => Some(port_open(cfg.blender_port)),
                    App::Unity => unity_bridge(),
                }
            };
            let m = match_server(app, servers, cfg);
            let claude = m.map(|s| s.health.clone());
            let (summary, light, hint) = describe(app, running, bridge, claude.as_deref());
            AppStatus {
                id: app.id(),
                name: app.name(),
                running,
                bridge,
                claude,
                claude_name: m.map(|s| s.name.clone()),
                summary,
                light,
                hint,
            }
        })
        .collect()
}

fn describe(app: App, running: bool, bridge: Option<bool>, claude: Option<&str>) -> (String, &'static str, Option<String>) {
    let start_hint = match app {
        App::Unreal => "In Unreal, open the console (`) and run ModelContextProtocol.StartServer. Your Enable-ClaudeMCP.ps1 script makes a project do this on its own.",
        App::Blender => "In Blender: Edit > Preferences > Add-ons > MCP, then Start server (you can turn on auto-start there).",
        App::Unity => "Add the Unity Pipeline package to the project once: unity pipeline install.",
    };
    match (running, bridge, claude) {
        (true, Some(false), None) => (
            "Open, but its MCP server isn't running and it isn't connected to Claude Code yet".into(),
            "wait",
            Some(format!("{start_hint} Then press Connect.")),
        ),
        (_, _, None) => (
            "Not connected to Claude Code yet".into(),
            if running { "wait" } else { "off" },
            Some("Press Connect and the app adds it to Claude Code for you.".into()),
        ),
        (false, _, Some(_)) => ("Closed. Claude Code is set up for it".into(), "off", None),
        (true, Some(false), Some(_)) => ("Open, but its MCP server isn't running".into(), "wait", Some(start_hint.into())),
        (true, _, Some("connected")) => ("Open and connected to Claude Code".into(), "good", None),
        (true, _, Some("needs_auth")) => (
            "Open; Claude Code needs you to approve the connection".into(),
            "wait",
            Some("Run claude in a terminal and type /mcp to approve it.".into()),
        ),
        (true, _, Some("pending")) => (
            "Open; Claude Code is waiting for you to approve it".into(),
            "wait",
            Some("Run claude in a terminal once and approve the server.".into()),
        ),
        (true, _, Some(_)) => (
            "Open, but Claude Code can't reach it".into(),
            "bad",
            Some(format!("{start_hint} Then press Check again.")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srv(name: &str, target: &str, health: &str) -> McpServer {
        McpServer { name: name.into(), target: target.into(), health: health.into(), detail: String::new() }
    }

    #[test]
    fn matches_servers_by_name_or_target() {
        let cfg = AppsCfg::default();
        let s = vec![
            srv("ue-editor", "http://127.0.0.1:8000/mcp (HTTP)", "connected"),
            srv("Blender", "uvx blender-mcp", "failed"),
            srv("notion", "https://mcp.notion.com/mcp (HTTP)", "connected"),
        ];
        assert_eq!(match_server(App::Unreal, &s, &cfg).unwrap().name, "ue-editor");
        assert_eq!(match_server(App::Blender, &s, &cfg).unwrap().name, "Blender");
        assert!(match_server(App::Unity, &s, &cfg).is_none());
    }

    #[test]
    fn describes_states() {
        let cfg = AppsCfg::default();
        let s = vec![srv("unreal", "http://127.0.0.1:8000/mcp", "connected")];
        let st = check(&cfg, &s, &["explorer".into()]);
        let unreal = st.iter().find(|a| a.id == "unreal").unwrap();
        assert!(!unreal.running);
        assert_eq!(unreal.light, "off");
        let unity = st.iter().find(|a| a.id == "unity").unwrap();
        assert!(unity.summary.contains("Not connected"));
        let (sum, light, _) = describe(App::Blender, true, Some(true), Some("connected"));
        assert_eq!((sum.as_str(), light), ("Open and connected to Claude Code", "good"));
        let (_, light, hint) = describe(App::Unreal, true, Some(false), Some("connected"));
        assert_eq!(light, "wait");
        assert!(hint.unwrap().contains("StartServer"));
    }

    #[test]
    fn reads_ports_from_urls() {
        assert_eq!(url_port("http://127.0.0.1:8000/mcp"), Some(8000));
        assert_eq!(url_port("http://localhost/mcp"), Some(80));
    }
}
