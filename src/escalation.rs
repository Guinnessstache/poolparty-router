//! Per-session failure tracking.
//!
//! Coding agents resend the whole conversation on every turn, so the router
//! only looks at messages it hasn't seen yet. Strikes come from observable
//! failure (repeated build errors, malformed or looping tool calls), never
//! from the model saying it is unsure.

use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Instant;

/// Tool names whose output is command output (builds, tests, shells).
static COMMAND_TOOL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)bash|shell|exec|run|command|terminal|test|build|compile|cmd|powershell|pwsh").unwrap()
});

/// Lines that mean a command failed.
static ERROR_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?im)^.*(",
        r"\berror(\[[a-z0-9]+\])?:|\berror (cs|c|ts|lnk)\d+|",
        r"\bfailed\b|\bfailure\b|\bexception\b|traceback \(most recent call last\)|",
        r"panicked at|exit (code|status):? *[1-9]|command failed|^fatal:|",
        r"undefined reference|cannot find (symbol|module|type)|unresolved external",
        r").*$"
    ))
    .unwrap()
});

/// Output that means the thing being fixed now works.
static SUCCESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)build succeeded|compiled successfully|\b0 errors?\b|\b0 failed\b|",
        r"all tests passed|test result: ok|finished `(dev|release|test)`|",
        r"\b\d+ passed\b|exit (code|status):? *0\b"
    ))
    .unwrap()
});

/// Summary lines like "5 passed; 0 failed" contain the word "failed" but are fine.
static ZERO_FAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b0 (failed|failures?|errors?|error\(s\))(\W|$)").unwrap());

/// A line that's only an exit code ("Exit code 1:"), which agents like
/// OpenCode print above a failed command's output. It says nothing about
/// *what* failed: grep with no matches and a broken build both print it.
static BARE_EXIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\W*(process )?exit(ed)? (with )?(code|status):? *-?\d+\W*$").unwrap());
static PATHISH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\w.~:-]*[/\\][\w./\\:-]+").unwrap());
static DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());

#[derive(Debug, Default)]
pub struct Session {
    pub id: String,
    pub task: String,
    pub strikes: u32,
    pub reasons: Vec<String>,
    pub last_failing_output: Option<String>,
    pub escalations: u32,
    pub last_handoff: Option<String>,
    pub requests: u64,
    seen: usize,
    error_sigs: HashMap<String, u32>,
    reason_keys: HashMap<String, usize>,
    call_names: HashMap<String, String>,
    last_call: Option<String>,
    repeat_run: u32,
    pub last_seen: Option<Instant>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub task: String,
    pub strikes: u32,
    pub reasons: Vec<String>,
    pub escalations: u32,
    pub last_handoff: Option<String>,
    pub requests: u64,
}

