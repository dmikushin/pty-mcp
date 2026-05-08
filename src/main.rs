//! `pty-mcp` — MCP server that exposes a freeze-able PTY tool to agents.
//!
//! Protocol: JSON-RPC 2.0 over stdio. Each request/response/notification
//! is a single line of JSON terminated by `\n`. We implement enough of
//! the Model Context Protocol surface for an agent host (Claude Code /
//! Dario / etc.) to discover and invoke our tools:
//!
//!   * `initialize`               -> server info, capabilities
//!   * `notifications/initialized` -> no response (host signals ready)
//!   * `tools/list`               -> registered tools + JSON schemas
//!   * `tools/call`               -> dispatch to a tool implementation
//!
//! Tools registered:
//!   * `pty_open`        — spawn a shell command in a fresh PTY.
//!   * `pty_send`        — send keys to an open session.
//!   * `pty_screenshot`  — settle, freeze (SIGSTOP), and return the
//!                         rendered screen.
//!   * `pty_resize`      — change cols/rows.
//!   * `pty_close`       — kill and clean up.
//!   * `pty_list`        — enumerate live handles.
//!   * `pty_resume`      — explicit SIGCONT (rarely needed; `pty_send`
//!                         resumes automatically).
//!
//! Stdout is RESERVED for protocol. Diagnostics go to stderr via
//! `tracing`.

mod keys;
mod session;

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::sync::Mutex;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use session::{PtySession, ScreenshotResult, SessionInfo, TailResult};
use tracing::{error, info, warn};

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "pty-mcp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

// --- JSON-RPC envelope types -------------------------------------------------

#[derive(Deserialize, Debug)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

fn ok(id: Value, result: Value) -> JsonRpcResponse {
    JsonRpcResponse { jsonrpc: "2.0", id, result: Some(result), error: None }
}

fn err(id: Value, code: i32, msg: impl Into<String>) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(JsonRpcError { code, message: msg.into(), data: None }),
    }
}

// --- Server state ------------------------------------------------------------

struct Server {
    sessions: Mutex<HashMap<String, PtySession>>,
    next_id: Mutex<u64>,
}

impl Server {
    fn new() -> Self {
        Self { sessions: Mutex::new(HashMap::new()), next_id: Mutex::new(1) }
    }

    fn next_handle(&self) -> String {
        let mut id = self.next_id.lock().unwrap();
        let h = format!("pty-{}", *id);
        *id += 1;
        h
    }

    // -- MCP method handlers --

