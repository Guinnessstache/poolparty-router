//! "Plan with Claude": Claude Code studies a project (read-only) and writes a
//! build blueprint, which the coding agent on the remote machine then follows.

use crate::claude;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Idle,
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub state: JobState,
    pub project: String,
    pub idea: String,
    /// Recent steps, newest last ("Reading Source/Knight.cpp").
    pub steps: Vec<String>,
    pub seconds: u64,
    pub file: Option<String>,
    pub error: Option<String>,
    /// Paste this into the coding agent on the remote machine.
    pub build_prompt: Option<String>,
    pub cost_note: Option<String>,
}

impl Default for Job {
    fn default() -> Self {
        Job {
            state: JobState::Idle,
            project: String::new(),
            idea: String::new(),
            steps: Vec::new(),
            seconds: 0,
            file: None,
            error: None,
            build_prompt: None,
            cost_note: None,
        }
    }
}

#[derive(Clone, Default)]
pub struct Planner {
    job: Arc<Mutex<Job>>,
    started: Arc<Mutex<Option<Instant>>>,
    cancel: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}

const PROMPT: &str = r#"You are the lead architect for this project. A coding agent running on a
local model will build what you plan, one step at a time, so your plan has to
be concrete enough for a less capable model to follow without guessing.

Study the project in the current folder first (read only: do not change any
files). Then write a build blueprint for this request:

<request>
{IDEA}
</request>

Reply with ONLY the blueprint, in Markdown, with these sections:

# Blueprint: <short title>
## Goal
What will exist when this is done, in two or three sentences.
## How it fits the project
The architecture: the main pieces, where each lives, and how they talk to each
other. Name real files, classes, assets and engine systems you found.
## Build steps
A numbered list. Each step is small enough to finish and check on its own, and
says exactly which files to create or change and what "done" looks like (the
command to build or test it, or what to see in the editor).
## Engine and tool notes
Anything specific to Unreal, Unity, Blender or the project's framework that a
coding agent is likely to get wrong. Leave this out if it doesn't apply.
## Risks and open questions
What could go wrong and what to decide before starting.
"#;

impl Planner {
    pub fn snapshot(&self) -> Job {
        let mut j = self.job.lock().unwrap().clone();
        if j.state == JobState::Running {
            if let Some(s) = *self.started.lock().unwrap() {
                j.seconds = s.elapsed().as_secs();
            }
        }
        j
    }

    pub fn cancel(&self) {
        if let Some(tx) = self.cancel.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }

    /// Start a blueprint job. Fails right away if one is already running or
    /// the folder doesn't exist.
    pub fn start(
        &self,
        claude_path: PathBuf,
        project: PathBuf,
        idea: String,
        model: String,
        router_url: String,
    ) -> Result<(), String> {
        if self.job.lock().unwrap().state == JobState::Running {
            return Err("A blueprint is already being written.".into());
        }
        if !project.is_dir() {
            return Err(format!("That folder doesn't exist: {}", project.display()));
        }
        if idea.trim().len() < 10 {
            return Err("Describe what you want built in a sentence or two.".into());
        }
        *self.job.lock().unwrap() = Job {
            state: JobState::Running,
            project: project.display().to_string(),
            idea: idea.clone(),
            steps: vec!["Starting Claude Code".into()],
            ..Default::default()
        };
        *self.started.lock().unwrap() = Some(Instant::now());
        let (tx, rx) = tokio::sync::oneshot::channel();
        *self.cancel.lock().unwrap() = Some(tx);
        let me = self.clone();
        tokio::spawn(async move {
            let result = me.run(&claude_path, &project, &idea, &model, &router_url, rx).await;
            let mut j = me.job.lock().unwrap();
            j.seconds = me.started.lock().unwrap().map_or(0, |s| s.elapsed().as_secs());
            match result {
                Ok(()) => {}
                Err(e) if e == "cancelled" => {
                    j.state = JobState::Cancelled;
                    j.steps.push("Cancelled".into());
                }
                Err(e) => {
                    tracing::warn!(target: "claude", "blueprint failed: {e}");
                    j.state = JobState::Failed;
                    j.error = Some(e);
                }
            }
        });
        Ok(())
    }

