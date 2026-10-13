//! Live numbers and the verbose log for the app: per-machine traffic
//! counters, a ring buffer of log lines, and a broadcast channel the UI
//! listens to over Server-Sent Events.

use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::broadcast;

pub const LOCAL: usize = 0;
pub const REMOTE: usize = 1;
pub const CLAUDE: usize = 2;

#[derive(Default)]
struct Counters {
    in_flight: AtomicI64,
    requests: AtomicU64,
    tokens: AtomicU64,
    bytes: AtomicU64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LinkStats {
    pub in_flight: i64,
    pub requests: u64,
    pub tokens: u64,
    /// Tokens per second over the last sample.
    pub tokens_per_sec: f64,
    /// Requests started in the last minute.
    pub requests_per_min: u64,
    pub bytes_per_sec: f64,
}

pub struct Traffic {
    links: [Counters; 3],
    history: Mutex<[VecDeque<(u64, u64, u64)>; 3]>, // (requests, tokens, bytes) per second, last 60s
    last: Mutex<[LinkStats; 3]>,
}

impl Traffic {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            links: Default::default(),
            history: Mutex::new(Default::default()),
            last: Mutex::new(Default::default()),
        })
    }

    /// Counts a request as in flight until the returned guard is dropped
    /// (which, for a streamed reply, is when the stream ends).
    pub fn start(self: &Arc<Self>, link: usize) -> InFlight {
        self.links[link].in_flight.fetch_add(1, Ordering::Relaxed);
        self.links[link].requests.fetch_add(1, Ordering::Relaxed);
        InFlight { t: self.clone(), link }
    }

    /// A handoff is a one-shot "request" on the Claude link.
    pub fn pulse(&self, link: usize) {
        self.links[link].requests.fetch_add(1, Ordering::Relaxed);
    }

    /// Feed a chunk of a reply. Streamed replies count one token per SSE
    /// event; whole replies use the server's own completion_tokens figure.
    pub fn chunk(&self, link: usize, bytes: &[u8], streamed: bool) {
        let c = &self.links[link];
        c.bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        let tokens = if streamed {
            count(bytes, b"data: {")
        } else {
            completion_tokens(bytes).unwrap_or(0)
        };
        c.tokens.fetch_add(tokens, Ordering::Relaxed);
    }

    /// Called once a second: turns totals into rates.
    pub fn sample(&self) -> [LinkStats; 3] {
        let mut hist = self.history.lock().unwrap();
        let mut out: [LinkStats; 3] = Default::default();
        for i in 0..3 {
            let c = &self.links[i];
            let now = (
                c.requests.load(Ordering::Relaxed),
                c.tokens.load(Ordering::Relaxed),
                c.bytes.load(Ordering::Relaxed),
            );
            let h = &mut hist[i];
            let prev = h.back().copied().unwrap_or(now);
            let minute_ago = h.front().copied().unwrap_or(now);
            h.push_back(now);
            while h.len() > 60 {
                h.pop_front();
            }
            out[i] = LinkStats {
                in_flight: c.in_flight.load(Ordering::Relaxed).max(0),
                requests: now.0,
                tokens: now.1,
                tokens_per_sec: (now.1 - prev.1) as f64,
                requests_per_min: now.0 - minute_ago.0,
                bytes_per_sec: (now.2 - prev.2) as f64,
            };
        }
        *self.last.lock().unwrap() = out.clone();
        out
    }

    pub fn last(&self) -> [LinkStats; 3] {
        self.last.lock().unwrap().clone()
    }
}

pub struct InFlight {
    t: Arc<Traffic>,
    link: usize,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.t.links[self.link].in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

fn count(hay: &[u8], needle: &[u8]) -> u64 {
    hay.windows(needle.len()).filter(|w| *w == needle).count() as u64
}

fn completion_tokens(bytes: &[u8]) -> Option<u64> {
    let key = b"\"completion_tokens\":";
    let i = bytes.windows(key.len()).position(|w| w == key)? + key.len();
    let digits: String = bytes[i..]
        .iter()
        .skip_while(|b| b.is_ascii_whitespace())
        .take_while(|b| b.is_ascii_digit())
        .map(|b| *b as char)
        .collect();
    digits.parse().ok()
}

// ------------------------------------------------------------ log + events

#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    pub time: String,
    pub level: String,
    /// "router", "model" (llama-server's own output), "request", "handoff"…
    pub source: String,
    pub message: String,
}

pub struct Events {
    pub tx: broadcast::Sender<Arc<str>>,
    log: Mutex<VecDeque<LogLine>>,
}

static EVENTS: OnceLock<Arc<Events>> = OnceLock::new();

pub fn events() -> &'static Arc<Events> {
    EVENTS.get_or_init(|| {
        let (tx, _) = broadcast::channel(1024);
        Arc::new(Events { tx, log: Mutex::new(VecDeque::new()) })
    })
}

impl Events {
    pub fn send(&self, kind: &str, data: impl Serialize) {
        let msg = serde_json::json!({ "type": kind, "data": data }).to_string();
        let _ = self.tx.send(Arc::from(msg));
    }

    pub fn log(&self, line: LogLine) {
        {
            let mut q = self.log.lock().unwrap();
            q.push_back(line.clone());
            while q.len() > 1000 {
                q.pop_front();
            }
        }
        self.send("log", line);
    }

    pub fn recent_log(&self, n: usize) -> Vec<LogLine> {
        let q = self.log.lock().unwrap();
        q.iter().skip(q.len().saturating_sub(n)).cloned().collect()
    }
}

/// A tracing layer that copies every event into the app's verbose log.
pub struct UiLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for UiLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let meta = event.metadata();
        let target = meta.target();
        // Only our own crate and the model's output; skip HTTP library noise.
        let source = match target {
            "model" | "request" | "handoff" | "router" | "claude" | "apps" => target.to_string(),
            t if t.starts_with("poolparty_router") => "router".to_string(),
            _ => return,
        };
        let mut v = Visitor::default();
        event.record(&mut v);
        let mut message = v.message;
        for (k, val) in v.fields {
            message.push_str(&format!("  {k}={val}"));
        }
        events().log(LogLine {
            time: chrono::Local::now().format("%H:%M:%S").to_string(),
            level: meta.level().to_string().to_lowercase(),
            source,
            message,
        });
    }
}

#[derive(Default)]
struct Visitor {
    message: String,
    fields: Vec<(String, String)>,
}

impl tracing::field::Visit for Visitor {
    fn record_str(&mut self, f: &tracing::field::Field, value: &str) {
        if f.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push((f.name().to_string(), value.to_string()));
        }
    }
    fn record_debug(&mut self, f: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if f.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields.push((f.name().to_string(), format!("{value:?}")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_tokens_and_in_flight() {
        let t = Traffic::new();
        t.sample(); // baseline
        {
            let _g = t.start(LOCAL);
            t.chunk(LOCAL, b"data: {\"a\":1}\n\ndata: {\"a\":2}\n\ndata: [DONE]\n\n", true);
            assert_eq!(t.sample()[LOCAL].in_flight, 1);
        }
        t.chunk(REMOTE, br#"{"usage":{"prompt_tokens":5,"completion_tokens": 42}}"#, false);
        let s = t.sample();
        assert_eq!(s[LOCAL].in_flight, 0);
        assert_eq!(s[LOCAL].tokens, 2);
        assert_eq!(s[REMOTE].tokens, 42);
        assert_eq!(s[LOCAL].requests_per_min, 1);
    }
}
