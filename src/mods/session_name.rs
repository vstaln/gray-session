//! Ported wholesale from gray-session-name. Entry points renamed: manifest/handle/main -> *_session_name.
//! gray-session-name — human names for sessions.
//!
//! `/name <text>` gives the current session a friendly name, `/name` shows
//! it, and `/name set` asks interactively via `host/ask` (capability
//! `host.ask`) — the question is sent with an EMPTY options list so the
//! free-form notes box is the input; `answers[qid].notes` is the name
//! (first `answers` entry as fallback).
//!
//! Names live at `~/.gray/session-name/names.json` keyed by `session.id`.
//! A `prompt/context` hook surfaces the name to the model once per session
//! (the host dedups injected context, so it stays quiet after the first
//! turn).

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

/// Internal cap on `host/ask` waits — under the host's 330s outer TTL.
const ASK_TTL: Duration = Duration::from_secs(300);
const NAME_QID: &str = "session-name";

pub fn manifest_session_name() -> Value {
    json!({
        "name": "session-name",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [],
        "commands": ["/name"],
        "hooks": ["prompt/context"],
        "capabilities": ["host.ask"],
    })
}

fn names_path() -> PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("session-name").join("names.json")
}

fn load_names() -> Value {
    std::fs::read_to_string(names_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}))
}

fn save_names(names: &Value) -> std::io::Result<()> {
    let path = names_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, names.to_string())?;
    std::fs::rename(&tmp, path)
}

