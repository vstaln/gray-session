//! gray-session — one sidecar for session utilities.
//!
//! Merges gray-bookmark, gray-session-name, gray-share, gray-handoff and
//! gray-summarize. Each source lives verbatim in `mods/` with its own state
//! files, tools and slash commands — only the wire plumbing is shared.
//!
//! Hook fan-out:
//! - `prompt/context` → session_name (session name surfaced once).
//! - `context/build`  → handoff, then summarize (chained: each observes and
//!   caches the conversation under its own `~/.gray/<orig>/` dir).
//! - `tool/call`      → `bookmark` / `share` by `params.name`.
//! - `command/run`    → routed by `params.name`; `/session` is an overview.
//!
//! Host round-trips: bookmark + session_name use `host/ask`, handoff +
//! summarize use `host/run`. One stdin reader thread routes host answers
//! (string id, no method) to a shared pending map; one id counter keeps the
//! `q<n>` ids unique across modules.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

#[path = "mods/bookmark.rs"]
mod bookmark;
#[path = "mods/handoff.rs"]
mod handoff;
#[path = "mods/session_name.rs"]
mod session_name;
#[path = "mods/share.rs"]
mod share;
#[path = "mods/summarize.rs"]
mod summarize;

/// Internal cap on `host/ask` waits — under the host's 330s outer TTL
/// (same as gray-bookmark / gray-session-name).
const ASK_TTL: Duration = Duration::from_secs(300);

type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;

fn manifest() -> Value {
    let mut tools: Vec<Value> = Vec::new();
    for m in [
        bookmark::manifest_bookmark(),
        session_name::manifest_session_name(),
        share::manifest_share(),
        handoff::manifest_handoff(),
        summarize::manifest_summarize(),
    ] {
        if let Some(ts) = m.get("tools").and_then(Value::as_array) {
            for t in ts {
                let name = t.get("name").and_then(Value::as_str).unwrap_or("");
                if !tools
                    .iter()
                    .any(|x| x.get("name").and_then(Value::as_str) == Some(name))
                {
                    tools.push(t.clone());
                }
            }
        }
    }
    json!({
        "name": "session",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": tools,
        "commands": [
            "/session", "/bookmark", "/bookmarks", "/name", "/share", "/handoff", "/summarize",
        ],
        "hooks": ["prompt/context", "context/build"],
        "capabilities": ["host.ask", "host.turn"],
    })
}

/// Host-side channels shared by every module that calls back into the host.
struct Host {
    out: Arc<Mutex<std::io::Stdout>>,
    pending: Pending,
    counter: Arc<Mutex<u64>>,
}

impl Host {
    /// `host/ask` round-trip; returns the reply's `result` (Null if absent),
    /// or None on timeout — exactly what the original modules' askers did.
    fn ask(&self, params: Value) -> Option<Value> {
        let id = {
            let mut n = self.counter.lock().expect("id counter");
            *n += 1;
            format!("q{n}")
        };
        let (tx, rx) = mpsc::channel::<Value>();
        self.pending.lock().expect("pending").insert(id.clone(), tx);
        {
            let req = json!({"id": id, "method": "host/ask", "params": params});
            let mut o = self.out.lock().expect("stdout");
            let _ = writeln!(o, "{req}");
            let _ = o.flush();
        }
        let reply = rx.recv_timeout(ASK_TTL).ok();
        self.pending.lock().expect("pending").remove(&id);
        reply.map(|v| v.get("result").cloned().unwrap_or(Value::Null))
    }

    fn handoff_link(&self) -> handoff::HostLink {
        handoff::HostLink {
            out: self.out.clone(),
            pending: self.pending.clone(),
            counter: self.counter.clone(),
        }
    }

    fn summarize_link(&self) -> summarize::HostLink {
        summarize::HostLink {
            out: self.out.clone(),
            pending: self.pending.clone(),
            counter: self.counter.clone(),
        }
    }
}

fn call_bookmark(req: &Value, host: &Host) -> (Option<Value>, bool) {
    let mut ask = |params: &Value| host.ask(params.clone());
    bookmark::handle_bookmark(req, &mut ask)
}

