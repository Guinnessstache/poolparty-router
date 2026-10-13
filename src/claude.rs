//! Claude Code on this PC: is it installed and signed in, which MCP
//! connections does it have (and are they healthy), adding the creative
//! apps to it, and running it unattended to write blueprints.
//!
//! Everything goes through the `claude` command, which uses the person's
//! Claude plan. The router never calls Claude's API itself.

use crate::sys;
use base64::Engine;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Find the claude command: the configured path, then PATH, then the usual
/// install locations (native installer and npm).
pub fn find(configured: &str) -> Option<PathBuf> {
    let c = configured.trim();
    if !c.is_empty() {
        let p = PathBuf::from(c);
        return p.is_file().then_some(p);
    }
    let names: &[&str] = if cfg!(windows) { &["claude.exe", "claude.cmd"] } else { &["claude"] };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let h = sys::home();
    dirs.push(h.join(".local").join("bin"));
    dirs.push(h.join(".claude").join("local"));
    if let Some(a) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(a).join("npm"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    for d in dirs {
        for n in names {
            let p = d.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Build a command that runs `claude <args>`. On Windows the npm install is a
/// .cmd script, which can't take arbitrary arguments safely, so those runs go
/// through PowerShell with every argument single-quoted. `stdin_file`, if
/// given, is piped in as the prompt.
pub fn command(claude: &Path, args: &[String], stdin_file: Option<&Path>) -> Command {
    // npm's claude.cmd just starts the real program; run that directly so
    // arguments (like JSON) arrive exactly as written.
    if let Some((prog, pre)) = npm_target(claude) {
        let mut c = Command::new(prog);
        c.args(pre).args(args);
        match stdin_file.and_then(|f| std::fs::File::open(f).ok()) {
            Some(f) => c.stdin(Stdio::from(f)),
            None => c.stdin(Stdio::null()),
        };
        sys::quiet(&mut c);
        return c;
    }
    let script_like = claude
        .extension()
        .map(|e| matches!(e.to_string_lossy().to_lowercase().as_str(), "cmd" | "bat" | "ps1"))
        .unwrap_or(false);
    if cfg!(windows) && script_like {
        let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
        // Windows PowerShell 5.1 drops embedded double quotes when it passes
        // arguments to programs, so escape them the way the program expects.
        let qa = |s: &str| q(&msvc_escape_quotes(s));
        let mut script = String::from(
            "$ErrorActionPreference='Continue'; $ProgressPreference='SilentlyContinue'; $u=New-Object Text.UTF8Encoding $false; \
             [Console]::OutputEncoding=$u; $OutputEncoding=$u; ",
        );
        if let Some(f) = stdin_file {
            script.push_str(&format!("Get-Content -Raw -Encoding UTF8 {} | ", q(&f.display().to_string())));
        }
        script.push_str(&format!("& {}", q(&claude.display().to_string())));
        for a in args {
            script.push(' ');
            script.push_str(&qa(a));
        }
        script.push_str("; exit $LASTEXITCODE");
        let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let enc = base64::engine::general_purpose::STANDARD.encode(utf16);
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &enc]);
        sys::quiet(&mut c);
        c.stdin(Stdio::null());
        return c;
    }
    let mut c = Command::new(claude);
    c.args(args);
    match stdin_file.and_then(|f| std::fs::File::open(f).ok()) {
        Some(f) => {
            c.stdin(Stdio::from(f));
        }
        None => {
            c.stdin(Stdio::null());
        }
    }
    sys::quiet(&mut c);
    c
}

/// For npm's claude.cmd shim: the program it really runs (a bundled .exe, or
/// node with cli.js) plus any leading arguments.
fn npm_target(claude: &Path) -> Option<(PathBuf, Vec<String>)> {
    let ext = claude.extension()?.to_string_lossy().to_lowercase();
    if ext != "cmd" && ext != "bat" && ext != "ps1" {
        return None;
    }
    let pkg = claude.parent()?.join("node_modules").join("@anthropic-ai").join("claude-code");
    for exe in ["bin/claude.exe", "claude.exe", "bin/claude"] {
        let p = pkg.join(exe);
        if p.is_file() {
            return Some((p, Vec::new()));
        }
    }
    let cli = pkg.join("cli.js");
    if cli.is_file() {
        let local_node = claude.parent()?.join("node.exe");
        let node = if local_node.is_file() { local_node } else { PathBuf::from("node") };
        return Some((node, vec![cli.display().to_string()]));
    }
    None
}

/// Escape double quotes for a Windows program's argument parser: backslashes
/// right before a quote are doubled, then the quote gets its own backslash.
fn msvc_escape_quotes(s: &str) -> String {
    let mut out = String::new();
    let mut slashes = 0;
    for c in s.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
                out.push('"');
                slashes = 0;
                continue;
            }
            _ => {}
        }
        if c != '\\' {
            out.push_str(&"\\".repeat(slashes));
            slashes = 0;
            out.push(c);
        }
    }
    out.push_str(&"\\".repeat(slashes));
    out
}