impl Session {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into(), ..Default::default() }
    }

    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.id.clone(),
            task: truncate(&self.task, 120),
            strikes: self.strikes,
            reasons: self.reasons.clone(),
            escalations: self.escalations,
            last_handoff: self.last_handoff.clone(),
            requests: self.requests,
        }
    }

    /// Look at messages the router hasn't seen yet and update strikes.
    pub fn observe(&mut self, messages: &[Value]) {
        self.requests += 1;
        self.last_seen = Some(Instant::now());
        if self.task.is_empty() {
            if let Some(first) = messages.iter().find(|m| m["role"] == "user") {
                self.task = content_text(&first["content"]);
            }
        }
        // History got shorter: the agent compacted it. Start fresh from here.
        if messages.len() < self.seen {
            self.seen = messages.len();
            return;
        }
        for m in &messages[self.seen..] {
            match m["role"].as_str() {
                Some("assistant") => self.observe_calls(m),
                Some("tool") => self.observe_tool_result(m),
                _ => {}
            }
        }
        self.seen = messages.len();
    }

    fn observe_calls(&mut self, m: &Value) {
        for call in m["tool_calls"].as_array().into_iter().flatten() {
            let name = call["function"]["name"].as_str().unwrap_or("?").to_string();
            let args = call["function"]["arguments"].as_str().unwrap_or("");
            if let Some(id) = call["id"].as_str() {
                self.call_names.insert(id.to_string(), name.clone());
            }
            if !args.is_empty() && serde_json::from_str::<Value>(args).is_err() {
                self.strike(1, format!("malformed tool call ({name})"));
            }
            // Only identical calls back to back count as a loop: re-running a
            // build after an edit is normal, re-running it with nothing in
            // between is not.
            let key = format!("{name}\u{0}{args}");
            if self.last_call.as_deref() == Some(key.as_str()) {
                self.repeat_run += 1;
                if self.repeat_run == 3 {
                    self.strike(2, format!("same {name} call 3 times in a row"));
                }
            } else {
                self.last_call = Some(key);
                self.repeat_run = 1;
            }
        }
    }

    fn observe_tool_result(&mut self, m: &Value) {
        let name = m["tool_call_id"]
            .as_str()
            .and_then(|id| self.call_names.get(id))
            .cloned()
            .or_else(|| m["name"].as_str().map(str::to_string))
            .unwrap_or_default();
        // Reads, edits and searches print source code, which is full of the
        // word "error". Only command output is judged.
        if !name.is_empty() && !COMMAND_TOOL.is_match(&name) {
            return;
        }
        let text = content_text(&m["content"]);
        if let Some((raw, sig)) = error_line(&text) {
            self.last_failing_output = Some(text);
            let n = self.error_sigs.entry(sig.clone()).or_insert(0);
            *n += 1;
            if *n >= 2 {
                let n = *n;
                self.strike_keyed(1, format!("same error {n} times: {}", truncate(&raw, 120)), Some(&sig));
            }
        } else if SUCCESS.is_match(&text) {
            self.reset();
        }
    }

    fn strike(&mut self, n: u32, why: String) {
        self.strike_keyed(n, why, None);
    }

    /// A strike with a key replaces the earlier reason with the same key, so
    /// "same error 3 times: X" replaces "same error 2 times: X".
    fn strike_keyed(&mut self, n: u32, why: String, key: Option<&str>) {
        self.strikes += n;
        tracing::debug!(session = %self.id, strikes = self.strikes, %why, "strike");
        if let Some(k) = key {
            if let Some(&i) = self.reason_keys.get(k) {
                self.reasons[i] = why;
                return;
            }
            self.reason_keys.insert(k.to_string(), self.reasons.len());
        }
        self.reasons.push(why);
    }

    pub fn reset(&mut self) {
        self.strikes = 0;
        self.reasons.clear();
        self.reason_keys.clear();
        self.error_sigs.clear();
        self.repeat_run = 0;
        self.last_call = None;
    }
}

/// A stable fingerprint of the first error line: paths and numbers removed,
/// so "foo.rs:12:5" and "foo.rs:14:9" count as the same error.
#[cfg(test)]
pub fn error_signature(text: &str) -> Option<String> {
    error_line(text).map(|(_, sig)| sig)
}

/// The first real error line, as written and as a fingerprint.
fn error_line(text: &str) -> Option<(String, String)> {
    let mut bare_exit = None;
    let found = ERROR_LINE
        .find_iter(text)
        .map(|m| m.as_str().trim())
        .filter(|l| !ZERO_FAIL.is_match(l))
        .find(|l| {
            if BARE_EXIT.is_match(l) {
                bare_exit.get_or_insert(*l);
                false
            } else {
                true
            }
        });
    let line = match (found, bare_exit) {
        (Some(l), _) => l.to_string(),
        // Only an exit code: the failure is whatever the command printed last
        // (that's where tools put the reason). No output at all, like grep
        // finding nothing, isn't an error worth counting.
        (None, Some(exit)) => {
            let detail = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !BARE_EXIT.is_match(l))
                .last()?;
            format!("{} {detail}", exit.trim_end_matches(':'))
        }
        (None, None) => return None,
    };
    let s = PATHISH.replace_all(&line, "<path>");
    let s = DIGITS.replace_all(&s, "#");
    Some((line.clone(), truncate(&s.to_lowercase(), 160)))
}

pub fn content_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

