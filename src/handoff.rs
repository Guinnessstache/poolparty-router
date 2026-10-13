//! Claude Code handoff: when a session is stuck, write a short brief to a
//! file and reply to the agent with instructions instead of another local try.
//!
//! The brief deliberately leaves out the full local transcript: the stronger
//! model gets the task, what was tried, and the last failure, so the local
//! model's wrong turns don't steer it.

use crate::escalation::{content_text, truncate, Session};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_FAILURE_CHARS: usize = 16_000;

pub struct Handoff {
    pub path: PathBuf,
    pub reasons: Vec<String>,
}

/// Write the handoff brief. `summary` is the local model's "what was tried"
/// paragraph, or None to fall back to a mechanical list of recent tool calls.
pub fn write(dir: &Path, s: &Session, messages: &[Value], summary: Option<String>) -> std::io::Result<Handoff> {
    std::fs::create_dir_all(dir)?;
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H%M%S");
    let path = dir.join(format!("handoff_{stamp}_{}.md", s.id));

    let tried = summary.unwrap_or_else(|| mechanical_summary(messages));
    let failure = s
        .last_failing_output
        .as_deref()
        .map(|f| tail(f, MAX_FAILURE_CHARS))
        .unwrap_or_else(|| "(no failing command output was captured)".into());

    let md = format!(
        "# Handoff from PoolParty\n\n\
         A local model got stuck on this task. Pick it up from here.\n\n\
         ## Task\n\n{task}\n\n\
         ## Why it was handed off\n\n{why}\n\n\
         ## What was already tried\n\n{tried}\n\n\
         ## Last failing output\n\n```\n{failure}\n```\n\n\
         ## Instructions\n\n\
         Look at the current state of the files in this project (the local model's \
         edits are already on disk), find the root cause of the failure above, fix it, \
         and run the same build or test command to confirm it passes.\n",
        task = s.task.trim(),
        why = s.reasons.iter().map(|r| format!("- {r}")).collect::<Vec<_>>().join("\n"),
    );
    std::fs::write(&path, md)?;
    Ok(Handoff { path, reasons: s.reasons.clone() })
}

/// The last N tool calls, as a fallback when the local model can't summarize.
fn mechanical_summary(messages: &[Value]) -> String {
    let calls: Vec<String> = messages
        .iter()
        .filter(|m| m["role"] == "assistant")
        .flat_map(|m| m["tool_calls"].as_array().cloned().unwrap_or_default())
        .map(|c| {
            format!(
                "- `{}` {}",
                c["function"]["name"].as_str().unwrap_or("?"),
                truncate(c["function"]["arguments"].as_str().unwrap_or(""), 200)
            )
        })
        .collect();
    if calls.is_empty() {
        return "(no tool calls recorded)".into();
    }
    let start = calls.len().saturating_sub(12);
    format!("The most recent tool calls, oldest first:\n\n{}", calls[start..].join("\n"))
}