    fn handle_initialize(&self, _params: &Value) -> Value {
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
        })
    }

    fn handle_tools_list(&self) -> Value {
        json!({ "tools": tool_specs() })
    }

    fn handle_tools_call(&self, params: &Value) -> Result<Value, String> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing tool name".to_string())?;
        let args = params.get("arguments").cloned().unwrap_or(Value::Object(Default::default()));
        match name {
            "pty_open" => self.tool_open(&args),
            "pty_send" => self.tool_send(&args),
            "pty_screenshot" => self.tool_screenshot(&args),
            "pty_tail" => self.tool_tail(&args),
            "pty_resize" => self.tool_resize(&args),
            "pty_close" => self.tool_close(&args),
            "pty_list" => self.tool_list(),
            "pty_resume" => self.tool_resume(&args),
            other => Err(format!("unknown tool: {other}")),
        }
        .map(content_text)
    }

    // -- Tool implementations --

    fn tool_open(&self, args: &Value) -> Result<Value, String> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "command is required".to_string())?
            .to_string();
        let cols = args.get("cols").and_then(|v| v.as_u64()).unwrap_or(120) as u16;
        let rows = args.get("rows").and_then(|v| v.as_u64()).unwrap_or(40) as u16;
        let cwd = args.get("cwd").and_then(|v| v.as_str()).map(String::from);
        let freeze = args.get("freeze").and_then(|v| v.as_bool()).unwrap_or(true);
        let env: HashMap<String, String> = args
            .get("env")
            .and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        let handle = self.next_handle();
        let session = PtySession::spawn(handle.clone(), &command, cols, rows, env, cwd, freeze)
            .map_err(|e| format!("spawn failed: {e:#}"))?;
        let info = session.info();
        self.sessions.lock().unwrap().insert(handle.clone(), session);
        Ok(json!(info))
    }

    fn tool_send(&self, args: &Value) -> Result<Value, String> {
        let handle = require_handle(args)?;
        let keys_input = args
            .get("keys")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "keys is required".to_string())?;
        let bytes = keys::parse(keys_input).map_err(|e| format!("key parse error: {e:#}"))?;
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get_mut(&handle)
            .ok_or_else(|| format!("unknown handle: {handle}"))?;
        session.send(&bytes).map_err(|e| format!("send failed: {e:#}"))?;
        Ok(json!({ "ok": true, "bytes_sent": bytes.len() }))
    }

    fn tool_screenshot(&self, args: &Value) -> Result<Value, String> {
        let handle = require_handle(args)?;
        let settle_ms = args.get("settle_ms").and_then(|v| v.as_u64()).unwrap_or(200);
        let max_wait_ms = args.get("max_wait_ms").and_then(|v| v.as_u64()).unwrap_or(2000);
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get_mut(&handle)
            .ok_or_else(|| format!("unknown handle: {handle}"))?;
        let snap: ScreenshotResult = session
            .screenshot(settle_ms, max_wait_ms)
            .map_err(|e| format!("screenshot failed: {e:#}"))?;
        Ok(json!(snap))
    }

    fn tool_tail(&self, args: &Value) -> Result<Value, String> {
        let handle = require_handle(args)?;
        let sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get(&handle)
            .ok_or_else(|| format!("unknown handle: {handle}"))?;
        let tail: TailResult = session.tail();
        Ok(json!(tail))
    }

    fn tool_resize(&self, args: &Value) -> Result<Value, String> {
        let handle = require_handle(args)?;
        let cols = args
            .get("cols")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "cols required".to_string())? as u16;
        let rows = args
            .get("rows")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "rows required".to_string())? as u16;
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get_mut(&handle)
            .ok_or_else(|| format!("unknown handle: {handle}"))?;
        session.resize(cols, rows).map_err(|e| format!("resize failed: {e:#}"))?;
        Ok(json!({ "ok": true, "size": [cols, rows] }))
    }

    fn tool_close(&self, args: &Value) -> Result<Value, String> {
        let handle = require_handle(args)?;
        let mut sessions = self.sessions.lock().unwrap();
        let mut session = sessions
            .remove(&handle)
            .ok_or_else(|| format!("unknown handle: {handle}"))?;
        session.close().map_err(|e| format!("close failed: {e:#}"))?;
        Ok(json!({ "ok": true, "handle": handle }))
    }

    fn tool_list(&self) -> Result<Value, String> {
        let sessions = self.sessions.lock().unwrap();
        let infos: Vec<SessionInfo> = sessions.values().map(|s| s.info()).collect();
        Ok(json!({ "sessions": infos }))
    }

    fn tool_resume(&self, args: &Value) -> Result<Value, String> {
        let handle = require_handle(args)?;
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get_mut(&handle)
            .ok_or_else(|| format!("unknown handle: {handle}"))?;
        session.resume().map_err(|e| format!("resume failed: {e:#}"))?;
        Ok(json!({ "ok": true }))
    }
}

fn require_handle(args: &Value) -> Result<String, String> {
    args.get("handle")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| "handle is required".to_string())
}

/// Wrap a structured tool result as MCP `tools/call` content. We send
/// the JSON pretty-printed so it's readable both to the agent and in
/// transcripts.
fn content_text(value: Value) -> Value {
    let pretty = serde_json::to_string_pretty(&value)
        .unwrap_or_else(|_| value.to_string());
    json!({
        "content": [{ "type": "text", "text": pretty }],
        "isError": false,
    })
}

fn content_error(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}

// --- Tool descriptors --------------------------------------------------------