/// Drop PowerShell's serialized progress records (#< CLIXML ... </Objs>).
pub fn clean_output(t: &str) -> String {
    let mut out = String::new();
    let mut rest = t;
    while let Some(i) = rest.find("<Objs") {
        out.push_str(&rest[..i]);
        rest = match rest[i..].find("</Objs>") {
            Some(j) => &rest[i + j + 7..],
            None => "",
        };
    }
    out.push_str(rest);
    out.lines().filter(|l| !l.trim_start().starts_with("#< CLIXML")).collect::<Vec<_>>().join("\n")
}

/// Run `claude <args>` to completion with a time limit. Returns (exit ok, stdout).
pub async fn run(claude: &Path, args: &[&str], timeout: Duration) -> Result<(bool, String), String> {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let mut cmd = command(claude, &args, None);
    cmd.current_dir(sys::home()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut cmd = tokio::process::Command::from(cmd);
    cmd.kill_on_drop(true);
    let out = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| "Claude Code didn't answer in time".to_string())?
        .map_err(|e| format!("couldn't run Claude Code: {e}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        text.push('\n');
        text.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    Ok((out.status.success(), clean_output(&text)))
}

// ------------------------------------------------------------ status

#[derive(Debug, Clone, Default, Serialize)]
pub struct ClaudeStatus {
    pub installed: bool,
    pub path: Option<String>,
    pub version: Option<String>,
    pub signed_in: Option<bool>,
    /// How it's signed in: "claude.ai" means a Claude plan.
    pub auth_method: Option<String>,
    pub servers: Vec<McpServer>,
    /// When the MCP list was last checked (local time), if ever.
    pub checked: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct McpServer {
    pub name: String,
    pub target: String,
    /// "connected", "failed", "needs_auth", "pending" or "unknown".
    pub health: String,
    pub detail: String,
}

pub async fn status(configured: &str) -> ClaudeStatus {
    let Some(path) = find(configured) else {
        return ClaudeStatus { error: Some("Claude Code isn't installed on this PC.".into()), ..Default::default() };
    };
    let mut st = ClaudeStatus { installed: true, path: Some(path.display().to_string()), ..Default::default() };
    if let Ok((true, v)) = run(&path, &["--version"], Duration::from_secs(20)).await {
        st.version = v.split_whitespace().next().map(str::to_string);
    }
    match run(&path, &["auth", "status"], Duration::from_secs(20)).await {
        Ok((ok, text)) => {
            let j: serde_json::Value = serde_json::from_str(text.trim()).unwrap_or_default();
            st.auth_method = j["authMethod"].as_str().map(str::to_string);
            st.signed_in = Some(ok && st.auth_method.as_deref() != Some("none"));
        }
        Err(e) => st.error = Some(e),
    }
    match run(&path, &["mcp", "list"], Duration::from_secs(90)).await {
        Ok((_, text)) => st.servers = parse_mcp_list(&text),
        Err(e) => st.error = Some(e),
    }
    st.checked = Some(chrono::Local::now().format("%H:%M").to_string());
    st
}

/// Lines look like `unreal: http://127.0.0.1:8000/mcp (HTTP) - ✔ Connected`.
pub fn parse_mcp_list(text: &str) -> Vec<McpServer> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some((name, rest)) = line.split_once(": ") else { continue };
        if name.contains(' ') || name.is_empty() {
            continue; // "Checking MCP server health..." and similar
        }
        let Some(dash) = rest.rfind(" - ") else { continue };
        let (target, state) = (rest[..dash].trim(), rest[dash + 3..].trim());
        let lower = state.to_lowercase();
        let health = if lower.contains("connected") && !lower.contains("failed") {
            "connected"
        } else if lower.contains("failed") || state.starts_with('✘') || state.starts_with('✗') {
            "failed"
        } else if lower.contains("auth") {
            "needs_auth"
        } else if lower.contains("pending") || lower.contains("approval") {
            "pending"
        } else {
            "unknown"
        };
        let detail = state.trim_start_matches(|c: char| !c.is_alphanumeric()).to_string();
        out.push(McpServer { name: name.to_string(), target: target.to_string(), health: health.into(), detail });
    }
    out
}

// ------------------------------------------------------------ connecting apps

/// Add (or replace) a user-scope MCP server in Claude Code from a JSON entry.
pub async fn add_server(claude: &Path, name: &str, json: &serde_json::Value) -> Result<(), String> {
    // Replace any older entry with the same name so the new settings win.
    let _ = run(claude, &["mcp", "remove", "--scope", "user", name], Duration::from_secs(30)).await;
    let body = json.to_string();
    let (ok, text) =
        run(claude, &["mcp", "add-json", "--scope", "user", name, &body], Duration::from_secs(60)).await?;
    if ok {
        Ok(())
    } else {
        let msg: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        let tail = msg[msg.len().saturating_sub(3)..].join(" ");
        Err(if tail.is_empty() { "Claude Code couldn't add the connection.".into() } else { format!("Claude Code said: {tail}") })
    }
}

/// Where the Claude desktop app keeps its settings. The Microsoft Store
/// version's folder is redirected under LocalAppData\Packages.
pub fn desktop_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(a) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(a).join("Claude"));
    }
    if let Some(l) = std::env::var_os("LOCALAPPDATA") {
        if let Ok(rd) = std::fs::read_dir(PathBuf::from(l).join("Packages")) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_lowercase();
                if n.starts_with("claude") || n.contains("anthropic") {
                    dirs.push(e.path().join("LocalCache").join("Roaming").join("Claude"));
                }
            }
        }
    }
    dirs.push(sys::home().join("Library/Application Support/Claude"));
    dirs.into_iter().filter(|d| d.is_dir()).collect()
}