fn session_id(params: &Value) -> &str {
    params
        .pointer("/session/id")
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn set_name(session: &str, name: &str) -> String {
    let mut names = load_names();
    names[session] = json!(name);
    match save_names(&names) {
        Ok(()) => format!("Session named: {name}"),
        Err(e) => format!("couldn't save name: {e}"),
    }
}

/// The free-form answer to a `host/ask` question with empty options:
/// `notes` first, then the first `answers` entry as fallback.
fn ask_text(result: &Value, qid: &str) -> Option<String> {
    let e = result.get("answers")?.get(qid)?;
    if let Some(n) = e.get("notes").and_then(Value::as_str) {
        let n = n.trim();
        if !n.is_empty() {
            return Some(n.to_string());
        }
    }
    e.get("answers")?
        .as_array()?
        .first()?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `/name set` — ask for the name via `host/ask` with no options so the
/// notes box carries it. No ask channel → tell the user how to set it.
fn name_interactive(session: &str, ask: &mut dyn FnMut(&[Value]) -> Option<Value>) -> String {
    if session.is_empty() {
        return "no session id — nothing to name".into();
    }
    let questions = [json!({
        "id": NAME_QID,
        "header": "Session name",
        "question": "Name this session",
        "options": [],
    })];
    let Some(result) = ask(&questions) else {
        return "no answer channel — use /name <text> to set the name directly".into();
    };
    match ask_text(&result, NAME_QID) {
        Some(name) => set_name(session, &name),
        None => "no name given".into(),
    }
}

/// `/name …` — `argv` excludes the command name.
fn run_command(
    argv: &[&str],
    session: &str,
    ask: &mut dyn FnMut(&[Value]) -> Option<Value>,
) -> String {
    if argv == ["set"] {
        return name_interactive(session, ask);
    }
    let name = argv.join(" ");
    let name = name.trim();
    if session.is_empty() {
        return "no session id — nothing to name".into();
    }
    if name.is_empty() {
        return match load_names().get(session).and_then(Value::as_str) {
            Some(n) => format!("Session: {n}"),
            None => "No session name set".into(),
        };
    }
    set_name(session, name)
}

/// `prompt/context` — tell the model the session's name once (host dedups).
fn prompt_context(params: &Value) -> Value {
    let session = session_id(params);
    if session.is_empty() {
        return json!({});
    }
    match load_names().get(session).and_then(Value::as_str) {
        Some(name) => json!({ "text": format!("This session is named \"{name}\".") }),
        None => json!({}),
    }
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply. `ask` is the host/ask channel.
pub fn handle_session_name(req: &Value, ask: &mut dyn FnMut(&[Value]) -> Option<Value>) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_session_name(),
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            json!({ "text": run_command(&argv, session_id(&params), ask) })
        }
        "prompt/context" => prompt_context(&params),
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

pub type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;

fn next_id(counter: &Mutex<u64>) -> String {
    let mut n = counter.lock().expect("id counter");
    *n += 1;
    format!("q{n}")
}

/// `host/ask` round-trip: write the request, wait on the pending channel.
/// None on timeout or a dead channel — callers degrade to text.
fn ask_host(
    out: &Mutex<std::io::Stdout>,
    pending: &Pending,
    counter: &Mutex<u64>,
    questions: &[Value],
) -> Option<Value> {
    let id = next_id(counter);
    let (tx, rx) = mpsc::channel();
    pending.lock().expect("pending").insert(id.clone(), tx);
    let req = json!({
        "id": id,
        "method": "host/ask",
        "params": { "questions": questions, "blocking": true },
    });
    {
        let mut o = out.lock().expect("stdout");
        let _ = writeln!(o, "{req}");
        let _ = o.flush();
    }
    let reply = rx.recv_timeout(ASK_TTL).ok();
    pending.lock().expect("pending").remove(&id);
    reply
}

fn main_session_name() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_session_name());
        return Ok(());
    }
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let counter = Mutex::new(0u64);

    // Reader thread: host answers (string ids, no method) go to `pending`;
    // everything else — requests and notifications — goes to the work loop.
    let (work_tx, work_rx) = mpsc::channel::<Value>();
    let reader_pending = pending.clone();
    let _reader = std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if let Some(id) = v.get("id").and_then(Value::as_str)
                && v.get("method").is_none()
                && let Some(tx) = reader_pending.lock().expect("pending").remove(id)
            {
                let _ = tx.send(v.get("result").cloned().unwrap_or(Value::Null));
                continue;
            }
            if work_tx.send(v).is_err() {
                break;
            }
        }
    });

    for req in work_rx {
        let mut asker = |questions: &[Value]| ask_host(&stdout, &pending, &counter, questions);
        let (reply, exit) = handle_session_name(&req, &mut asker);
        if let Some(reply) = reply {
            let mut o = stdout.lock().expect("stdout");
            writeln!(o, "{reply}")?;
            o.flush()?;
        }
        if exit {
            break;
        }
    }
    // The reader thread is a detached stdin pump; returning from main
    // ends the process with it, so there is nothing to join.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_ask() -> impl FnMut(&[Value]) -> Option<Value> {
        |_: &[Value]| None
    }

    fn call(method: &str, params: Value) -> Value {
        call_with(method, params, &mut no_ask())
    }

    fn call_with(
        method: &str,
        params: Value,
        ask: &mut dyn FnMut(&[Value]) -> Option<Value>,
    ) -> Value {
        handle_session_name(&json!({ "id": 1, "method": method, "params": params }), ask)
            .0
            .unwrap()
    }

    /// Serialized: GRAY_HOME is process-global and tests share the process.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static CTR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn with_home(body: impl FnOnce(&std::path::Path)) {
        let _g = LOCK.lock().unwrap();
        let n = CTR.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("gray-session-name-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("GRAY_HOME", &dir) };
        body(&dir);
        unsafe { std::env::remove_var("GRAY_HOME") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_has_command_and_context_hook() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "session-name");
        assert_eq!(m["commands"], json!(["/name"]));
        assert_eq!(m["hooks"], json!(["prompt/context"]));
        assert_eq!(m["capabilities"], json!(["host.ask"]));
    }

    #[test]
    fn name_set_show_and_context() {
        with_home(|home| {
            let sess = json!({"id": "s-42", "cwd": "/tmp/x"});
            // Nothing set yet.
            let r = call("command/run", json!({"name": "/name", "argv": [], "session": sess}));
            assert_eq!(r["result"]["text"], "No session name set");
            assert_eq!(call("prompt/context", json!({"session": sess}))["result"], json!({}));

            // Set it.
            let r = call(
                "command/run",
                json!({"name": "/name", "argv": ["deep", "dive"], "session": sess}),
            );
            assert_eq!(r["result"]["text"], "Session named: deep dive");
            let r = call("command/run", json!({"name": "/name", "argv": [], "session": sess}));
            assert_eq!(r["result"]["text"], "Session: deep dive");

            // prompt/context surfaces it.
            let r = call("prompt/context", json!({"session": sess}));
            assert_eq!(r["result"]["text"], "This session is named \"deep dive\".");

            // Persisted keyed by session id.
            let names = load_names();
            assert_eq!(names["s-42"], "deep dive");
            let _ = home;

            // A different session sees nothing.
            let other = json!({"id": "s-99", "cwd": "/tmp/x"});
            assert_eq!(call("prompt/context", json!({"session": other}))["result"], json!({}));
        });
    }

    #[test]
    fn name_set_uses_notes_box() {
        with_home(|_| {
            let sess = json!({"id": "s-77", "cwd": "/tmp/x"});
            let mut ask = |qs: &[Value]| -> Option<Value> {
                // Empty options → the free-form notes box is the input.
                assert_eq!(qs[0]["question"], "Name this session");
                assert_eq!(qs[0]["options"], json!([]));
                Some(json!({"answers": {NAME_QID: {"answers": ["yes"], "notes": "  notes win  "}}}))
            };
            let r = call_with(
                "command/run",
                json!({"name": "/name", "argv": ["set"], "session": sess}),
                &mut ask,
            );
            assert_eq!(r["result"]["text"], "Session named: notes win");
            assert_eq!(load_names()["s-77"], "notes win");
        });
    }

    #[test]
    fn name_set_falls_back_to_answers_entry() {
        with_home(|_| {
            let sess = json!({"id": "s-78", "cwd": "/tmp/x"});
            let mut ask = |_: &[Value]| -> Option<Value> {
                Some(json!({"answers": {NAME_QID: {"answers": ["typed name"], "notes": ""}}}))
            };
            let r = call_with(
                "command/run",
                json!({"name": "/name", "argv": ["set"], "session": sess}),
                &mut ask,
            );
            assert_eq!(r["result"]["text"], "Session named: typed name");
        });
    }

    #[test]
    fn name_set_without_channel_or_empty_answer() {
        with_home(|_| {
            let sess = json!({"id": "s-79", "cwd": "/tmp/x"});
            let r = call(
                "command/run",
                json!({"name": "/name", "argv": ["set"], "session": sess}),
            );
            assert!(r["result"]["text"].as_str().unwrap().contains("no answer channel"));
            let mut empty = |_: &[Value]| -> Option<Value> {
                Some(json!({"answers": {NAME_QID: {"answers": [], "notes": ""}}}))
            };
            let r = call_with(
                "command/run",
                json!({"name": "/name", "argv": ["set"], "session": sess}),
                &mut empty,
            );
            assert_eq!(r["result"]["text"], "no name given");
            assert!(load_names().get("s-79").is_none());
        });
    }

    #[test]
    fn name_set_with_extra_args_sets_directly() {
        with_home(|_| {
            let sess = json!({"id": "s-80", "cwd": "/tmp/x"});
            let r = call(
                "command/run",
                json!({"name": "/name", "argv": ["set", "sail"], "session": sess}),
            );
            assert_eq!(r["result"]["text"], "Session named: set sail");
        });
    }

    #[test]
    fn empty_session_id_is_safe() {
        with_home(|_| {
            let r = call("command/run", json!({"name": "/name", "argv": ["x"], "session": {}}));
            assert!(r["result"]["text"].as_str().unwrap().contains("no session id"));
            assert_eq!(call("prompt/context", json!({"session": {}}))["result"], json!({}));
        });
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let (reply, exit) =
            handle_session_name(&json!({ "id": 2, "method": "plugin/shutdown" }), &mut no_ask());
        assert!(reply.is_some() && exit);
    }
}