    fn step(&self, s: String) {
        tracing::info!(target: "claude", "{s}");
        let mut j = self.job.lock().unwrap();
        j.steps.push(s);
        if j.steps.len() > 40 {
            j.steps.remove(0);
        }
    }

    async fn run(
        &self,
        claude_path: &Path,
        project: &Path,
        idea: &str,
        model: &str,
        router_url: &str,
        mut cancel: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<(), String> {
        let prompt = PROMPT.replace("{IDEA}", idea.trim());
        let prompt_file = std::env::temp_dir().join(format!("poolparty-blueprint-{}.txt", std::process::id()));
        std::fs::write(&prompt_file, &prompt).map_err(|e| e.to_string())?;

        let mut args: Vec<String> = [
            "-p",
            "Write the blueprint described above.",
            "--output-format",
            "stream-json",
            "--verbose",
            "--tools",
            "Read,Glob,Grep,WebSearch,WebFetch",
            "--disallowedTools",
            "mcp__*",
            "--max-turns",
            "60",
            "--no-session-persistence",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if !model.trim().is_empty() {
            args.push("--model".into());
            args.push(model.trim().into());
        }

        let mut cmd = claude::command(claude_path, &args, Some(&prompt_file));
        cmd.current_dir(project).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut cmd = tokio::process::Command::from(cmd);
        cmd.kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| format!("couldn't start Claude Code: {e}"))?;
        self.step(format!("Claude Code is studying {}", project.display()));

        let stdout = child.stdout.take().ok_or("no output from Claude Code")?;
        let stderr = child.stderr.take();
        let err_tail = Arc::new(Mutex::new(String::new()));
        if let Some(e) = stderr {
            let tail = err_tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(e).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let mut t = tail.lock().unwrap();
                    t.push_str(&l);
                    t.push('\n');
                    if t.len() > 4000 {
                        let cut = t.len() - 4000;
                        t.drain(..cut);
                    }
                }
            });
        }

        let mut lines = BufReader::new(stdout).lines();
        let mut result: Option<String> = None;
        let mut is_error = false;
        let mut cost: Option<f64> = None;
        loop {
            tokio::select! {
                _ = &mut cancel => {
                    let _ = child.kill().await;
                    let _ = std::fs::remove_file(&prompt_file);
                    return Err("cancelled".into());
                }
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else { break };
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                    match v["type"].as_str() {
                        Some("assistant") => {
                            for c in v["message"]["content"].as_array().into_iter().flatten() {
                                if c["type"] == "tool_use" {
                                    self.step(describe_tool(&c["name"], &c["input"], project));
                                }
                            }
                        }
                        Some("result") => {
                            is_error = v["is_error"].as_bool().unwrap_or(false);
                            result = v["result"].as_str().map(str::to_string);
                            cost = v["total_cost_usd"].as_f64();
                        }
                        _ => {}
                    }
                }
            }
        }
        let status = child.wait().await.map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&prompt_file);

        let text = match (result, is_error) {
            (Some(t), false) if t.trim().len() > 40 => t,
            (Some(t), _) => return Err(friendly_error(&t)),
            (None, _) => {
                let tail = err_tail.lock().unwrap().clone();
                let why = if tail.trim().is_empty() {
                    format!("Claude Code exited ({status}) without writing a blueprint.")
                } else {
                    tail
                };
                return Err(friendly_error(&why));
            }
        };
        let text = text.trim().trim_start_matches("```markdown").trim_start_matches("```").trim_end_matches("```").trim();

        let file = project.join("BLUEPRINT.md");
        if file.exists() {
            let _ = std::fs::copy(&file, project.join("BLUEPRINT.previous.md"));
        }
        std::fs::write(&file, format!("{text}\n")).map_err(|e| format!("couldn't save BLUEPRINT.md: {e}"))?;
        let wrote_cfg = ensure_opencode_config(project, router_url);

        let mut j = self.job.lock().unwrap();
        j.state = JobState::Done;
        j.file = Some(file.display().to_string());
        j.build_prompt = Some(build_prompt());
        j.cost_note = cost.filter(|c| *c > 0.0).map(|c| format!("about ${c:.2} of plan usage"));
        j.steps.push("Blueprint saved to BLUEPRINT.md".into());
        if wrote_cfg {
            j.steps.push("Pointed OpenCode in this project at the router (opencode.json)".into());
        }
        tracing::info!(target: "claude", "blueprint saved: {}", file.display());
        Ok(())
    }
}