/// MCP servers the Claude desktop app has: entries in claude_desktop_config.json
/// plus installed desktop extensions (Settings > Extensions/Connectors).
/// Values stay on this PC: they're only copied into Claude Code's settings.
pub fn desktop_servers() -> Vec<(String, serde_json::Value)> {
    let mut out = Vec::new();
    for dir in desktop_dirs() {
        out.extend(desktop_servers_in(&dir));
    }
    out
}

fn desktop_servers_in(dir: &Path) -> Vec<(String, serde_json::Value)> {
    let mut out: Vec<(String, serde_json::Value)> = std::fs::read_to_string(dir.join("claude_desktop_config.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v["mcpServers"].as_object().cloned())
        .map(|m| m.into_iter().collect())
        .unwrap_or_default();
    out.extend(desktop_extensions(dir));
    out
}

/// Installed desktop extensions (.mcpb/.dxt), turned into stdio entries that
/// Claude Code can run: the manifest's mcp_config with its placeholders filled.
fn desktop_extensions(dir: &Path) -> Vec<(String, serde_json::Value)> {
    let Ok(rd) = std::fs::read_dir(dir.join("Claude Extensions")) else { return Vec::new() };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let ext_dir = e.path();
        let id = e.file_name().to_string_lossy().to_string();
        let Ok(text) = std::fs::read_to_string(ext_dir.join("manifest.json")) else { continue };
        let Ok(m) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let settings: serde_json::Value = std::fs::read_to_string(
            dir.join("Claude Extensions Settings").join(format!("{id}.json")),
        )
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
        if settings["isEnabled"].as_bool() == Some(false) {
            continue;
        }
        if let Some(entry) = extension_entry(&m, &ext_dir, &settings["userConfig"]) {
            let name = m["name"].as_str().or(m["display_name"].as_str()).unwrap_or(&id).to_string();
            out.push((format!("{name} ({id})"), entry));
        }
    }
    out
}

fn extension_entry(m: &serde_json::Value, ext_dir: &Path, user: &serde_json::Value) -> Option<serde_json::Value> {
    let cfg = &m["server"]["mcp_config"];
    let command = cfg["command"].as_str()?;
    let home = sys::home();
    let fill = |s: &str| -> String {
        let mut r = s
            .replace("${__dirname}", &ext_dir.display().to_string())
            .replace("${HOME}", &home.display().to_string())
            .replace("${DESKTOP}", &home.join("Desktop").display().to_string())
            .replace("${DOCUMENTS}", &home.join("Documents").display().to_string())
            .replace("${DOWNLOADS}", &home.join("Downloads").display().to_string())
            .replace("${pathSeparator}", std::path::MAIN_SEPARATOR_STR)
            .replace("${/}", std::path::MAIN_SEPARATOR_STR);
        // ${user_config.key}: the saved value, else the manifest's default.
        while let Some(start) = r.find("${user_config.") {
            let Some(len) = r[start..].find('}') else { break };
            let key = &r[start + 14..start + len];
            let val = user.get(key).cloned().unwrap_or_else(|| m["user_config"][key]["default"].clone());
            let v = match val {
                serde_json::Value::String(s) => s,
                serde_json::Value::Null => String::new(),
                other => other.to_string(),
            };
            r.replace_range(start..start + len + 1, &v);
        }
        r
    };
    let args: Vec<String> = cfg["args"].as_array().into_iter().flatten().filter_map(|a| a.as_str()).map(fill).collect();
    let env: serde_json::Map<String, serde_json::Value> = cfg["env"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), serde_json::Value::String(fill(s)))))
        .collect();
    let mut entry = serde_json::json!({"type": "stdio", "command": fill(command), "args": args});
    if !env.is_empty() {
        entry["env"] = serde_json::Value::Object(env);
    }
    Some(entry)
}