/// Ask the Mac's model for a short "approaches tried and how each failed".
pub async fn summarize(
    http: &reqwest::Client,
    url: &str,
    api_key: Option<&str>,
    model: Option<&str>,
    messages: &[Value],
) -> Option<String> {
    let mut transcript = String::new();
    for m in messages.iter().rev().take(40).collect::<Vec<_>>().into_iter().rev() {
        let role = m["role"].as_str().unwrap_or("?");
        let mut text = content_text(&m["content"]);
        for c in m["tool_calls"].as_array().into_iter().flatten() {
            text.push_str(&format!(
                "\n[calls {} {}]",
                c["function"]["name"].as_str().unwrap_or("?"),
                truncate(c["function"]["arguments"].as_str().unwrap_or(""), 300)
            ));
        }
        if role == "system" || text.trim().is_empty() {
            continue;
        }
        transcript.push_str(&format!("\n--- {role} ---\n{}\n", tail(&text, 1500)));
    }
    let body = json!({
        "model": model.unwrap_or("local"),
        "stream": false,
        "max_tokens": 600,
        "temperature": 0.2,
        "messages": [
            {"role": "system", "content": "You write terse engineering handoff notes."},
            {"role": "user", "content": format!(
                "Below is the end of a coding session that got stuck. List each approach \
                 that was tried and how it failed, as short bullets, under 300 words. \
                 No preamble.\n{transcript}")}
        ]
    });
    let mut req = http.post(format!("{url}/chat/completions")).json(&body).timeout(Duration::from_secs(120));
    if let Some(k) = api_key {
        req = req.bearer_auth(k);
    }
    let v: Value = req.send().await.ok()?.error_for_status().ok()?.json().await.ok()?;
    let text = v["choices"][0]["message"]["content"].as_str()?.trim().to_string();
    // Thinking models may leave their reasoning inline.
    let text = match text.rfind("</think>") {
        Some(i) => text[i + 8..].trim().to_string(),
        None => text,
    };
    (!text.is_empty()).then_some(text)
}

/// The text the agent shows the user in place of another local attempt.
pub fn stop_message(h: &Handoff) -> String {
    let p = h.path.display();
    format!(
        "PoolParty paused this session: the local model looks stuck.\n\n{reasons}\n\n\
         A handoff brief is saved at:\n{p}\n\n\
         To continue with Claude, open a terminal in your project and run:\n\n\
         claude \"Read {p} and take over the task it describes.\"\n\n\
         (Claude Code uses your Claude plan, so this costs nothing extra.) \
         When it's fixed, keep working here as normal — the session starts fresh.",
        reasons = h.reasons.iter().map(|r| format!("- {r}")).collect::<Vec<_>>().join("\n"),
    )
}

/// An OpenAI chat.completion (or its streamed equivalent) carrying `text`.
pub fn completion_json(model: &str, text: &str) -> Value {
    json!({
        "id": format!("poolparty-{}", chrono::Utc::now().timestamp_millis()),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": text}}],
        "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
    })
}

pub fn completion_sse(model: &str, text: &str) -> String {
    let id = format!("poolparty-{}", chrono::Utc::now().timestamp_millis());
    let created = chrono::Utc::now().timestamp();
    let chunk = |delta: Value, finish: Value| {
        json!({"id": id, "object": "chat.completion.chunk", "created": created, "model": model,
               "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    };
    let mut out = String::new();
    for c in [
        chunk(json!({"role": "assistant", "content": ""}), Value::Null),
        chunk(json!({"content": text}), Value::Null),
        chunk(json!({}), json!("stop")),
    ] {
        out.push_str(&format!("data: {c}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        let t: String = s.chars().skip(n - max).collect();
        format!("…{t}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_brief() {
        let dir = std::env::temp_dir().join("pp-handoff-test");
        let mut s = Session::new("s1");
        s.task = "Make the knight jump".into();
        s.reasons.push("same error again: error cs#".into());
        s.last_failing_output = Some("Player.cs(3,1): error CS0103".into());
        let msgs = vec![serde_json::json!({"role":"assistant","tool_calls":[{"id":"a","function":{"name":"bash","arguments":"{\"cmd\":\"dotnet build\"}"}}]})];
        let h = write(&dir, &s, &msgs, None).unwrap();
        let md = std::fs::read_to_string(&h.path).unwrap();
        assert!(md.contains("Make the knight jump"));
        assert!(md.contains("dotnet build"));
        assert!(md.contains("error CS0103"));
        assert!(stop_message(&h).contains("claude \"Read"));
    }

    #[test]
    fn sse_is_well_formed() {
        let s = completion_sse("auto", "hi");
        assert!(s.ends_with("data: [DONE]\n\n"));
        for line in s.lines().filter(|l| l.starts_with("data: {")) {
            serde_json::from_str::<Value>(&line[6..]).unwrap();
        }
    }
}
