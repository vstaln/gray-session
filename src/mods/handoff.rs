//! Ported wholesale from gray-handoff. Entry points renamed: manifest/handle/main -> *_handoff.
//! gray-handoff — transfer context to a new focused session.
//!
//! Every `context/build` caches the serialized
//! conversation (cap ~200KB, latest wins) under `~/.gray/handoff/<sid>.txt`.
//! `/handoff <goal>` sends a handoff system prompt plus that cache and the
//! goal through `host/run` (capability `host.turn`) and hands back the
//! generated prompt as text for the user to paste into a fresh `gray`
//! session — gray's wire has no `newSession` API, so the swap is manual.
//!
//! `host/run` failures degrade to a plain-text error, never a crash.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

/// The host caps the `gray -p` child at 28s and `command/run` at 30s.
const RUN_TTL: Duration = Duration::from_secs(29);

/// Cache cap: ~200KB of serialized conversation, newest bytes win.
const CACHE_CAP: usize = 200 * 1024;
const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Prompt template sent to `host/run` with the cached history and goal.
const SYSTEM_PROMPT: &str = "You are a context transfer assistant. Given a conversation history and the user's goal for a new thread, generate a focused prompt that:

1. Summarizes relevant context from the conversation (decisions made, approaches taken, key findings)
2. Lists any relevant files that were discussed or modified
3. Clearly states the next task based on the user's goal
4. Is self-contained - the new thread should be able to proceed without the old conversation

Format your response as a prompt the user can send to start the new thread. Be concise but include all necessary context. Do not include any preamble like \"Here's the prompt\" - just output the prompt itself.

Example output format:
## Context
We've been working on X. Key decisions:
- Decision 1
- Decision 2

Files involved:
- path/to/file1.ts
- path/to/file2.ts

## Task
[Clear description of what to do next based on user's goal]";

pub fn manifest_handoff() -> Value {
    json!({
        "name": "handoff",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": [],
        "commands": ["/handoff"],
        "hooks": ["context/build"],
        "capabilities": ["host.turn"],
    })
}

// --- host link --------------------------------------------------------------

pub type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;

pub struct HostLink {
    pub out: Arc<Mutex<std::io::Stdout>>,
    pub pending: Pending,
    pub counter: Arc<Mutex<u64>>,
}

fn next_id(counter: &Arc<Mutex<u64>>) -> String {
    let mut n = counter.lock().expect("id counter");
    *n += 1;
    format!("q{n}")
}

/// `host/run` one prompt, returning the reply text. Every failure is an
/// `Err`, never a panic.
fn host_run(link: Option<&HostLink>, prompt: &str) -> Result<String, String> {
    let Some(link) = link else {
        return Err("host/run unavailable".into());
    };
    let id = next_id(&link.counter);
    let (tx, rx) = mpsc::channel();
    link.pending
        .lock()
        .expect("pending")
        .insert(id.clone(), tx);
    let req = json!({"id": id, "method": "host/run", "params": {"prompt": prompt}});
    {
        let mut o = link.out.lock().expect("stdout");
        if writeln!(o, "{req}").and_then(|_| o.flush()).is_err() {
            link.pending.lock().expect("pending").remove(&id);
            return Err("host/run: wire write failed".into());
        }
    }
    let reply = match rx.recv_timeout(RUN_TTL) {
        Ok(v) => v,
        Err(_) => {
            link.pending.lock().expect("pending").remove(&id);
            return Err("host/run did not answer in time".into());
        }
    };
    if let Some(text) = reply.pointer("/result/text").and_then(Value::as_str)
        && !text.trim().is_empty()
    {
        return Ok(text.to_string());
    }
    if let Some(e) = reply.pointer("/result/error").and_then(Value::as_str) {
        let hint = reply
            .pointer("/result/hint")
            .and_then(Value::as_str)
            .map(|h| format!(" ({h})"))
            .unwrap_or_default();
        return Err(format!("host/run: {e}{hint}"));
    }
    if let Some(e) = reply.pointer("/error/message").and_then(Value::as_str) {
        return Err(format!("host/run: {e}"));
    }
    Err("host/run: empty reply".into())
}

// --- conversation serialization ---------------------------------------------

fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    let n = text.chars().count();
    if n <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!("{kept}\n\n[... {} more characters truncated]", n - max_chars)
}

