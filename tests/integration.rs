//! Integration test that drives the compiled `pty-mcp` binary as a
//! subprocess via JSON-RPC stdio — exercising the full protocol path
//! (spawn, send, screenshot, close) and the SIGSTOP/SIGCONT freeze
//! behavior against real PTY-attached commands.
//!
//! Each test starts a fresh `pty-mcp` child, sends the standard MCP
//! handshake, then runs scenario-specific tool calls.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Server {
    fn start() -> Self {
        let bin = env!("CARGO_BIN_EXE_pty-mcp");
        let mut child = Command::new(bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn pty-mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut s = Self { child, stdin, stdout, next_id: 1 };
        s.handshake();
        s
    }

    fn handshake(&mut self) {
        let _ = self.request("initialize", json!({}));
        // notification — no response
        self.notify("notifications/initialized", json!({}));
    }

    fn notify(&mut self, method: &str, params: Value) {
        let msg = json!({"jsonrpc":"2.0","method":method,"params":params});
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("bad JSON-RPC response: {e}\n{line}"));
        assert_eq!(resp["id"], id);
        resp["result"].clone()
    }

    /// Call a tool and unwrap the inner JSON payload. We pretty-print
    /// JSON in `tools/call` content, so we re-parse it here to the
    /// structured value.
    fn call_tool(&mut self, name: &str, args: Value) -> Value {
        let result = self.request("tools/call", json!({"name": name, "arguments": args}));
        let is_error = result.get("isError").and_then(|v| v.as_bool()).unwrap_or(false);
        let text = result["content"][0]["text"].as_str().unwrap().to_string();
        if is_error {
            panic!("{name} returned isError: {text}");
        }
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("tool {name} returned non-JSON content: {e}\n{text}"))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn echo_command_renders_through_vt100() {
    let mut s = Server::start();
    let opened = s.call_tool("pty_open", json!({
        "command": "echo hello; echo world",
        "cols": 80,
        "rows": 24
    }));
    let handle = opened["handle"].as_str().unwrap().to_string();

    let snap = s.call_tool("pty_screenshot", json!({
        "handle": &handle,
        "settle_ms": 300,
        "max_wait_ms": 2000
    }));
    let screen = snap["screen"].as_str().unwrap();
    assert!(screen.contains("hello"), "screen missing 'hello': {screen:?}");
    assert!(screen.contains("world"), "screen missing 'world': {screen:?}");
    assert_eq!(snap["state"], "exited");
    assert_eq!(snap["exit_code"], 0);

    s.call_tool("pty_close", json!({ "handle": &handle }));
}

#[test]
fn interactive_bash_session_round_trips() {
    let mut s = Server::start();
    let opened = s.call_tool("pty_open", json!({
        "command": "bash --noprofile --norc",
        "cols": 80,
        "rows": 24
    }));
    let handle = opened["handle"].as_str().unwrap().to_string();

    // Wait for the prompt. bash --norc shows `bash-X.Y$ ` (or `#` for
    // root) — assert on the trailing `$ ` / `# ` rather than a specific
    // text so the test isn't brittle to bash version.
    let snap = s.call_tool("pty_screenshot", json!({
        "handle": &handle,
        "settle_ms": 200,
        "max_wait_ms": 1500
    }));
    let screen = snap["screen"].as_str().unwrap();
    assert!(
        screen.contains("$ ") || screen.contains("# "),
        "no shell prompt visible: {screen:?}"
    );
    assert_eq!(snap["state"], "paused", "should freeze after screenshot");

    // Send a command. send auto-resumes the SIGSTOP'd shell.
    s.call_tool("pty_send", json!({
        "handle": &handle,
        "keys": "uname -s<Enter>"
    }));

    let snap2 = s.call_tool("pty_screenshot", json!({
        "handle": &handle,
        "settle_ms": 200,
        "max_wait_ms": 1500
    }));
    let screen2 = snap2["screen"].as_str().unwrap();
    assert!(
        screen2.contains("Linux") || screen2.contains("Darwin"),
        "uname output not visible: {screen2:?}"
    );

    s.call_tool("pty_close", json!({ "handle": &handle }));
}

#[test]
fn freeze_actually_pauses_the_child() {
    // Run a tight loop that emits "tick" forever. A correctly frozen
    // session has the same screen contents on two screenshots taken
    // half a second apart (because SIGSTOP between them prevents new
    // output). Without freeze, output keeps growing.

    let mut s = Server::start();
    let opened = s.call_tool("pty_open", json!({
        "command": "while true; do echo tick; sleep 0.05; done",
        "cols": 80,
        "rows": 24,
        "freeze": true
    }));
    let handle = opened["handle"].as_str().unwrap().to_string();

    let snap1 = s.call_tool("pty_screenshot", json!({
        "handle": &handle,
        "settle_ms": 100,
        "max_wait_ms": 500
    }));
    assert_eq!(snap1["state"], "paused", "should be SIGSTOP'd after screenshot");

    // Sleep WITHOUT calling resume. While we sleep, the child must
    // stay paused — no new ticks should accumulate.
    std::thread::sleep(Duration::from_millis(700));

    let snap2 = s.call_tool("pty_screenshot", json!({
        "handle": &handle,
        "settle_ms": 100,
        "max_wait_ms": 500
    }));
    let raw_during_pause = snap2["raw_since_last"].as_str().unwrap();
    assert!(
        raw_during_pause.is_empty(),
        "expected no output while paused, got {} bytes: {:?}",
        raw_during_pause.len(),
        &raw_during_pause[..raw_during_pause.len().min(120)]
    );

    s.call_tool("pty_close", json!({ "handle": &handle }));
}

#[test]
fn close_kills_runaway_loop() {
    let mut s = Server::start();
    let opened = s.call_tool("pty_open", json!({
        "command": "while true; do :; done",
        "freeze": false
    }));
    let handle = opened["handle"].as_str().unwrap().to_string();

    s.call_tool("pty_close", json!({ "handle": &handle }));

    let listed = s.call_tool("pty_list", json!({}));
    let sessions = listed["sessions"].as_array().unwrap();
    assert!(sessions.is_empty(), "session should be removed after close");
}

#[test]
fn tail_streams_without_freezing() {
    // `pty_tail` must drain accumulated output without SIGSTOP-ing the
    // child. Verify by tailing twice with a sleep in between against
    // a `tick` loop and observing that *both* tails return non-empty
    // output (process kept running between them).

    let mut s = Server::start();
    let opened = s.call_tool("pty_open", json!({
        "command": "while true; do echo tick; sleep 0.05; done",
        "freeze": true
    }));
    let handle = opened["handle"].as_str().unwrap().to_string();

    // Give the child time to produce a few ticks before the first tail.
    std::thread::sleep(Duration::from_millis(250));

    // Initial tail: state should still be running, ticks accumulated.
    let t1 = s.call_tool("pty_tail", json!({ "handle": &handle }));
    assert_eq!(t1["state"], "running", "tail must NOT freeze the child");
    let raw1 = t1["raw_since_last"].as_str().unwrap();
    assert!(raw1.contains("tick"), "expected ticks in first tail: {raw1:?}");

    // Wait, then tail again. Process should still be running and
    // produced more output.
    std::thread::sleep(Duration::from_millis(300));
    let t2 = s.call_tool("pty_tail", json!({ "handle": &handle }));
    assert_eq!(t2["state"], "running", "still running between tails");
    let raw2 = t2["raw_since_last"].as_str().unwrap();
    assert!(raw2.contains("tick"), "expected new ticks in second tail: {raw2:?}");

    s.call_tool("pty_close", json!({ "handle": &handle }));
}