/// Ask an HTTP MCP endpoint to initialize; true if it answers like one.
pub async fn probe_http_mcp(http: &reqwest::Client, url: &str) -> bool {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "poolparty-router", "version": env!("CARGO_PKG_VERSION")}}
    });
    let r = http
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .json(&body)
        .timeout(Duration::from_secs(3))
        .send()
        .await;
    match r {
        Ok(r) if r.status().is_success() => {
            let t = r.text().await.unwrap_or_default();
            t.contains("jsonrpc") || t.contains("serverInfo")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mcp_list() {
        let out = "Checking MCP server health...\n\n\
            unreal: http://127.0.0.1:8000/mcp (HTTP) - ✔ Connected\n\
            blender: uvx blender-mcp - ✘ Failed to connect\n\
            notion: https://mcp.notion.com/mcp (HTTP) - ! Needs authentication\n\
            unity: unity mcp - ⏸ Pending approval (run `claude` to approve)\n";
        let s = parse_mcp_list(out);
        assert_eq!(s.len(), 4);
        assert_eq!(s[0].name, "unreal");
        assert_eq!(s[0].health, "connected");
        assert_eq!(s[0].target, "http://127.0.0.1:8000/mcp (HTTP)");
        assert_eq!(s[1].health, "failed");
        assert_eq!(s[2].health, "needs_auth");
        assert_eq!(s[3].health, "pending");
        assert_eq!(s[1].detail, "Failed to connect");
    }

    #[test]
    fn turns_desktop_extensions_into_entries() {
        let m: serde_json::Value = serde_json::from_str(r#"{"name":"blender-mcp","server":{"type":"python",
            "mcp_config":{"command":"uvx","args":["${__dirname}/server.py","--port","${user_config.port}"],
            "env":{"BLENDER_HOST":"${user_config.host}"}}},
            "user_config":{"port":{"default":9876},"host":{"default":"localhost"}}}"#).unwrap();
        let e = extension_entry(&m, Path::new("/ext/b"), &serde_json::json!({"host": "127.0.0.1"})).unwrap();
        assert_eq!(e["command"], "uvx");
        assert_eq!(e["args"], serde_json::json!(["/ext/b/server.py", "--port", "9876"]));
        assert_eq!(e["env"]["BLENDER_HOST"], "127.0.0.1");
    }

    #[test]
    fn escapes_quotes_for_windows_programs() {
        assert_eq!(msvc_escape_quotes(r#"{"a":"b"}"#), r#"{\"a\":\"b\"}"#);
        assert_eq!(msvc_escape_quotes(r#"{"p":"C:\\x\\"}"#), r#"{\"p\":\"C:\\x\\\\\"}"#);
        assert_eq!(msvc_escape_quotes(r"a\b"), r"a\b");
    }

    #[test]
    fn strips_powershell_progress() {
        let t = "#< CLIXML\n<Objs Version=\"1.1.0.1\"><Obj S=\"progress\">x</Obj></Objs>\nError: Invalid JSON";
        assert_eq!(clean_output(t).trim(), "Error: Invalid JSON");
    }

    #[test]
    fn powershell_wrapper_quotes_arguments() {
        // Only meaningful on Windows; elsewhere the plain command is used.
        let c = command(Path::new("/usr/bin/claude"), &["mcp".into(), "add-json".into(), "{\"a\":\"it's\"}".into()], None);
        if !cfg!(windows) {
            assert_eq!(c.get_program(), "/usr/bin/claude");
            assert_eq!(c.get_args().count(), 3);
        }
    }
}