fn tool_specs() -> Vec<Value> {
    vec![
        json!({
            "name": "pty_open",
            "description": "Spawn a command in a fresh PTY. Returns a handle used by all other pty_* tools.\n\nWhen `freeze` is true (default), the child is automatically SIGSTOP'd between turns so the agent can read state without racing the process. Disable for network-bound work where remote keepalives matter.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command (passed to `sh -c`)." },
                    "cols": { "type": "integer", "default": 120 },
                    "rows": { "type": "integer", "default": 40 },
                    "cwd": { "type": "string", "description": "Working directory" },
                    "env": { "type": "object", "additionalProperties": { "type": "string" } },
                    "freeze": { "type": "boolean", "default": true,
                        "description": "If true, SIGSTOP the child after each settled screenshot. Disable for network-bound or real-time-sensitive commands." }
                },
                "required": ["command"]
            }
        }),
        json!({
            "name": "pty_send",
            "description": "Send keys to a PTY session. Literal text is passed through; named escapes use angle brackets:\n  <Enter> <CR> <Tab> <Esc> <Backspace> <Space>\n  <C-c> <C-z> <C-d> ... (Ctrl + letter)\n  <Up> <Down> <Left> <Right>\n  <Home> <End> <PageUp> <PageDown> <Delete> <Insert>\n  <F1>..<F12>\n\nIf the session is currently SIGSTOP'd (frozen by a previous screenshot), it's automatically resumed before the keys are written.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "handle": { "type": "string" },
                    "keys": { "type": "string" }
                },
                "required": ["handle", "keys"]
            }
        }),
        json!({
            "name": "pty_screenshot",
            "description": "Wait for the PTY to settle (no output for `settle_ms` ms, capped at `max_wait_ms`), then SIGSTOP the child and return the rendered terminal screen plus the raw bytes captured since the last screenshot. Cursor position, alternate-screen flag, and process state are also returned.\n\nThis is the primary read tool — call it after every `pty_send` to see what happened, and again to confirm the state hasn't changed if you're unsure whether the child is waiting for input.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "handle": { "type": "string" },
                    "settle_ms": { "type": "integer", "default": 200,
                        "description": "Quiet period required before the screenshot is taken." },
                    "max_wait_ms": { "type": "integer", "default": 2000,
                        "description": "Hard cap on total settle wait. After this elapses, the screenshot is taken regardless of activity (useful for `top`-style continuously-redrawing programs)." }
                },
                "required": ["handle"]
            }
        }),
        json!({
            "name": "pty_tail",
            "description": "Drain the raw output buffer WITHOUT freezing or rendering. Returns whatever bytes the reader has accumulated since the last `tail` or `screenshot` call.\n\nUse this for live-monitoring long commands (`make`, `pytest`, `cargo build`) where the agent wants incremental output without disturbing the process. State (running/paused/exited) is unchanged — call `pty_screenshot` afterwards if you need the rendered screen.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "handle": { "type": "string" }
                },
                "required": ["handle"]
            }
        }),
        json!({
            "name": "pty_resize",
            "description": "Change the PTY's window size. Most TUIs handle SIGWINCH and redraw.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "handle": { "type": "string" },
                    "cols": { "type": "integer" },
                    "rows": { "type": "integer" }
                },
                "required": ["handle", "cols", "rows"]
            }
        }),
        json!({
            "name": "pty_close",
            "description": "Resume (if frozen), SIGTERM, then SIGKILL after a 1s grace. Removes the handle.",
            "inputSchema": {
                "type": "object",
                "properties": { "handle": { "type": "string" } },
                "required": ["handle"]
            }
        }),
        json!({
            "name": "pty_list",
            "description": "List every live PTY session this server owns.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "pty_resume",
            "description": "Explicit SIGCONT — rarely needed. `pty_send` already resumes automatically.",
            "inputSchema": {
                "type": "object",
                "properties": { "handle": { "type": "string" } },
                "required": ["handle"]
            }
        }),
    ]
}

// --- stdio loop --------------------------------------------------------------

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("PTY_MCP_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!("pty-mcp v{} starting", SERVER_VERSION);
    let server = Server::new();

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let mut stdin = stdin.lock();

    let mut line = String::new();
    loop {
        line.clear();
        match stdin.read_line(&mut line) {
            Ok(0) => {
                info!("stdin closed; exiting");
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                error!("stdin read error: {e}");
                return Err(e.into());
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(response) = handle_message(&server, trimmed) else { continue };
        let serialized = serde_json::to_string(&response).context("response serialize")?;
        writeln!(stdout, "{serialized}")?;
        stdout.flush()?;
    }
}

/// Process one JSON-RPC message. Returns `Some(response)` for requests
/// (those with an `id`); returns `None` for notifications.
fn handle_message(server: &Server, line: &str) -> Option<JsonRpcResponse> {
    let req: JsonRpcRequest = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            warn!("invalid JSON-RPC: {e} — line was: {}", &line[..line.len().min(200)]);
            return Some(err(Value::Null, -32700, format!("parse error: {e}")));
        }
    };
    if req.jsonrpc != "2.0" {
        return Some(err(req.id.unwrap_or(Value::Null), -32600, "jsonrpc must be 2.0"));
    }

    let is_notification = req.id.is_none();

    let response = match req.method.as_str() {
        "initialize" => Ok(server.handle_initialize(&req.params)),
        "notifications/initialized" => return None,
        "tools/list" => Ok(server.handle_tools_list()),
        "tools/call" => match server.handle_tools_call(&req.params) {
            Ok(v) => Ok(v),
            Err(message) => Ok(content_error(&message)),
        },
        "ping" => Ok(json!({})),
        other => Err((-32601, format!("method not found: {other}"))),
    };

    if is_notification {
        return None;
    }
    let id = req.id.unwrap_or(Value::Null);
    Some(match response {
        Ok(v) => ok(id, v),
        Err((code, message)) => err(id, code, message),
    })
}
