//! Runs this PC's model: starts llama-server, watches it load, streams its
//! output into the app's verbose log, and notices if it crashes.

use crate::config::LocalModelCfg;
use crate::sys;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum ModelState {
    /// No model chosen yet.
    NotSetUp,
    Stopped,
    /// Process started, model loading into the GPU.
    Loading,
    Running,
    /// Exited on its own; detail says why.
    Crashed(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelStatus {
    #[serde(flatten)]
    pub state: ModelState,
    pub model_name: Option<String>,
    pub llama_server: Option<String>,
    pub port: u16,
    pub uptime_secs: Option<u64>,
    pub load_secs: Option<u64>,
}

struct Inner {
    state: ModelState,
    child: Option<Child>,
    /// Bumped on every start/stop so a stale watcher can't overwrite newer state.
    generation: u64,
    started: Option<Instant>,
    loaded_after: Option<Duration>,
    last_lines: Vec<String>,
    model: Option<String>,
    exe: Option<String>,
    port: u16,
}

#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<Mutex<Inner>>,
}

impl Supervisor {
    pub fn new(cfg: &LocalModelCfg) -> Self {
        let state = if cfg.model.is_empty() { ModelState::NotSetUp } else { ModelState::Stopped };
        Self {
            inner: Arc::new(Mutex::new(Inner {
                state,
                child: None,
                generation: 0,
                started: None,
                loaded_after: None,
                last_lines: Vec::new(),
                model: None,
                exe: None,
                port: cfg.port,
            })),
        }
    }

    pub fn status(&self) -> ModelStatus {
        let i = self.inner.lock().unwrap();
        ModelStatus {
            state: i.state.clone(),
            model_name: i.model.as_deref().map(|m| crate::nodes::friendly_model_name(m)),
            llama_server: i.exe.clone(),
            port: i.port,
            uptime_secs: (i.state == ModelState::Running)
                .then(|| i.started.map(|s| s.elapsed().as_secs()))
                .flatten(),
            load_secs: i.loaded_after.map(|d| d.as_secs()),
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self.inner.lock().unwrap().state, ModelState::Loading | ModelState::Running)
    }

    /// Start llama-server with the chosen model. Returns a plain-English error
    /// when something needed is missing.
    pub async fn start(&self, cfg: &LocalModelCfg) -> Result<(), String> {
        self.stop().await;
        if cfg.model.trim().is_empty() {
            self.set(ModelState::NotSetUp);
            return Err("Choose a model first.".into());
        }
        let model = PathBuf::from(cfg.model.trim());
        if !model.is_file() {
            return Err(format!("The model file isn't there any more: {}", model.display()));
        }
        let exe = resolve_llama_server(&cfg.llama_server).ok_or_else(|| {
            "llama-server wasn't found. Download a Windows CUDA build from \
             github.com/ggml-org/llama.cpp/releases and unzip it to C:\\llama."
                .to_string()
        })?;

        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("-m")
            .arg(&model)
            .args(["-ngl", &cfg.gpu_layers.to_string()])
            .args(["-c", &cfg.context.to_string()])
            .arg("--jinja")
            .args(["--host", "127.0.0.1", "--port", &cfg.port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = exe.parent() {
            cmd.current_dir(dir); // so it finds its own CUDA DLLs
        }
        sys::quiet(&mut cmd);
        let mut cmd = tokio::process::Command::from(cmd);
        cmd.kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| format!("Couldn't start llama-server: {e}"))?;

        tracing::info!(target: "router", "starting this PC's model: {} ({}k context, port {})",
            crate::nodes::friendly_model_name(&model.display().to_string()), cfg.context / 1024, cfg.port);

        let generation = {
            let mut i = self.inner.lock().unwrap();
            i.generation += 1;
            i.state = ModelState::Loading;
            i.started = Some(Instant::now());
            i.loaded_after = None;
            i.last_lines.clear();
            i.model = Some(model.display().to_string());
            i.exe = Some(exe.display().to_string());
            i.port = cfg.port;
            i.generation
        };

        for pipe in [child.stdout.take().map(Pipe::Out), child.stderr.take().map(Pipe::Err)].into_iter().flatten() {
            let me = self.clone();
            tokio::spawn(async move { me.pump(pipe).await });
        }
        self.inner.lock().unwrap().child = Some(child);

        // Watch it load, then watch for it exiting on its own.
        let me = self.clone();
        let port = cfg.port;
        tokio::spawn(async move { me.watch(generation, port).await });
        Ok(())
    }

    pub async fn stop(&self) {
        let child = {
            let mut i = self.inner.lock().unwrap();
            i.generation += 1;
            if matches!(i.state, ModelState::Loading | ModelState::Running) {
                i.state = ModelState::Stopped;
            }
            i.started = None;
            i.child.take()
        };
        if let Some(mut c) = child {
            tracing::info!(target: "router", "stopping this PC's model");
            let _ = c.kill().await;
        }
    }

    /// Forget the chosen model (state shown as "not set up").
    pub fn mark_not_set_up(&self) {
        self.set(ModelState::NotSetUp);
    }

    fn set(&self, s: ModelState) {
        self.inner.lock().unwrap().state = s;
    }

    async fn pump(&self, pipe: Pipe) {
        match pipe {
            Pipe::Out(p) => self.read_lines(BufReader::new(p)).await,
            Pipe::Err(p) => self.read_lines(BufReader::new(p)).await,
        }
    }

    async fn read_lines<R: tokio::io::AsyncRead + Unpin>(&self, r: BufReader<R>) {
        let mut lines = r.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim_end().to_string();
            if line.is_empty() {
                continue;
            }
            {
                let mut i = self.inner.lock().unwrap();
                i.last_lines.push(line.clone());
                if i.last_lines.len() > 30 {
                    i.last_lines.remove(0);
                }
            }
            if is_noise(&line) {
                tracing::debug!(target: "model", "{line}");
            } else {
                tracing::info!(target: "model", "{line}");
            }
        }
    }

    async fn watch(&self, generation: u64, port: u16) {
        let http = reqwest::Client::new();
        let health = format!("http://127.0.0.1:{port}/health");
        loop {
            tokio::time::sleep(Duration::from_millis(700)).await;
            // Did it exit?
            let exited = {
                let mut i = self.inner.lock().unwrap();
                if i.generation != generation {
                    return; // stopped or restarted
                }
                match i.child.as_mut().map(|c| c.try_wait()) {
                    Some(Ok(Some(status))) => Some(status),
                    _ => None,
                }
            };
            if let Some(status) = exited {
                let mut i = self.inner.lock().unwrap();
                if i.generation != generation {
                    return;
                }
                let why = explain_exit(&i.last_lines, status.code());
                tracing::error!(target: "router", "this PC's model stopped unexpectedly: {why}");
                i.state = ModelState::Crashed(why);
                i.child = None;
                return;
            }
            // Loaded yet?
            let loading = self.inner.lock().unwrap().state == ModelState::Loading;
            if loading {
                let ok = http
                    .get(&health)
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false);
                if ok {
                    let mut i = self.inner.lock().unwrap();
                    if i.generation == generation {
                        i.state = ModelState::Running;
                        i.loaded_after = i.started.map(|s| s.elapsed());
                        tracing::info!(target: "router", "this PC's model is ready (loaded in {}s)",
                            i.loaded_after.map_or(0, |d| d.as_secs()));
                    }
                }
            }
        }
    }
}

