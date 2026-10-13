//! Cheap request classification (no model call) and the routing rules.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Fill-in-the-middle / tab completion: latency matters most.
    Completion,
    /// A coding-agent turn (the request carries tools).
    Agent,
    /// Plain chat.
    Chat,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Shape {
    pub kind: Kind,
    pub vision: bool,
    /// Rough prompt size: characters / 4.
    pub est_tokens: usize,
}

pub fn classify(path: &str, body: &Value) -> Shape {
    let est_tokens = estimate_tokens(body);
    let vision = has_image(body);
    let kind = if path.ends_with("/completions") && !path.ends_with("/chat/completions") {
        Kind::Completion
    } else if path.ends_with("/infill") {
        Kind::Completion
    } else if body.get("tools").and_then(|t| t.as_array()).is_some_and(|a| !a.is_empty()) {
        Kind::Agent
    } else if body
        .get("max_tokens")
        .or_else(|| body.get("max_completion_tokens"))
        .and_then(|v| v.as_u64())
        .is_some_and(|n| n <= 64)
    {
        Kind::Completion
    } else {
        Kind::Chat
    };
    Shape { kind, vision, est_tokens }
}

fn estimate_tokens(body: &Value) -> usize {
    let mut chars = 0usize;
    if let Some(p) = body.get("prompt") {
        chars += text_len(p);
    }
    for m in body.get("messages").and_then(|m| m.as_array()).into_iter().flatten() {
        chars += m.get("content").map(text_len).unwrap_or(0);
        if let Some(tc) = m.get("tool_calls") {
            chars += tc.to_string().len();
        }
    }
    if let Some(t) = body.get("tools") {
        chars += t.to_string().len();
    }
    chars / 4
}

fn text_len(v: &Value) -> usize {
    match v {
        Value::String(s) => s.len(),
        Value::Array(parts) => parts
            .iter()
            .map(|p| p.get("text").and_then(|t| t.as_str()).map_or(0, str::len))
            .sum(),
        _ => 0,
    }
}

fn has_image(body: &Value) -> bool {
    body.get("messages")
        .and_then(|m| m.as_array())
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("content").and_then(|c| c.as_array()))
        .flatten()
        .any(|p| matches!(p.get("type").and_then(|t| t.as_str()), Some("image_url" | "input_image" | "image")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    Mac,
    Jayspc,
}

impl Target {
    pub fn name(self) -> &'static str {
        match self {
            Target::Mac => "mac",
            Target::Jayspc => "jayspc",
        }
    }
}

/// What the registry knows about a node right now.
#[derive(Debug, Clone, Copy, Default)]
pub struct NodeView {
    pub online: bool,
    /// Context window of the loaded model, if the node reported one.
    pub n_ctx: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Route { target: Target, rule: &'static str },
    /// Nothing local can take it (e.g. prompt too big for any loaded model).
    NoLocal { reason: String },
}

/// The rule table from the spec, evaluated top to bottom.
/// `pinned` is set when the client asked for a node by name instead of "auto".
pub fn decide(shape: &Shape, pinned: Option<Target>, mac: NodeView, jay: NodeView) -> Decision {
    let fits = |n: NodeView| n.n_ctx.map_or(true, |c| shape.est_tokens < c);

    if let Some(t) = pinned {
        return Decision::Route { target: t, rule: "pinned by model name" };
    }

    match shape.kind {
        Kind::Completion => {
            if jay.online && fits(jay) {
                return Decision::Route { target: Target::Jayspc, rule: "completion -> 4070 Ti" };
            }
            if mac.online && fits(mac) {
                return Decision::Route { target: Target::Mac, rule: "completion fallback -> Mac" };
            }
        }
        Kind::Agent | Kind::Chat => {
            if mac.online && fits(mac) {
                let rule = match (shape.kind, shape.vision) {
                    (_, true) => "vision -> Mac",
                    (Kind::Agent, _) => "agent turn -> Mac",
                    _ => "chat -> Mac",
                };
                return Decision::Route { target: Target::Mac, rule };
            }
            // Agent turns never spill to the small model: quality drops too far.
            if shape.kind == Kind::Chat && !shape.vision && jay.online && fits(jay) {
                return Decision::Route { target: Target::Jayspc, rule: "chat fallback -> 4070 Ti" };
            }
        }
    }

    let reason = if !mac.online && !jay.online {
        "no local node is online".to_string()
    } else if shape.kind == Kind::Agent && !mac.online {
        "the Mac is offline and agent turns are not sent to the 4070 Ti".to_string()
    } else {
        format!("prompt (~{} tokens) is larger than any loaded model's context", shape.est_tokens)
    };
    Decision::NoLocal { reason }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const UP: NodeView = NodeView { online: true, n_ctx: Some(65536) };
    const SMALL: NodeView = NodeView { online: true, n_ctx: Some(8192) };
    const DOWN: NodeView = NodeView { online: false, n_ctx: None };

    #[test]
    fn classifies() {
        assert_eq!(classify("/v1/completions", &json!({"prompt":"fn main"})).kind, Kind::Completion);
        let agent = json!({"messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function"}]});
        assert_eq!(classify("/v1/chat/completions", &agent).kind, Kind::Agent);
        let chat = json!({"messages":[{"role":"user","content":"hi"}]});
        assert_eq!(classify("/v1/chat/completions", &chat).kind, Kind::Chat);
        let img = json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"x"}}]}]});
        assert!(classify("/v1/chat/completions", &img).vision);
    }

    #[test]
    fn routes() {
        let s = |kind| Shape { kind, vision: false, est_tokens: 1000 };
        assert_eq!(decide(&s(Kind::Completion), None, UP, SMALL), Decision::Route { target: Target::Jayspc, rule: "completion -> 4070 Ti" });
        assert!(matches!(decide(&s(Kind::Agent), None, UP, SMALL), Decision::Route { target: Target::Mac, .. }));
        assert!(matches!(decide(&s(Kind::Chat), None, DOWN, SMALL), Decision::Route { target: Target::Jayspc, .. }));
        assert!(matches!(decide(&s(Kind::Agent), None, DOWN, SMALL), Decision::NoLocal { .. }));
        assert!(matches!(decide(&s(Kind::Completion), None, UP, DOWN), Decision::Route { target: Target::Mac, .. }));
        let big = Shape { kind: Kind::Chat, vision: false, est_tokens: 100_000 };
        assert!(matches!(decide(&big, None, UP, SMALL), Decision::NoLocal { .. }));
        assert!(matches!(decide(&s(Kind::Agent), Some(Target::Jayspc), UP, SMALL), Decision::Route { target: Target::Jayspc, .. }));
    }
}