/// `[Message]` values → `[Role]: ...` text (opaque in, text out).
fn serialize_messages(messages: &[Value]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let content = msg.get("content").cloned().unwrap_or(Value::Null);
        let empty = Vec::new();
        let blocks = content.as_array().unwrap_or(&empty);
        let text = content_text(&content);
        let label = match role {
            "user" => "User",
            "assistant" => "Assistant",
            "system" => "System",
            other if !other.is_empty() => other,
            _ => continue,
        };
        if role == "assistant" {
            if !text.is_empty() {
                parts.push(format!("[Assistant]: {text}"));
            }
            let calls: Vec<String> = blocks
                .iter()
                .filter(|b| {
                    matches!(
                        b.get("type").and_then(Value::as_str),
                        Some("tool_use") | Some("toolCall")
                    )
                })
                .filter_map(|b| {
                    let name = b.get("name").and_then(Value::as_str)?;
                    let args = b
                        .get("args")
                        .or_else(|| b.get("arguments"))
                        .and_then(Value::as_object);
                    let args_str = args
                        .map(|o| {
                            o.iter()
                                .map(|(k, v)| format!("{k}={v}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    Some(format!("{name}({args_str})"))
                })
                .collect();
            if !calls.is_empty() {
                parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
            }
        } else if !text.is_empty() {
            parts.push(format!("[{label}]: {text}"));
        }
        for b in blocks {
            if matches!(
                b.get("type").and_then(Value::as_str),
                Some("tool_result") | Some("toolResult")
            ) && let Some(c) = b.get("content")
            {
                let t = match c {
                    Value::String(s) => s.clone(),
                    other => content_text(other),
                };
                if !t.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(&t, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
        }
    }
    parts.join("\n\n")
}

// --- per-session cache ------------------------------------------------------

fn state_dir() -> PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("handoff")
}

fn sid_key(sid: &str) -> String {
    let k: String = sid
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if k.is_empty() { "default".into() } else { k }
}

/// Latest serialized conversation for `sid`, capped at ~200KB (the tail
/// survives: newest context matters most for a handoff).
fn cache_write(sid: &str, text: &str) {
    let dir = state_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let bytes = text.as_bytes();
    let body = if bytes.len() > CACHE_CAP {
        // Split on a char boundary at or after the cap point.
        let mut start = bytes.len() - CACHE_CAP;
        while start < bytes.len() && !text.is_char_boundary(start) {
            start += 1;
        }
        format!("[... earlier conversation truncated ...]\n\n{}", &text[start..])
    } else {
        text.to_string()
    };
    let _ = std::fs::write(dir.join(format!("{}.txt", sid_key(sid))), body);
}

fn cache_read(sid: &str) -> Option<String> {
    let s = std::fs::read_to_string(state_dir().join(format!("{}.txt", sid_key(sid)))).ok()?;
    if s.trim().is_empty() { None } else { Some(s) }
}

// --- wire methods ------------------------------------------------------------

/// `context/build` only observes: serialize + cache, never replace.
fn on_context_build(params: &Value) -> Value {
    let empty = Vec::new();
    let messages = params
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let sid = params
        .pointer("/session/id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let text = serialize_messages(messages);
    if !text.trim().is_empty() {
        cache_write(sid, &text);
    }
    json!({})
}

fn run_command(argv: &[&str], sid: &str, link: Option<&HostLink>) -> String {
    let goal = argv.join(" ");
    let goal = goal.trim();
    if goal.is_empty() {
        return format!(
            "usage: /handoff <goal for new thread>\n\
             Generates a self-contained prompt from this conversation. Start a fresh `gray` session and paste it."
        );
    }
    let Some(history) = cache_read(sid) else {
        return "no conversation cached yet — chat a bit first, then /handoff".into();
    };
    let prompt = format!(
        "{SYSTEM_PROMPT}\n\n## Conversation History\n\n{history}\n\n## User's Goal for New Thread\n\n{goal}"
    );
    match host_run(link, &prompt) {
        Ok(generated) => format!(
            "Handoff prompt generated. Start a new session and paste:\n\n---\n{generated}"
        ),
        Err(e) => format!("handoff failed: {e}"),
    }
}

pub fn handle_handoff(req: &Value, link: Option<&HostLink>) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_handoff(),
        "context/build" => on_context_build(&params),
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let sid = params
                .pointer("/session/id")
                .and_then(Value::as_str)
                .unwrap_or("");
            json!({ "text": run_command(&argv, sid, link) })
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main_handoff() {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_handoff());
        return;
    }
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let counter = Arc::new(Mutex::new(0u64));

    let (work_tx, work_rx) = mpsc::channel::<Value>();
    let reader_pending = pending.clone();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(id) = v.get("id").and_then(|i| i.as_str())
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
        let link = HostLink {
            out: stdout.clone(),
            pending: pending.clone(),
            counter: counter.clone(),
        };
        let (reply, exit) = handle_handoff(&req, Some(&link));
        if let Some(reply) = reply {
            let mut o = stdout.lock().expect("stdout");
            let _ = writeln!(o, "{reply}");
            let _ = o.flush();
        }
        if exit {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str, params: Value) -> Value {
        handle_handoff(&json!({ "id": 1, "method": method, "params": params }), None)
            .0
            .unwrap()
    }

    #[test]
    fn manifest_claims_context_build_command_and_host_turn() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "handoff");
        assert_eq!(m["protocol"], "2.0");
        assert_eq!(m["hooks"], json!(["context/build"]));
        assert_eq!(m["commands"], json!(["/handoff"]));
        assert_eq!(m["capabilities"], json!(["host.turn"]));
        assert_eq!(m["tools"], json!([]));
    }

    #[test]
    fn context_build_keeps_the_list() {
        let r = call(
            "context/build",
            json!({
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hey"}]}],
                "session": {"id": "hb1", "cwd": "/tmp"},
            }),
        );
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn bare_handoff_shows_usage() {
        let r = call(
            "command/run",
            json!({"name": "/handoff", "argv": [], "session": {"id": "hb2"}}),
        );
        assert!(r["result"]["text"].as_str().unwrap().contains("usage: /handoff"));
    }

    #[test]
    fn handoff_without_cache_reports_empty() {
        let r = call(
            "command/run",
            json!({"name": "/handoff", "argv": ["do", "phase", "two"], "session": {"id": "no-such-sid-xyz"}}),
        );
        assert!(r["result"]["text"].as_str().unwrap().contains("no conversation cached"));
    }

    #[test]
    fn serialization_includes_roles_and_tool_calls() {
        let m = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "fix it"}]}),
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "ok"},
                {"type": "tool_use", "id": "1", "name": "edit", "args": {"file": "a.rs"}},
            ]}),
        ];
        let s = serialize_messages(&m);
        assert!(s.contains("[User]: fix it"));
        assert!(s.contains("[Assistant]: ok"));
        assert!(s.contains("[Assistant tool calls]: edit(file=\"a.rs\")"));
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let (reply, exit) = handle_handoff(&json!({ "id": 2, "method": "plugin/shutdown" }), None);
        assert!(reply.is_some() && exit);
        let (reply, exit) = handle_handoff(&json!({ "method": "plugin/shutdown" }), None);
        assert!(reply.is_none() && exit);
    }
}