/// Session id: the client's x-session-id header, else a hash of the first
/// user message (agents keep it fixed for the whole session).
pub fn session_key(header: Option<&str>, messages: &[Value]) -> String {
    if let Some(h) = header.filter(|h| !h.is_empty()) {
        return truncate(h, 64);
    }
    let first = messages
        .iter()
        .find(|m| m["role"] == "user")
        .map(|m| content_text(&m["content"]))
        .unwrap_or_default();
    // FNV-1a: stable across restarts, unlike std's randomized hasher.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in first.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("s{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str, name: &str, args: &str) -> Value {
        json!({"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":args}}]})
    }
    fn result(id: &str, text: &str) -> Value {
        json!({"role":"tool","tool_call_id":id,"content":text})
    }

    #[test]
    fn signature_ignores_line_numbers_and_paths() {
        let a = error_signature("src/main.rs:12:5: error[E0308]: mismatched types").unwrap();
        let b = error_signature("C:\\proj\\src\\main.rs:40:1: error[E0308]: mismatched types").unwrap();
        assert_eq!(a, b);
        assert!(error_signature("Build succeeded. 0 warnings").is_none());
        assert!(error_signature("test result: ok. 12 passed; 0 failed; 0 ignored").is_none());
        assert!(error_signature("Tests: 3 failed, 9 passed").is_some());
    }

    #[test]
    fn bare_exit_codes_are_not_all_the_same_error() {
        // OpenCode prints "Exit code 1:" above the output of any failed command.
        assert!(error_signature("Exit code 1:\n").is_none()); // e.g. grep found nothing
        let a = error_signature("Exit code 1:\nls: cannot access 'Gen/x.py': No such file or directory").unwrap();
        let b = error_signature("Exit code 1:\nModuleNotFoundError: No module named 'bpy'").unwrap();
        assert_ne!(a, b);
        // A real error line still wins over the bare exit code.
        let c = error_signature("Exit code 2:\nsrc/a.rs:3:1: error[E0425]: cannot find value `x`\nmore").unwrap();
        assert!(c.contains("e#"), "{c}");

        let mut s = Session::new("t");
        let mut msgs = vec![json!({"role": "user", "content": "build it"})];
        for (i, out) in ["Exit code 1:", "Exit code 1:\nnope: not found", "Exit code 1:\nNo such file: a.txt", "Exit code 1:"]
            .iter()
            .enumerate()
        {
            msgs.push(call(&format!("c{i}"), "bash", &format!("{{\"command\":\"cmd{i}\"}}")));
            msgs.push(result(&format!("c{i}"), out));
            s.observe(&msgs);
        }
        assert_eq!(s.strikes, 0, "{:?}", s.reasons);
    }

    #[test]
    fn repeated_build_error_strikes_and_success_resets() {
        let mut s = Session::default();
        let mut msgs = vec![json!({"role":"user","content":"fix the build"})];
        let err = "Assets/Player.cs(10,5): error CS0103: The name 'speed' does not exist";
        for i in 0..4 {
            msgs.push(call(&format!("c{i}"), "bash", &format!("{{\"cmd\":\"dotnet build\",\"try\":{i}}}")));
            msgs.push(result(&format!("c{i}"), err));
            s.observe(&msgs);
        }
        assert_eq!(s.strikes, 3, "{:?}", s.reasons);
        assert_eq!(s.reasons.len(), 1);
        assert!(s.reasons[0].starts_with("same error 4 times"));
        msgs.push(call("c9", "bash", "{\"cmd\":\"dotnet build\"}"));
        msgs.push(result("c9", "Build succeeded.\n    0 Error(s)"));
        s.observe(&msgs);
        assert_eq!(s.strikes, 0);
    }

    #[test]
    fn reading_source_with_error_text_is_ignored() {
        let mut s = Session::default();
        let mut msgs = vec![json!({"role":"user","content":"x"})];
        for i in 0..3 {
            msgs.push(call(&format!("r{i}"), "read", &format!("{{\"path\":\"a{i}.rs\"}}")));
            msgs.push(result(&format!("r{i}"), "fn f() -> Result<(), Error> { panic!(\"error: bad\") }"));
        }
        s.observe(&msgs);
        assert_eq!(s.strikes, 0);
    }

    #[test]
    fn loop_and_malformed_calls() {
        let mut s = Session::default();
        let mut msgs = vec![json!({"role":"user","content":"x"})];
        for i in 0..3 {
            msgs.push(call(&format!("l{i}"), "grep", "{\"q\":\"foo\"}"));
            msgs.push(result(&format!("l{i}"), "no matches"));
        }
        msgs.push(call("m", "edit", "{\"path\": \"a.rs\", "));
        s.observe(&msgs);
        assert_eq!(s.strikes, 3, "{:?}", s.reasons);
    }

    #[test]
    fn session_key_is_stable() {
        let msgs = vec![json!({"role":"user","content":"make the player jump"})];
        assert_eq!(session_key(None, &msgs), session_key(None, &msgs));
        assert_eq!(session_key(Some("abc"), &msgs), "abc");
    }
}