/// What the coding agent on the remote machine should be told.
pub fn build_prompt() -> String {
    "Read BLUEPRINT.md in this project. It was written by the architect and is the plan to follow.\n\
     Work through the Build steps in order. For each step: make the change, run the build or test the \
     step names, fix anything that fails, then move on to the next step. Don't redesign the plan; if \
     a step can't work as written, stop and explain why."
        .into()
}

/// Add an opencode.json pointing at the router if the project has none, so
/// OpenCode in this folder uses the remote machine through the router.
fn ensure_opencode_config(project: &Path, router_url: &str) -> bool {
    let p = project.join("opencode.json");
    if p.exists() {
        return false;
    }
    let cfg = serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "provider": {"poolparty": {
            "npm": "@ai-sdk/openai-compatible", "name": "PoolParty",
            "options": {"baseURL": router_url},
            "models": {"auto": {"name": "PoolParty auto"}}
        }},
        "model": "poolparty/auto"
    });
    std::fs::write(&p, serde_json::to_string_pretty(&cfg).unwrap_or_default()).is_ok()
}

fn describe_tool(name: &serde_json::Value, input: &serde_json::Value, project: &Path) -> String {
    let rel = |p: &str| {
        Path::new(p)
            .strip_prefix(project)
            .map(|r| r.display().to_string())
            .unwrap_or_else(|_| p.to_string())
    };
    match name.as_str().unwrap_or("") {
        "Read" => format!("Reading {}", rel(input["file_path"].as_str().unwrap_or("a file"))),
        "Glob" => format!("Looking for {}", input["pattern"].as_str().unwrap_or("files")),
        "Grep" => format!("Searching for \"{}\"", input["pattern"].as_str().unwrap_or("")),
        "WebSearch" => format!("Searching the web: {}", input["query"].as_str().unwrap_or("")),
        "WebFetch" => format!("Reading {}", input["url"].as_str().unwrap_or("a web page")),
        other => format!("Using {other}"),
    }
}

fn friendly_error(raw: &str) -> String {
    let l = raw.to_lowercase();
    if l.contains("not logged in") || l.contains("please run /login") || l.contains("invalid api key") {
        "Claude Code isn't signed in. Open a terminal, run claude, and sign in with your Claude account.".into()
    } else if l.contains("usage limit") || l.contains("rate limit") || l.contains("limit reached") {
        "Your Claude plan's usage limit was reached. Try again after it resets.".into()
    } else if l.contains("max turns") || l.contains("max_turns") {
        "Claude Code ran out of steps before finishing. Try a narrower request.".into()
    } else {
        let last = raw.trim().lines().last().unwrap_or(raw).trim();
        format!("Claude Code stopped: {last}")
    }
}

// ------------------------------------------------------------ Unreal project setup

/// Make an Unreal project start its MCP server for Claude every time the
/// editor opens: turn on the ModelContextProtocol, EditorToolset and Terminal
/// plugins, and add Content/Python/init_unreal.py to start the server.
/// The .uproject for a path: the file itself, one in the folder, or one in a
/// folder directly inside it (in case the parent folder was picked).
pub fn find_uproject(path: &Path) -> Option<PathBuf> {
    if path.extension().is_some_and(|x| x == "uproject") && path.is_file() {
        return Some(path.to_path_buf());
    }
    let here = |d: &Path| {
        std::fs::read_dir(d).ok()?.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "uproject"))
    };
    if let Some(p) = here(path) {
        return Some(p);
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(path).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).filter_map(|d| here(&d)).collect();
    (found.len() == 1).then(|| found.remove(0))
}

