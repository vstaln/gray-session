//! Ported wholesale from gray-summarize. Entry points renamed: manifest/handle/main -> *_summarize.
//! gray-summarize — summarize the current conversation on demand.
//!
//! Port of pi's `summarize.ts`. Every `context/build` caches the serialized
//! conversation (cap ~200KB, latest wins) under `~/.gray/summarize/<sid>.txt`.
//! `/summarize [focus]` sends pi's summarize prompt — plus the optional
//! focus — through `host/run` (capability `host.turn`) and replies with the
//! summary as text. With nothing cached it says so; `host/run` failures
//! degrade to a plain-text error, never a crash.

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

pub fn manifest_summarize() -> Value {
    json!({
        "name": "summarize",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": [],
        "commands": ["/summarize"],
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
    link.pending.lock().expect("pending").insert(id.clone(), tx);
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
    format!(
        "{kept}\n\n[... {} more characters truncated]",
        n - max_chars
    )
}

/// `[Message]` values → `[Role]: ...` text; pi's summarize.ts emits
/// `User: ...` / `Assistant: ...` and `Tool <name> was called with args <json>`
/// — this port keeps that shape for the cache (it's what the model sees).
fn serialize_messages(messages: &[Value]) -> String {
    let mut sections: Vec<String> = Vec::new();
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
        let mut lines: Vec<String> = Vec::new();
        if !text.is_empty() {
            lines.push(format!("{label}: {}", text.trim()));
        }
        if role == "assistant" {
            for b in blocks {
                if matches!(
                    b.get("type").and_then(Value::as_str),
                    Some("tool_use") | Some("toolCall")
                ) && let Some(name) = b.get("name").and_then(Value::as_str)
                {
                    let args = b
                        .get("args")
                        .or_else(|| b.get("arguments"))
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    lines.push(format!("Tool {name} was called with args {args}"));
                }
            }
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
                    lines.push(format!(
                        "Tool result: {}",
                        truncate_for_summary(&t, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
        }
        if !lines.is_empty() {
            sections.push(lines.join("\n"));
        }
    }
    sections.join("\n\n")
}

/// pi summarize.ts's prompt, with the spec's optional `<: focus>` slot.
fn summary_prompt(conversation_text: &str, focus: &str) -> String {
    let focus = focus.trim();
    let focus_clause = if focus.is_empty() {
        String::new()
    } else {
        format!(" Focus on: {focus}.")
    };
    format!(
        "Summarize this conversation so I can resume it later.{focus_clause}\n\
         Include goals, key decisions, progress, open questions, and next steps.\n\
         Keep it concise and structured with headings.\n\n\
         <conversation>\n{conversation_text}\n</conversation>"
    )
}

// --- per-session cache ------------------------------------------------------

fn state_dir() -> PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("summarize")
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

fn cache_write(sid: &str, text: &str) {
    let dir = state_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let bytes = text.as_bytes();
    let body = if bytes.len() > CACHE_CAP {
        let mut start = bytes.len() - CACHE_CAP;
        while start < bytes.len() && !text.is_char_boundary(start) {
            start += 1;
        }
        format!(
            "[... earlier conversation truncated ...]\n\n{}",
            &text[start..]
        )
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
    let focus = argv.join(" ");
    let Some(history) = cache_read(sid) else {
        return "nothing to summarize yet".into();
    };
    match host_run(link, &summary_prompt(&history, &focus)) {
        Ok(summary) => summary,
        Err(e) => format!("summarize failed: {e}"),
    }
}

pub fn handle_summarize(req: &Value, link: Option<&HostLink>) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_summarize(),
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

fn main_summarize() {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_summarize());
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
        let (reply, exit) = handle_summarize(&req, Some(&link));
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
        handle_summarize(
            &json!({ "id": 1, "method": method, "params": params }),
            None,
        )
        .0
        .unwrap()
    }

    #[test]
    fn manifest_claims_context_build_command_and_host_turn() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "summarize");
        assert_eq!(m["protocol"], "2.0");
        assert_eq!(m["hooks"], json!(["context/build"]));
        assert_eq!(m["commands"], json!(["/summarize"]));
        assert_eq!(m["capabilities"], json!(["host.turn"]));
        assert_eq!(m["tools"], json!([]));
    }

    #[test]
    fn context_build_keeps_the_list() {
        let r = call(
            "context/build",
            json!({
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hey"}]}],
                "session": {"id": "sb1", "cwd": "/tmp"},
            }),
        );
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn summarize_without_cache_reports_empty() {
        let r = call(
            "command/run",
            json!({"name": "/summarize", "argv": [], "session": {"id": "no-such-sid-abc"}}),
        );
        assert_eq!(r["result"]["text"], "nothing to summarize yet");
    }

    #[test]
    fn serialization_matches_pi_summarize_shape() {
        let m = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "build it"}]}),
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "on it"},
                {"type": "tool_use", "id": "1", "name": "edit", "args": {"file": "a.rs"}},
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "id": "1", "content": "done", "is_error": false},
            ]}),
        ];
        let s = serialize_messages(&m);
        assert!(s.contains("User: build it"));
        assert!(s.contains("Assistant: on it"));
        assert!(s.contains("Tool edit was called with args {\"file\":\"a.rs\"}"));
        assert!(s.contains("Tool result: done"));
    }

    #[test]
    fn focus_slot_joins_the_prompt() {
        let p = summary_prompt("the cache", "the open questions");
        assert!(p.contains("Focus on: the open questions."));
        assert!(p.contains("<conversation>\nthe cache\n</conversation>"));
        let bare = summary_prompt("x", "");
        assert!(!bare.contains("Focus on"));
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let (reply, exit) =
            handle_summarize(&json!({ "id": 2, "method": "plugin/shutdown" }), None);
        assert!(reply.is_some() && exit);
        let (reply, exit) = handle_summarize(&json!({ "method": "plugin/shutdown" }), None);
        assert!(reply.is_none() && exit);
    }
}
