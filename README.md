<div align="center">
  <img alt="gray-session" src="assets/icon.svg" width="120" height="120" />
  <h1>gray-session</h1>
  <p><strong>Session utilities in one sidecar: bookmarks, friendly names, HTML export, handoff prompts, summarize.</strong></p>
  <p>
    <a href="https://gray.alignment.id">Website</a> ·
    <a href="https://github.com/vstaln/gray-session">Source</a> ·
    <a href="https://github.com/vstaln/gray">gray</a>
  </p>
  <p>
    <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-1c1c20?style=flat-square&labelColor=0a0a0b" /></a>
    <a href="https://www.rust-lang.org"><img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-rust-1c1c20?style=flat-square&labelColor=0a0a0b&logo=rust&logoColor=d4a373" /></a>
    <img alt="gray plugin" src="https://img.shields.io/badge/gray-plugin-1c1c20?style=flat-square&labelColor=0a0a0b&color=7aa2f7" />
  </p>
</div>

<br/>

```bash
gray plugin install session
```

`gray-session` merges five session-utility plugins into a single sidecar
process. Each module keeps its own state files, tools and slash commands, so
existing setups keep working after you uninstall the standalone versions.

## Replaces

- `gray-bookmark`
- `gray-session-name`
- `gray-share`
- `gray-handoff`
- `gray-summarize`

Every module reads and writes the same paths as the plugin it replaces
(under `$GRAY_HOME`, default `~/.gray/`):

| Module | State |
|---|---|
| bookmark | `~/.gray/bookmarks.jsonl` |
| session_name | `~/.gray/session-name/names.json` |
| share | reads `~/.gray/sessions/`, writes `~/.gray/share/<sid>-<ts>.html` |
| handoff | `~/.gray/handoff/<sid>.txt` (conversation cache) |
| summarize | `~/.gray/summarize/<sid>.txt` (conversation cache) |

## Modules

| Module | Former plugin | Tools | Commands | Hooks | Host calls |
|---|---|---|---|---|---|
| bookmark | `gray-bookmark` | `bookmark` | `/bookmark` `/bookmarks` | — | `host/ask` (jump picker) |
| session_name | `gray-session-name` | — | `/name` | `prompt/context` | `host/ask` (`/name set`) |
| share | `gray-share` | `share` | `/share` | — | — |
| handoff | `gray-handoff` | — | `/handoff` | `context/build` | `host/run` |
| summarize | `gray-summarize` | — | `/summarize` | `context/build` | `host/run` |

`context/build` is chained through handoff and then summarize. Both only
observe and cache the conversation, so the hook returns `{}`. `/session`
prints an overview of all five modules. The manifest declares the union of
capabilities: `host.ask` and `host.turn`.

## Build from source

```sh
cargo build --release
gray plugin check ./target/release/session
```

## License

MIT