pub fn setup_unreal_project(path: &Path) -> Result<Vec<String>, String> {
    let uproject = find_uproject(path).ok_or("Couldn't find a .uproject file in that folder.")?;
    let dir = uproject.parent().ok_or("bad project path")?.to_path_buf();
    let mut done = Vec::new();
    let text = std::fs::read_to_string(&uproject).map_err(|e| e.to_string())?;
    let mut v: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("couldn't read {}: {e}", uproject.display()))?;
    let plugins = v
        .as_object_mut()
        .ok_or("unexpected .uproject format")?
        .entry("Plugins")
        .or_insert_with(|| serde_json::json!([]));
    let list = plugins.as_array_mut().ok_or("unexpected Plugins list")?;
    let mut changed = false;
    // PythonScriptPlugin runs Content/Python/init_unreal.py at startup.
    for name in ["ModelContextProtocol", "EditorToolset", "Terminal", "PythonScriptPlugin"] {
        match list.iter_mut().find(|p| p["Name"] == name) {
            Some(p) if p["Enabled"] == true => {}
            Some(p) => {
                p["Enabled"] = serde_json::json!(true);
                changed = true;
            }
            None => {
                list.push(serde_json::json!({"Name": name, "Enabled": true}));
                changed = true;
            }
        }
    }
    if changed {
        let bak = uproject.with_extension("uproject.bak");
        if !bak.exists() {
            std::fs::copy(&uproject, &bak).map_err(|e| e.to_string())?;
        }
        std::fs::write(&uproject, serde_json::to_string_pretty(&v).unwrap()).map_err(|e| e.to_string())?;
        done.push(format!("Turned on Unreal's MCP, editor tools, terminal and Python plugins in {}", uproject.file_name().unwrap_or_default().to_string_lossy()));
    }
    let py_dir = dir.join("Content").join("Python");
    std::fs::create_dir_all(&py_dir).map_err(|e| e.to_string())?;
    let init = py_dir.join("init_unreal.py");
    let existing = std::fs::read_to_string(&init).unwrap_or_default();
    if !existing.contains("ModelContextProtocol.StartServer") {
        let sep = if existing.is_empty() { "" } else { "\n\n" };
        std::fs::write(&init, format!("{existing}{sep}{UNREAL_INIT}")).map_err(|e| e.to_string())?;
        done.push("Added Content/Python/init_unreal.py so the MCP server starts with the editor".into());
    }
    if done.is_empty() {
        done.push("This project was already set up".into());
    }
    Ok(done)
}

const UNREAL_INIT: &str = r#"# Start Unreal's MCP server so Claude Code can connect without a console command.
# Unreal runs Content/Python/init_unreal.py every time the editor opens.
# Added by PoolParty Router. Server: http://127.0.0.1:8000
import unreal

_ticks = 0
_handle = None

def _start_mcp(delta_seconds):
    global _ticks
    _ticks += 1
    if _ticks < 90:  # let the editor finish loading first
        return
    unreal.unregister_slate_post_tick_callback(_handle)
    unreal.SystemLibrary.execute_console_command(None, "ModelContextProtocol.StartServer")
    unreal.log("[PoolParty] ModelContextProtocol server started on port 8000")

_handle = unreal.register_slate_post_tick_callback(_start_mcp)
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_up_an_unreal_project_once() {
        let dir = std::env::temp_dir().join(format!("pp-ue-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Knight.uproject"), "\u{feff}{\"FileVersion\":3,\"Plugins\":[{\"Name\":\"Terminal\",\"Enabled\":false}]}").unwrap();
        let first = setup_unreal_project(&dir).unwrap();
        assert_eq!(first.len(), 2);
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("Knight.uproject")).unwrap()).unwrap();
        assert_eq!(v["Plugins"].as_array().unwrap().len(), 4);
        assert!(v["Plugins"].as_array().unwrap().iter().all(|p| p["Enabled"] == true));
        assert!(dir.join("Knight.uproject.bak").exists());
        let again = setup_unreal_project(&dir).unwrap();
        assert_eq!(again, vec!["This project was already set up".to_string()]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn friendly_errors() {
        assert!(friendly_error("Error: Not logged in · Please run /login").contains("isn't signed in"));
        assert!(friendly_error("Claude AI usage limit reached|1700000000").contains("usage limit"));
    }

    #[test]
    fn tool_steps_read_well() {
        let p = Path::new("/proj");
        let s = describe_tool(&serde_json::json!("Read"), &serde_json::json!({"file_path": "/proj/Source/Knight.cpp"}), p);
        assert_eq!(s, "Reading Source/Knight.cpp");
    }
}
