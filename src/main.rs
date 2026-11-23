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

mod parts;

#[allow(dead_code)]
#[path = "mods/bookmark.rs"]
mod bookmark;
#[allow(dead_code)]
#[path = "mods/handoff.rs"]
mod handoff;
#[allow(dead_code)]
#[path = "mods/session_name.rs"]
mod session_name;
#[allow(dead_code)]
#[path = "mods/share.rs"]
mod share;
#[allow(dead_code)]
#[path = "mods/summarize.rs"]
mod summarize;

#[cfg(test)]
static HOME_LOCK: Mutex<()> = Mutex::new(());

/// Internal cap on `host/ask` waits — under the host's 330s outer TTL
/// (same as gray-bookmark / gray-session-name).
const ASK_TTL: Duration = Duration::from_secs(300);

type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;

const PARTS: &[parts::Part] = &[
    parts::Part {
        name: "bookmark",
        default: true,
        description: "named session markers plus the bookmark tool",
    },
    parts::Part {
        name: "session-name",
        default: true,
        description: "friendly session names surfaced in the prompt",
    },
    parts::Part {
        name: "share",
        default: true,
        description: "export a transcript to a self-contained HTML page",
    },
    parts::Part {
        name: "handoff",
        default: true,
        description: "write a prompt for continuing in a fresh session",
    },
    parts::Part {
        name: "summarize",
        default: true,
        description: "summarize the conversation so far",
    },
];

fn manifest(parts: &parts::Parts) -> Value {
    let mut tools: Vec<Value> = Vec::new();
    let mut sources = Vec::new();
    if parts.on("bookmark") {
        sources.push(bookmark::manifest_bookmark());
    }
    if parts.on("session-name") {
        sources.push(session_name::manifest_session_name());
    }
    if parts.on("share") {
        sources.push(share::manifest_share());
    }
    if parts.on("handoff") {
        sources.push(handoff::manifest_handoff());
    }
    if parts.on("summarize") {
        sources.push(summarize::manifest_summarize());
    }
    for m in sources {
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
    let mut commands: Vec<&str> = vec!["/session"];
    let mut descriptions = serde_json::Map::new();
    descriptions.insert("/session".into(), json!("list the session modules"));
    if parts.on("bookmark") {
        commands.extend(["/bookmark", "/bookmarks"]);
        descriptions.insert("/bookmark".into(), json!("mark this point in the session"));
        descriptions.insert("/bookmarks".into(), json!("list session bookmarks"));
    }
    if parts.on("session-name") {
        commands.push("/name");
        descriptions.insert("/name".into(), json!("name or rename this session"));
    }
    if parts.on("share") {
        commands.push("/share");
        descriptions.insert("/share".into(), json!("export a session to an HTML file"));
    }
    if parts.on("handoff") {
        commands.push("/handoff");
        descriptions.insert(
            "/handoff".into(),
            json!("write a prompt to continue in a new session"),
        );
    }
    if parts.on("summarize") {
        commands.push("/summarize");
        descriptions.insert(
            "/summarize".into(),
            json!("summarize the conversation so far"),
        );
    }
    let mut hooks: Vec<&str> = Vec::new();
    if parts.on("session-name") {
        hooks.push("prompt/context");
    }
    if parts.on("handoff") || parts.on("summarize") {
        hooks.push("context/build");
    }
    let mut capabilities: Vec<&str> = Vec::new();
    if parts.on("bookmark") || parts.on("session-name") {
        capabilities.push("host.ask");
    }
    if parts.on("handoff") || parts.on("summarize") {
        capabilities.push("host.turn");
    }
    json!({
        "name": "session",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": tools,
        "commands": commands,
        "command_descriptions": descriptions,
        "hooks": hooks,
        "capabilities": capabilities,
        "parts": parts.manifest(PARTS),
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
fn context_build(req: &Value, host: &Host, parts: &parts::Parts) -> Value {
    let mut req = req.clone();
    let mut replaced: Option<Value> = None;
    let link_h = host.handoff_link();
    let link_s = host.summarize_link();
    type Step<'a> = &'a dyn Fn(&Value) -> (Option<Value>, bool);
    let hand_off = |r: &Value| handoff::handle_handoff(r, Some(&link_h));
    let sum_up = |r: &Value| summarize::handle_summarize(r, Some(&link_s));
    let mut steps: Vec<Step> = Vec::new();
    if parts.on("handoff") {
        steps.push(&hand_off);
    }
    if parts.on("summarize") {
        steps.push(&sum_up);
    }
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

fn handle(req: &Value, host: &Host, parts: &parts::Parts) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest(parts),
        "prompt/context" => {
            if !parts.on("session-name") {
                return (Some(json!({ "id": id, "result": {} })), false);
            }
            let (reply, _) = call_session_name(req, host);
            return (reply, false);
        }
        "context/build" => context_build(req, host, parts),
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            match name {
                "bookmark" if parts.on("bookmark") => {
                    let (reply, _) = call_bookmark(req, host);
                    return (reply, false);
                }
                "share" if parts.on("share") => {
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
                "/bookmark" | "/bookmarks" if parts.on("bookmark") => call_bookmark(req, host),
                "/name" if parts.on("session-name") => call_session_name(req, host),
                "/share" if parts.on("share") => share::handle_share(req),
                "/handoff" if parts.on("handoff") => {
                    handoff::handle_handoff(req, Some(&host.handoff_link()))
                }
                "/summarize" if parts.on("summarize") => {
                    summarize::handle_summarize(req, Some(&host.summarize_link()))
                }
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
    let parts = parts::Parts::load("session", PARTS);
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest(&parts));
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
        let (reply, exit) = handle(&req, &host, &parts);
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