enum Pipe {
    Out(tokio::process::ChildStdout),
    Err(tokio::process::ChildStderr),
}

pub fn resolve_llama_server(configured: &str) -> Option<PathBuf> {
    let c = configured.trim();
    if !c.is_empty() {
        let p = Path::new(c);
        return p.is_file().then(|| p.to_path_buf());
    }
    sys::find_llama_servers().into_iter().next()
}

/// llama-server prints a line per HTTP request and per slot update; keep
/// those out of the default view.
fn is_noise(line: &str) -> bool {
    let l = line.trim_start();
    l.starts_with("srv  log_server_r")
        || l.contains("GET /health")
        || l.contains("GET /props")
        || l.contains("GET /v1/models")
        || l.starts_with("slot update_slots")
}

/// Turn llama-server's last words into something a person can act on.
fn explain_exit(lines: &[String], code: Option<i32>) -> String {
    let all = lines.join("\n").to_lowercase();
    if all.contains("out of memory") || all.contains("cudamalloc failed") || all.contains("failed to allocate") {
        "the GPU ran out of memory. Pick a smaller model or lower the context size.".into()
    } else if all.contains("address already in use") || all.contains("couldn't bind") || all.contains("failed to bind") {
        "the port is already taken by another program (LM Studio or Ollama?). Close it and press Start again.".into()
    } else if all.contains("failed to load model") || all.contains("error loading model") {
        "the model file couldn't be loaded. It may be incomplete or need a newer llama-server.".into()
    } else {
        let last = lines.iter().rev().find(|l| !l.trim().is_empty()).cloned().unwrap_or_default();
        format!("exit code {}. Last message: {}", code.map_or("unknown".into(), |c| c.to_string()), last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_common_failures() {
        let oom = vec!["ggml_cuda: cudaMalloc failed: out of memory".to_string()];
        assert!(explain_exit(&oom, Some(1)).contains("ran out of memory"));
        let port = vec!["couldn't bind HTTP server socket".to_string()];
        assert!(explain_exit(&port, Some(1)).contains("port is already taken"));
        assert!(explain_exit(&["boom".into()], Some(3)).contains("exit code 3"));
    }
}
