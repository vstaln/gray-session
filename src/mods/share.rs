//! Ported wholesale from gray-share. Entry points renamed: manifest/handle/main -> *_share.
//! gray-share — export a session transcript to one self-contained HTML file.
//!
//! The `share` tool {session?, out?} and the `/share [sid]` command find a
//! transcript under `~/.gray/sessions/` (`<sid>*.jsonl`, prefix match;
//! default is the current session or the newest file) and render it to
//! `~/.gray/share/<sid>-<ts>.html` — dark theme, monospace, role-colored
//! user/assistant/tool entries, tool args in collapsible `<details>`, no
//! external assets. `/share list` shows existing exports. The transcript
//! file is only ever read, never written.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub fn manifest_share() -> Value {
    json!({
        "name": "share",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [{
            "name": "share",
            "description": "Export a session transcript to a self-contained dark HTML page (role-colored, tool args collapsible) in ~/.gray/share/, or 'out'. 'session' takes a session id or unique prefix; default is the current session.",
            "parameters": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session id or unique prefix; default is the current session." },
                    "out": { "type": "string", "description": "Output HTML path; default ~/.gray/share/<sid>-<unix-ts>.html." }
                }
            }
        }],
        "commands": ["/share"],
    })
}

/// `~/.gray`, honoring `GRAY_HOME`.
fn gray_home() -> PathBuf {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn sessions_dir() -> PathBuf {
    gray_home().join("sessions")
}

fn exports_dir() -> PathBuf {
    gray_home().join("share")
}

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(home) = std::env::home_dir() {
        if p == "~" {
            return home;
        }
        if let Some(rest) = p.strip_prefix("~/") {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Every `<dir>/<sid-prefix>*.jsonl` match, sorted.
fn transcript_matches(dir: &Path, sid: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.extension().is_some_and(|x| x == "jsonl")
                        && p.file_stem()
                            .and_then(|s| s.to_str())
                            .is_some_and(|s| s.starts_with(sid))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Resolve `sid` (or none → newest) to one transcript file in `dir`.
fn find_transcript_in(dir: &Path, sid: Option<&str>) -> Result<PathBuf, String> {
    let Some(sid) = sid.map(str::trim).filter(|s| !s.is_empty()) else {
        // No sid: newest transcript by mtime.
        let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "jsonl") {
                    if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                        if best.as_ref().is_none_or(|(_, t)| m > *t) {
                            best = Some((p, m));
                        }
                    }
                }
            }
        }
        return best
            .map(|(p, _)| p)
            .ok_or_else(|| format!("no transcripts under {}", dir.display()));
    };
    let exact = dir.join(format!("{sid}.jsonl"));
    if exact.is_file() {
        return Ok(exact);
    }
    let matches = transcript_matches(dir, sid);
    match matches.len() {
        0 => Err(format!(
            "no session matching '{sid}' under {}",
            dir.display()
        )),
        1 => Ok(matches.into_iter().next().unwrap()),
        n => Err(format!(
            "'{sid}' is ambiguous — {n} matches ({})",
            matches
                .iter()
                .take(4)
                .filter_map(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// millis-since-epoch → "YYYY-MM-DD HH:MM:SSZ" (UTC, no chrono).
fn fmt_ts(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}Z")
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A text block: escape, newlines → <br>.
fn text_html(s: &str) -> String {
    esc(s).replace('\n', "<br>\n")
}

/// One content block → its HTML fragment.
fn block_html(b: &Value) -> String {
    match b.get("type").and_then(Value::as_str) {
        Some("text") => {
            let t = b.get("text").and_then(Value::as_str).unwrap_or("");
            format!("<div class=\"text\">{}</div>", text_html(t))
        }
        Some("thinking") => {
            let t = b.get("text").and_then(Value::as_str).unwrap_or("");
            format!(
                "<details class=\"thinking\"><summary>thinking</summary><pre>{}</pre></details>",
                esc(t)
            )
        }
        Some("tool_use") => {
            let name = b.get("name").and_then(Value::as_str).unwrap_or("?");
            let args = b.get("args").cloned().unwrap_or(Value::Null);
            let pretty = serde_json::to_string_pretty(&args).unwrap_or_else(|_| args.to_string());
            format!(
                "<div class=\"tool use\"><span class=\"tname\">{}</span>\
                 <details><summary>args</summary><pre>{}</pre></details></div>",
                esc(name),
                esc(&pretty)
            )
        }
        Some("tool_result") => {
            let body = match b.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()),
                None => String::new(),
            };
            let chars = body.chars().count();
            format!(
                "<details class=\"tool result\"><summary>result ({} chars)</summary><pre>{}</pre></details>",
                chars,
                esc(&body)
            )
        }
        Some(other) => {
            format!("<div class=\"other\">[{}]</div>", esc(other))
        }
        None => String::new(),
    }
}

/// The transcript file → (sid, full HTML page). Read-only.
fn render(path: &Path) -> Result<(String, String), String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut lines = text.lines();
    let header: Value = lines
        .next()
        .and_then(|l| serde_json::from_str(l).ok())
        .ok_or("transcript has no header line")?;
    let sid = header
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| path.file_stem().and_then(|s| s.to_str()))
        .unwrap_or("session")
        .to_string();
    let started = header.get("timestamp").and_then(Value::as_u64).unwrap_or(0);
    let cwd = header.get("cwd").and_then(Value::as_str).unwrap_or("");
    let model = header.get("model").and_then(Value::as_str).unwrap_or("");

    let mut body = String::new();
    for line in lines {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(msg) = entry.get("message") else {
            continue;
        };
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("?");
        let ts = entry.get("timestamp").and_then(Value::as_u64).unwrap_or(0);
        let clock = fmt_ts(ts);
        let clock = clock
            .rsplit_once(' ')
            .map(|(_, t)| t)
            .unwrap_or("")
            .to_string();
        let mut blocks = String::new();
        if let Some(content) = msg.get("content").and_then(Value::as_array) {
            for b in content {
                blocks.push_str(&block_html(b));
            }
        }
        if blocks.is_empty() {
            continue;
        }
        body.push_str(&format!(
            "<div class=\"entry {}\"><div class=\"meta\"><span class=\"role\">{}</span>\
             <time>{}</time></div>{}</div>\n",
            esc(role),
            esc(role),
            esc(&clock),
            blocks
        ));
    }
    if body.is_empty() {
        return Err("transcript has no renderable entries".into());
    }

    let html = format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>gray session {sid}</title>
<style>
:root {{ color-scheme: dark; }}
body {{ background: #0d1117; color: #c9d1d9; margin: 0;
  font: 14px/1.55 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }}
.page {{ max-width: 880px; margin: 0 auto; padding: 24px 16px 64px; }}
header {{ border-bottom: 1px solid #21262d; padding-bottom: 12px; margin-bottom: 20px; }}
h1 {{ font-size: 16px; margin: 0 0 6px; color: #e6edf3; word-break: break-all; }}
.meta {{ color: #8b949e; font-size: 12px; }}
.entry {{ margin: 18px 0; }}
.entry > .meta {{ display: flex; gap: 10px; margin-bottom: 4px; }}
.role {{ font-weight: 700; text-transform: uppercase; font-size: 11px; letter-spacing: .08em; }}
time {{ color: #6e7681; font-size: 11px; align-self: center; }}
.user .role {{ color: #7ee787; }}
.assistant .role {{ color: #79c0ff; }}
.system .role {{ color: #d29922; }}
.text {{ white-space: normal; word-break: break-word; }}
pre {{ background: #161b22; border: 1px solid #21262d; border-radius: 6px;
  padding: 10px 12px; overflow-x: auto; white-space: pre-wrap; word-break: break-word;
  margin: 6px 0; font-size: 12.5px; }}
details {{ margin: 6px 0; }}
summary {{ cursor: pointer; color: #8b949e; font-size: 12px; }}
summary:hover {{ color: #c9d1d9; }}
.tool {{ border-left: 2px solid #30363d; padding-left: 10px; margin: 8px 0; }}
.tname {{ color: #d2a8ff; font-weight: 600; }}
.thinking summary {{ color: #6e7681; font-style: italic; }}
.other {{ color: #6e7681; font-size: 12px; }}
a {{ color: #79c0ff; }}
</style></head>
<body><div class="page">
<header>
<h1>gray session <span class="sid">{sid}</span></h1>
<div class="meta">started {started} · cwd <span>{cwd}</span> · model <span>{model}</span></div>
</header>
{body}</div></body></html>
"#,
        sid = esc(&sid),
        started = esc(&fmt_ts(started)),
        cwd = esc(cwd),
        model = esc(model),
        body = body,
    );
    Ok((sid, html))
}

/// Resolve + render + write; returns the one-line confirmation.
fn do_share(session: Option<&str>, out: Option<&str>) -> Result<String, String> {
    let src = find_transcript_in(&sessions_dir(), session)?;
    let (sid, html) = render(&src)?;
    let dest = match out.map(str::trim).filter(|o| !o.is_empty()) {
        Some(o) => expand_tilde(o),
        None => exports_dir().join(format!("{sid}-{}.html", unix_secs())),
    };
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("mkdir {}: {e}", d.display()))?;
    }
    std::fs::write(&dest, html.as_bytes()).map_err(|e| format!("write {}: {e}", dest.display()))?;
    let kb = html.len().div_ceil(1024);
    Ok(format!("wrote {} ({kb} KB)", dest.display()))
}

fn list_exports() -> String {
    let dir = exports_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "html"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    if files.is_empty() {
        return format!("no exports yet — /share writes to {}", dir.display());
    }
    let mut lines = vec![format!("{} export(s) in {}:", files.len(), dir.display())];
    for f in files {
        lines.push(format!("- {}", f.display()));
    }
    lines.join("\n")
}

/// A tool call. `session` is the caller's session object when the host
/// sends one (used as the default sid).
fn call_tool(name: &str, args: &Value, session: Option<&Value>) -> Result<String, String> {
    match name {
        "share" => {
            let sid = args
                .get("session")
                .and_then(Value::as_str)
                .or_else(|| session.and_then(|s| s.get("id")).and_then(Value::as_str));
            do_share(sid, args.get("out").and_then(Value::as_str))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// `/share …` — `argv` excludes the name; `session` is the live session.
fn run_command(argv: &[&str], session: Option<&Value>) -> String {
    match argv.first().copied() {
        Some("list") => list_exports(),
        Some("help") => "usage: /share [session-id|prefix] · /share list".into(),
        Some(sid) => match do_share(Some(sid), None) {
            Ok(m) => m,
            Err(e) => format!("share: {e}"),
        },
        None => {
            let sid = session.and_then(|s| s.get("id")).and_then(Value::as_str);
            match do_share(sid, None) {
                Ok(m) => m,
                Err(e) => format!("share: {e}"),
            }
        }
    }
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_share(req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_share(),
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("args").cloned().unwrap_or(Value::Null);
            match call_tool(name, &args, params.get("session")) {
                Ok(text) => json!({ "content": text }),
                Err(text) => json!({ "content": text, "is_error": true }),
            }
        }
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            json!({ "text": run_command(&argv, params.get("session")) })
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main_share() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_share());
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (reply, exit) = handle_share(&req);
        if let Some(reply) = reply {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
        if exit {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str, params: Value) -> Value {
        handle_share(&json!({ "id": 1, "method": method, "params": params }))
            .0
            .unwrap()
    }

    /// A throwaway transcript file inside a unique temp dir.
    fn fake_transcript(dir: &Path, sid: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(format!("{sid}.jsonl"));
        std::fs::write(
            &p,
            concat!(
                "{\"version\":1,\"id\":\"SID\",\"timestamp\":1788439169115,",
                "\"cwd\":\"/tmp/x\",\"model\":\"m1\"}\n",
                "{\"entry_id\":0,\"parent_id\":null,\"timestamp\":1788439169119,",
                "\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi <b>\"}]}}\n",
                "{\"entry_id\":1,\"parent_id\":0,\"timestamp\":1788439169121,",
                "\"message\":{\"role\":\"assistant\",\"content\":[",
                "{\"type\":\"text\",\"text\":\"calling a tool\"},",
                "{\"type\":\"tool_use\",\"id\":\"c1\",\"name\":\"bash\",\"args\":{\"command\":\"ls\"}}]}}\n",
                "{\"entry_id\":2,\"parent_id\":1,\"timestamp\":1788439169122,",
                "\"message\":{\"role\":\"user\",\"content\":[",
                "{\"type\":\"tool_result\",\"id\":\"c1\",\"content\":\"ok output\"}]}}\n",
            )
            .replace("SID", sid),
        )
        .unwrap();
        p
    }

    #[test]
    fn manifest_names_tool_and_command() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "share");
        assert_eq!(m["tools"][0]["name"], "share");
        assert_eq!(m["commands"], json!(["/share"]));
    }

    #[test]
    fn html_escapes_markup() {
        assert_eq!(esc("<a>&\"</a>"), "&lt;a&gt;&amp;&quot;&lt;/a&gt;");
    }

    #[test]
    fn civil_dates_format() {
        assert_eq!(fmt_ts(1788439169115), "2026-09-03 12:39:29Z");
        assert_eq!(fmt_ts(0), "1970-01-01 00:00:00Z");
    }

    #[test]
    fn find_transcript_resolves_exact_prefix_and_newest() {
        let dir = std::env::temp_dir().join(format!("gray-share-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        fake_transcript(&dir, "abc111");
        fake_transcript(&dir, "abc222");
        assert_eq!(
            find_transcript_in(&dir, Some("abc111"))
                .unwrap()
                .file_name()
                .unwrap(),
            "abc111.jsonl"
        );
        assert!(find_transcript_in(&dir, Some("abc")).is_err()); // ambiguous
        assert!(find_transcript_in(&dir, Some("zzz")).is_err()); // none
        assert!(find_transcript_in(&dir, None).is_ok()); // newest
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_produces_role_colored_collapsible_html() {
        let dir = std::env::temp_dir().join(format!("gray-share-render-{}", std::process::id()));
        let p = fake_transcript(&dir, "t-000");
        let (sid, html) = render(&p).unwrap();
        assert_eq!(sid, "t-000");
        assert!(html.contains("entry user"), "{html}");
        assert!(html.contains("entry assistant"), "{html}");
        assert!(html.contains("hi &lt;b&gt;"), "{html}");
        assert!(html.contains("tname\">bash"), "{html}");
        assert!(html.contains("<details"), "{html}");
        assert!(html.contains("result (9 chars)"), "{html}");
        assert!(html.contains("model"), "{html}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn share_tool_errors_on_unknown_session() {
        let r = call(
            "tool/call",
            json!({ "name": "share", "args": { "session": "no-such-sid-xyz" } }),
        );
        assert_eq!(r["result"]["is_error"], true);
        assert!(
            r["result"]["content"]
                .as_str()
                .unwrap()
                .contains("no session matching")
        );
    }

    #[test]
    fn unknown_methods_are_method_not_found() {
        assert_eq!(call("nope", Value::Null)["error"]["code"], -32601);
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let (reply, exit) = handle_share(&json!({ "id": 2, "method": "plugin/shutdown" }));
        assert!(reply.is_some() && exit);
    }
}
