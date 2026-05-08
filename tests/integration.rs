//! Integration test that drives the compiled `pty-mcp` binary as a
//! subprocess via JSON-RPC stdio — exercising the full client/daemon
//! round-trip plus the SIGSTOP/SIGCONT freeze behavior against real
//! PTY-attached commands.
//!
//! Architecture under test:
//!
//!   TestDaemon  ── spawns `pty-mcp --daemon --socket /tmp/...`
//!       │           (a long-lived process owning sessions)
//!       │
//!       ▼ unix socket
//!   TestClient  ── spawns `pty-mcp --socket /tmp/...`
//!                  (the MCP server speaking stdio JSON-RPC; its
//!                   stdin/stdout are wired to the test).
//!
//! Each test gets its own socket path so they're fully isolated even
//! when cargo runs them in parallel.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_pty-mcp");

static SOCKET_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_socket_path() -> PathBuf {
    let n = SOCKET_COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    PathBuf::from(format!("/tmp/pty-mcp-test-{pid}-{n}.sock"))
}

// --- TestDaemon: one long-lived daemon process per test ---------------------

struct TestDaemon {
    socket: PathBuf,
    process: Option<Child>,
}

impl TestDaemon {
    fn start() -> Self {
        let socket = unique_socket_path();
        // No-op if the file is fresh, but harmless if a previous run
        // crashed and left the path behind.
        let _ = std::fs::remove_file(&socket);
        let process = Command::new(BIN)
            .arg("--daemon")
            .arg("--socket")
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon");
        let deadline = Instant::now() + Duration::from_secs(3);
        while !socket.exists() {
            if Instant::now() > deadline {
                panic!("daemon at {} did not bind socket in time", socket.display());
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        Self { socket, process: Some(process) }
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        if let Some(mut p) = self.process.take() {
            let _ = p.kill();
            let _ = p.wait();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

// --- TestClient: one MCP-server (client) process talking to the daemon ------

struct TestClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl TestClient {
    fn connect(socket: &Path) -> Self {
        let mut child = Command::new(BIN)
            .arg("--socket")
            .arg(socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn client");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut c = Self { child, stdin, stdout, next_id: 1 };
        let _ = c.request("initialize", json!({}));
        c.notify("notifications/initialized", json!({}));
        c
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

impl Drop for TestClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --- Tests ------------------------------------------------------------------

#[test]
fn echo_command_renders_through_vt100() {
    let daemon = TestDaemon::start();
    let mut c = TestClient::connect(&daemon.socket);
    let opened = c.call_tool("pty_open", json!({
        "command": "echo hello; echo world",
        "cols": 80,
        "rows": 24
    }));
    let handle = opened["handle"].as_str().unwrap().to_string();

    let snap = c.call_tool("pty_screenshot", json!({
        "handle": &handle,
        "settle_ms": 300,
        "max_wait_ms": 2000
    }));
    let screen = snap["screen"].as_str().unwrap();
    assert!(screen.contains("hello"), "screen missing 'hello': {screen:?}");
    assert!(screen.contains("world"), "screen missing 'world': {screen:?}");
    assert_eq!(snap["state"], "exited");
    assert_eq!(snap["exit_code"], 0);

    c.call_tool("pty_close", json!({ "handle": &handle }));
}

#[test]
fn interactive_bash_session_round_trips() {
    let daemon = TestDaemon::start();
    let mut c = TestClient::connect(&daemon.socket);
    let opened = c.call_tool("pty_open", json!({
        "command": "bash --noprofile --norc",
        "cols": 80,
        "rows": 24
    }));
    let handle = opened["handle"].as_str().unwrap().to_string();

    let snap = c.call_tool("pty_screenshot", json!({
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

    c.call_tool("pty_send", json!({
        "handle": &handle,
        "keys": "uname -s<Enter>"
    }));

    let snap2 = c.call_tool("pty_screenshot", json!({
        "handle": &handle,
        "settle_ms": 200,
        "max_wait_ms": 1500
    }));
    let screen2 = snap2["screen"].as_str().unwrap();
    assert!(
        screen2.contains("Linux") || screen2.contains("Darwin"),
        "uname output not visible: {screen2:?}"
    );

    c.call_tool("pty_close", json!({ "handle": &handle }));
}

#[test]
fn freeze_actually_pauses_the_child() {
    let daemon = TestDaemon::start();
    let mut c = TestClient::connect(&daemon.socket);
    let opened = c.call_tool("pty_open", json!({
        "command": "while true; do echo tick; sleep 0.05; done",
        "cols": 80, "rows": 24, "freeze": true
    }));
    let handle = opened["handle"].as_str().unwrap().to_string();

    let snap1 = c.call_tool("pty_screenshot", json!({
        "handle": &handle, "settle_ms": 100, "max_wait_ms": 500
    }));
    assert_eq!(snap1["state"], "paused");

    std::thread::sleep(Duration::from_millis(700));

    let snap2 = c.call_tool("pty_screenshot", json!({
        "handle": &handle, "settle_ms": 100, "max_wait_ms": 500
    }));
    let raw_during_pause = snap2["raw_since_last"].as_str().unwrap();
    assert!(
        raw_during_pause.is_empty(),
        "expected no output while paused, got {} bytes: {:?}",
        raw_during_pause.len(),
        &raw_during_pause[..raw_during_pause.len().min(120)]
    );

    c.call_tool("pty_close", json!({ "handle": &handle }));
}

#[test]
fn close_kills_runaway_loop() {
    let daemon = TestDaemon::start();
    let mut c = TestClient::connect(&daemon.socket);
    let opened = c.call_tool("pty_open", json!({
        "command": "while true; do :; done", "freeze": false
    }));
    let handle = opened["handle"].as_str().unwrap().to_string();
    c.call_tool("pty_close", json!({ "handle": &handle }));
    let listed = c.call_tool("pty_list", json!({}));
    assert!(listed["sessions"].as_array().unwrap().is_empty());
}

#[test]
fn tail_streams_without_freezing() {
    let daemon = TestDaemon::start();
    let mut c = TestClient::connect(&daemon.socket);
    let opened = c.call_tool("pty_open", json!({
        "command": "while true; do echo tick; sleep 0.05; done",
        "freeze": true
    }));
    let handle = opened["handle"].as_str().unwrap().to_string();

    std::thread::sleep(Duration::from_millis(250));

    let t1 = c.call_tool("pty_tail", json!({ "handle": &handle }));
    assert_eq!(t1["state"], "running", "tail must NOT freeze the child");
    let raw1 = t1["raw_since_last"].as_str().unwrap();
    assert!(raw1.contains("tick"), "expected ticks in first tail: {raw1:?}");

    std::thread::sleep(Duration::from_millis(300));
    let t2 = c.call_tool("pty_tail", json!({ "handle": &handle }));
    assert_eq!(t2["state"], "running");
    let raw2 = t2["raw_since_last"].as_str().unwrap();
    assert!(raw2.contains("tick"), "expected new ticks in second tail: {raw2:?}");

    c.call_tool("pty_close", json!({ "handle": &handle }));
}

// --- Persistence ------------------------------------------------------------

#[test]
fn session_persists_across_client_restart() {
    // The whole point of the daemon split: opening a session in one
    // client, then disconnecting that client and connecting a new
    // one, the session is still there. This is what makes
    // Dario/Claude-Code restarts non-destructive.

    let daemon = TestDaemon::start();

    // Client A: opens a long-running session.
    let handle = {
        let mut a = TestClient::connect(&daemon.socket);
        let opened = a.call_tool("pty_open", json!({
            "command": "bash --noprofile --norc",
            "cols": 80, "rows": 24
        }));
        let handle = opened["handle"].as_str().unwrap().to_string();
        // Drop a marker we can find later.
        a.call_tool("pty_send", json!({
            "handle": &handle,
            "keys": "echo MARKER-PERSIST-OK<Enter>"
        }));
        // Give bash a moment to actually emit the line so the marker
        // is in the daemon's render buffer when client B connects.
        std::thread::sleep(Duration::from_millis(300));
        handle
        // <- client A drops here; its process dies, but the daemon
        //    keeps the session alive.
    };

    // Tiny pause to make sure A's stream is fully torn down.
    std::thread::sleep(Duration::from_millis(150));

    // Client B: fresh process, same socket. The session must still
    // be here AND have the marker we left earlier visible on screen.
    let mut b = TestClient::connect(&daemon.socket);
    let listed = b.call_tool("pty_list", json!({}));
    let sessions = listed["sessions"].as_array().unwrap();
    assert!(
        sessions.iter().any(|s| s["handle"] == handle),
        "session {handle} not visible to a fresh client: {sessions:?}"
    );

    let snap = b.call_tool("pty_screenshot", json!({
        "handle": &handle, "settle_ms": 150, "max_wait_ms": 1000
    }));
    let screen = snap["screen"].as_str().unwrap();
    assert!(
        screen.contains("MARKER-PERSIST-OK"),
        "marker from client A is gone: {screen:?}"
    );

    b.call_tool("pty_close", json!({ "handle": &handle }));
}
