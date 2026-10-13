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
    /// This PC's GPU.
    Local,
    /// The big-memory machine (e.g. a Mac Studio).
    Remote,
}

impl Target {
    pub fn name(self) -> &'static str {
        match self {
            Target::Local => "local",
            Target::Remote => "remote",
        }
    }

    /// Model names that pin a request to one machine.
    pub fn from_model_name(m: &str) -> Option<Target> {
        match m {
            "local" | "pc" | "jayspc" => Some(Target::Local),
            "remote" | "mac" => Some(Target::Remote),
            _ => None,
        }
    }
}

/// What the registry knows about a machine right now.
#[derive(Debug, Clone, Copy, Default)]
pub struct NodeView {
    pub online: bool,
    /// Context window of the loaded model, if the machine reported one.
    pub n_ctx: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Route { target: Target, rule: &'static str },
    /// Nothing local can take it (e.g. prompt too big for any loaded model).
    NoLocal { reason: String },
}

/// The routing rules, evaluated top to bottom.
/// `pinned` is set when the client asked for a machine by name instead of "auto".
/// `remote_set_up` is false until someone adds the big machine; until then
/// this PC takes everything, agent turns included.
pub fn decide(
    shape: &Shape,
    pinned: Option<Target>,
    remote: NodeView,
    local: NodeView,
    remote_set_up: bool,
) -> Decision {
    let fits = |n: NodeView| n.n_ctx.map_or(true, |c| shape.est_tokens < c);

    if let Some(t) = pinned {
        return Decision::Route { target: t, rule: "pinned by model name" };
    }

    if !remote_set_up {
        if local.online && fits(local) {
            return Decision::Route { target: Target::Local, rule: "this PC (no remote machine set up)" };
        }
    } else {
        match shape.kind {
            Kind::Completion => {
                if local.online && fits(local) {
                    return Decision::Route { target: Target::Local, rule: "completion -> this PC" };
                }
                if remote.online && fits(remote) {
                    return Decision::Route { target: Target::Remote, rule: "completion fallback -> remote" };
                }
            }
            Kind::Agent | Kind::Chat => {
                if remote.online && fits(remote) {
                    let rule = match (shape.kind, shape.vision) {
                        (_, true) => "image -> remote",
                        (Kind::Agent, _) => "agent turn -> remote",
                        _ => "chat -> remote",
                    };
                    return Decision::Route { target: Target::Remote, rule };
                }
                // Agent turns never spill to the small model: quality drops too far.
                if shape.kind == Kind::Chat && !shape.vision && local.online && fits(local) {
                    return Decision::Route { target: Target::Local, rule: "chat fallback -> this PC" };
                }
            }
        }
    }

    let reason = if !remote.online && !local.online {
        if remote_set_up {
            "no model machine is online".to_string()
        } else {
            "this PC's model isn't running (press Start in the app)".to_string()
        }
    } else if remote_set_up && shape.kind == Kind::Agent && !remote.online {
        "the remote machine is offline and agent turns are not sent to the smaller local model".to_string()
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
        assert_eq!(decide(&s(Kind::Completion), None, UP, SMALL, true), Decision::Route { target: Target::Local, rule: "completion -> this PC" });
        assert!(matches!(decide(&s(Kind::Agent), None, UP, SMALL, true), Decision::Route { target: Target::Remote, .. }));
        assert!(matches!(decide(&s(Kind::Chat), None, DOWN, SMALL, true), Decision::Route { target: Target::Local, .. }));
        assert!(matches!(decide(&s(Kind::Agent), None, DOWN, SMALL, true), Decision::NoLocal { .. }));
        assert!(matches!(decide(&s(Kind::Completion), None, UP, DOWN, true), Decision::Route { target: Target::Remote, .. }));
        let big = Shape { kind: Kind::Chat, vision: false, est_tokens: 100_000 };
        assert!(matches!(decide(&big, None, UP, SMALL, true), Decision::NoLocal { .. }));
        assert!(matches!(decide(&s(Kind::Agent), Some(Target::Local), UP, SMALL, true), Decision::Route { target: Target::Local, .. }));
    }

    #[test]
    fn without_a_remote_this_pc_takes_everything() {
        let agent = Shape { kind: Kind::Agent, vision: false, est_tokens: 1000 };
        assert!(matches!(decide(&agent, None, DOWN, SMALL, false), Decision::Route { target: Target::Local, .. }));
        match decide(&agent, None, DOWN, DOWN, false) {
            Decision::NoLocal { reason } => assert!(reason.contains("press Start")),
            d => panic!("{d:?}"),
        }
    }
}