fn call_session_name(req: &Value, host: &Host) -> (Option<Value>, bool) {
    let mut ask =
        |questions: &[Value]| host.ask(json!({ "questions": questions, "blocking": true }));
    session_name::handle_session_name(req, &mut ask)
}

/// `context/build` chain: handoff then summarize. Both only observe today,
/// but if one returns `messages` the next sees them as `params.messages`.
fn context_build(req: &Value, host: &Host) -> Value {
    let mut req = req.clone();
    let mut replaced: Option<Value> = None;
    let link_h = host.handoff_link();
    let link_s = host.summarize_link();
    let steps: [&dyn Fn(&Value) -> (Option<Value>, bool); 2] =
        [&|r| handoff::handle_handoff(r, Some(&link_h)), &|r| {
            summarize::handle_summarize(r, Some(&link_s))
        }];
    for step in steps {
        if let (Some(reply), _) = step(&req)
            && let Some(msgs) = reply.pointer("/result/messages")
        {
            if let Some(p) = req.get_mut("params").and_then(Value::as_object_mut) {
                p.insert("messages".into(), msgs.clone());
            }
            replaced = Some(msgs.clone());
        }
    }
    match replaced {
        Some(m) => json!({ "messages": m }),
        None => json!({}),
    }
}

/// `/session` — merged-plugin overview.
fn session_command() -> String {
    format!(
        "gray-session {} — one sidecar, five session utilities:\n\
         \x20 /bookmark, /bookmarks  named markers to `gray resume` around (+ bookmark tool)\n\
         \x20 /name       friendly session names (/name set asks interactively)\n\
         \x20 /share      export the transcript as self-contained HTML (+ share tool)\n\
         \x20 /handoff    generate a focused prompt for a fresh session\n\
         \x20 /summarize  summarize the current conversation",
        env!("CARGO_PKG_VERSION")
    )
}

fn handle(req: &Value, host: &Host) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest(),
        "prompt/context" => {
            let (reply, _) = call_session_name(req, host);
            return (reply, false);
        }
        "context/build" => context_build(req, host),
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            match name {
                "bookmark" => {
                    let (reply, _) = call_bookmark(req, host);
                    return (reply, false);
                }
                "share" => {
                    let (reply, _) = share::handle_share(req);
                    return (reply, false);
                }
                _ => json!({ "content": format!("unknown tool {name}"), "is_error": true }),
            }
        }
        "command/run" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let (reply, _) = match name {
                "/session" => {
                    return (
                        Some(json!({ "id": id, "result": { "text": session_command() } })),
                        false,
                    );
                }
                "/bookmark" | "/bookmarks" => call_bookmark(req, host),
                "/name" => call_session_name(req, host),
                "/share" => share::handle_share(req),
                "/handoff" => handoff::handle_handoff(req, Some(&host.handoff_link())),
                "/summarize" => summarize::handle_summarize(req, Some(&host.summarize_link())),
                _ => {
                    let text = format!("unknown command {name} — try /session");
                    return (Some(json!({ "id": id, "result": { "text": text } })), false);
                }
            };
            return (reply, false);
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest());
        return Ok(());
    }
    let host = Host {
        out: Arc::new(Mutex::new(std::io::stdout())),
        pending: Arc::new(Mutex::new(HashMap::new())),
        counter: Arc::new(Mutex::new(0u64)),
    };

    // Reader thread: host answers (string id, no method, known pending id)
    // go to `pending` as the full message; everything else goes to the work
    // loop (`handle` drops id-less notifications).
    let (work_tx, work_rx) = mpsc::channel::<Value>();
    let reader_pending = host.pending.clone();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(id) = v.get("id").and_then(Value::as_str)
                && v.get("method").is_none()
                && let Some(tx) = reader_pending.lock().expect("pending").remove(id)
            {
                let _ = tx.send(v);
                continue;
            }
            if work_tx.send(v).is_err() {
                break;
            }
        }
    });

    for req in work_rx {
        let (reply, exit) = handle(&req, &host);
        if let Some(reply) = reply {
            let mut o = host.out.lock().expect("stdout");
            writeln!(o, "{reply}")?;
            o.flush()?;
        }
        if exit {
            break;
        }
    }
    Ok(())
}
