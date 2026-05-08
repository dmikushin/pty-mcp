//! A single PTY-attached child process under our control.
//!
//! Lifecycle:
//!   - `spawn()` allocates a pty pair, forks the command, kicks off a
//!     reader thread that pumps bytes from the master into a vt100
//!     parser (so `screenshot()` is cheap).
//!   - `send()` writes user keys to the master.
//!   - `screenshot()` waits a brief settle period, then `SIGSTOP`s the
//!     child so the agent can think with a frozen process state, and
//!     returns the rendered screen + cursor + status.
//!   - `resume()` sends `SIGCONT` (called automatically before `send`).
//!   - `close()` sends SIGCONT (so the child can clean up), then SIGTERM,
//!     then SIGKILL after a short grace period.
//!
//! Reader thread:
//!   The thread owns the master `Read` half. Each chunk it reads is
//!   simultaneously fed to the vt100 parser (under a Mutex shared with
//!   the main thread) and copied into a "since-last-screenshot"
//!   accumulator so `screenshot()` can return the raw bytes that
//!   landed in the interim.
//!
//! Freeze accuracy:
//!   The settle loop polls `last_byte_time` (an Instant the reader
//!   updates on every read). If the PTY has been quiet for
//!   `settle_ms`, we SIGSTOP and lock in the current screen. Programs
//!   that emit at <settle_ms intervals indefinitely (e.g. `top`) cap
//!   out via `max_wait_ms` and we SIGSTOP regardless.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PtyState {
    Running,
    Paused,
    Exited,
}

#[derive(Serialize)]
pub struct ScreenshotResult {
    pub handle: String,
    pub state: PtyState,
    pub exit_code: Option<i32>,
    pub cursor: (u16, u16),
    pub alternate_screen: bool,
    pub size: (u16, u16),
    /// Rendered current screen — `rows * cols` characters of cells
    /// the user would see. No ANSI escapes; trailing whitespace on
    /// each row trimmed.
    pub screen: String,
    /// Raw bytes captured by the reader thread since the previous
    /// screenshot (or session start). Useful for diffing / for
    /// programs that emit logs without redrawing.
    pub raw_since_last: String,
}

#[derive(Serialize)]
pub struct TailResult {
    pub handle: String,
    pub state: PtyState,
    pub exit_code: Option<i32>,
    /// Raw bytes captured by the reader thread since the previous
    /// `tail` or `screenshot` call (whichever was most recent).
    pub raw_since_last: String,
}

#[derive(Serialize, Clone)]
pub struct SessionInfo {
    pub handle: String,
    pub command: String,
    pub state: PtyState,
    pub exit_code: Option<i32>,
    pub size: (u16, u16),
    pub freeze_enabled: bool,
}

pub struct PtySession {
    pub handle: String,
    pub command: String,
    pub freeze_enabled: bool,
    pub size: (u16, u16),

    pid: Pid,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    /// Last time the reader thread observed any byte. Used by the
    /// settle loop in `screenshot`.
    last_byte_at: Arc<Mutex<Instant>>,
    /// Raw bytes captured since the last screenshot. Drained on
    /// screenshot.
    raw_buffer: Arc<Mutex<Vec<u8>>>,
    /// Set by the reader thread when EOF hits the master (child is gone).
    eof: Arc<AtomicBool>,
    /// Exit status filled in by the wait thread once the child reaps.
    exit_status: Arc<Mutex<Option<i32>>>,

    state: PtyState,
    _reader_handle: JoinHandle<()>,
    _wait_handle: JoinHandle<()>,
}

impl PtySession {
    pub fn spawn(
        handle: String,
        command: &str,
        cols: u16,
        rows: u16,
        env: HashMap<String, String>,
        cwd: Option<String>,
        freeze_enabled: bool,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("openpty failed")?;

        // Run through `sh -c` so the agent can pass arbitrary command
        // strings (pipes, env vars, etc.) — same shape as `Bash` tool.
        let mut cmd = CommandBuilder::new("sh");
        cmd.arg("-c");
        cmd.arg(command);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        if let Some(cwd) = cwd.as_ref() {
            cmd.cwd(cwd);
        }
        // Make sure the child sees a usable TERM if the host didn't.
        if !env.contains_key("TERM") && std::env::var("TERM").is_err() {
            cmd.env("TERM", "xterm-256color");
        }

        let mut child = pair.slave.spawn_command(cmd).context("spawn_command failed")?;
        let pid = child
            .process_id()
            .ok_or_else(|| anyhow!("spawned child has no pid"))?;
        let pid = Pid::from_raw(pid as i32);

        let writer = pair.master.take_writer().context("take_writer failed")?;
        let mut reader = pair.master.try_clone_reader().context("try_clone_reader failed")?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 1000)));
        let last_byte_at = Arc::new(Mutex::new(Instant::now()));
        let raw_buffer = Arc::new(Mutex::new(Vec::new()));
        let eof = Arc::new(AtomicBool::new(false));
        let exit_status = Arc::new(Mutex::new(None));

        // Reader thread: pump master -> parser + raw buffer.
        let reader_handle = {
            let parser = parser.clone();
            let last_byte_at = last_byte_at.clone();
            let raw_buffer = raw_buffer.clone();
            let eof = eof.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            eof.store(true, Ordering::Release);
                            break;
                        }
                        Ok(n) => {
                            // Update last_byte_at first so settle loops
                            // see the timing as soon as possible.
                            *last_byte_at.lock().unwrap() = Instant::now();
                            // Feed parser. If lock is contended (caller
                            // is taking a screenshot mid-flight), we
                            // wait — output ordering matters more than
                            // throughput.
                            parser.lock().unwrap().process(&buf[..n]);
                            raw_buffer.lock().unwrap().extend_from_slice(&buf[..n]);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => {
                            eof.store(true, Ordering::Release);
                            break;
                        }
                    }
                }
            })
        };

        // Wait thread: reaps the child, fills exit_status. portable-pty's
        // `Child::wait` is blocking, so we put it on its own thread.
        let wait_handle = {
            let exit_status = exit_status.clone();
            thread::spawn(move || match child.wait() {
                Ok(status) => {
                    let code = status.exit_code() as i32;
                    *exit_status.lock().unwrap() = Some(code);
                }
                Err(_) => {
                    *exit_status.lock().unwrap() = Some(-1);
                }
            })
        };

        Ok(Self {
            handle,
            command: command.to_string(),
            freeze_enabled,
            size: (cols, rows),
            pid,
            master: pair.master,
            writer,
            parser,
            last_byte_at,
            raw_buffer,
            eof,
            exit_status,
            state: PtyState::Running,
            _reader_handle: reader_handle,
            _wait_handle: wait_handle,
        })
    }

    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            handle: self.handle.clone(),
            command: self.command.clone(),
            state: self.current_state(),
            exit_code: *self.exit_status.lock().unwrap(),
            size: self.size,
            freeze_enabled: self.freeze_enabled,
        }
    }

    fn current_state(&self) -> PtyState {
        if self.exit_status.lock().unwrap().is_some() {
            return PtyState::Exited;
        }
        self.state
    }

    /// Send raw bytes (already key-parsed) to the master. If the child
    /// is currently `SIGSTOP`'d, we resume it first.
    pub fn send(&mut self, bytes: &[u8]) -> Result<()> {
        if self.current_state() == PtyState::Exited {
            return Err(anyhow!("session has exited"));
        }
        if self.state == PtyState::Paused {
            self.resume()?;
        }
        self.writer.write_all(bytes).context("PTY master write failed")?;
        self.writer.flush().ok();
        Ok(())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("resize failed")?;
        self.parser.lock().unwrap().set_size(rows, cols);
        self.size = (cols, rows);
        Ok(())
    }

    /// Take a screenshot. If `freeze_enabled`, we wait for `settle_ms`
    /// of quiet output (capped at `max_wait_ms`) and then SIGSTOP the
    /// child, so the rendered screen reflects a stable state and the
    /// agent can think with the process paused.
    pub fn screenshot(&mut self, settle_ms: u64, max_wait_ms: u64) -> Result<ScreenshotResult> {
        if self.current_state() != PtyState::Exited {
            self.settle(settle_ms, max_wait_ms);
            if self.freeze_enabled && self.state != PtyState::Paused {
                kill(self.pid, Signal::SIGSTOP).ok();
                self.state = PtyState::Paused;
            }
        }

        let parser = self.parser.lock().unwrap();
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        let cursor = screen.cursor_position();
        let alt = screen.alternate_screen();

        // vt100's screen.contents() returns the visible grid as a
        // newline-joined String — ideal for the agent.
        let rendered = screen.contents();

        let raw = {
            let mut buf = self.raw_buffer.lock().unwrap();
            let drained = std::mem::take(&mut *buf);
            String::from_utf8_lossy(&drained).into_owned()
        };

        Ok(ScreenshotResult {
            handle: self.handle.clone(),
            state: self.current_state(),
            exit_code: *self.exit_status.lock().unwrap(),
            cursor: (cursor.0, cursor.1),
            alternate_screen: alt,
            size: (cols, rows),
            screen: rendered,
            raw_since_last: raw,
        })
    }

    fn settle(&self, settle_ms: u64, max_wait_ms: u64) {
        if settle_ms == 0 {
            return;
        }
        let deadline = Instant::now() + Duration::from_millis(max_wait_ms);
        let quiet_threshold = Duration::from_millis(settle_ms);
        loop {
            let last = *self.last_byte_at.lock().unwrap();
            if last.elapsed() >= quiet_threshold {
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            // Sleep for half the remaining quiet window — bounded
            // tighter than the threshold so we don't oversleep past
            // the moment of quiet.
            let needed = quiet_threshold.saturating_sub(last.elapsed());
            let sleep = needed.min(Duration::from_millis(50));
            thread::sleep(sleep);
        }
    }

    /// Resume after a freeze. Idempotent.
    pub fn resume(&mut self) -> Result<()> {
        if self.state == PtyState::Paused {
            kill(self.pid, Signal::SIGCONT).ok();
            self.state = PtyState::Running;
        }
        Ok(())
    }

    /// Drain the raw byte buffer **without** freezing the child or
    /// rendering the screen. Useful for live tail-style monitoring of a
    /// long-running command (e.g. `make`, `pytest`, `cargo build`)
    /// where the agent wants to see incremental output without
    /// disrupting the process. State (running/paused/exited) and
    /// process timing are unchanged.
    pub fn tail(&self) -> TailResult {
        let raw = {
            let mut buf = self.raw_buffer.lock().unwrap();
            let drained = std::mem::take(&mut *buf);
            String::from_utf8_lossy(&drained).into_owned()
        };
        TailResult {
            handle: self.handle.clone(),
            state: self.current_state(),
            exit_code: *self.exit_status.lock().unwrap(),
            raw_since_last: raw,
        }
    }

    /// Close the session. Best-effort SIGCONT (in case it was paused),
    /// SIGTERM, then SIGKILL after a short grace.
    pub fn close(&mut self) -> Result<()> {
        if self.current_state() == PtyState::Exited {
            return Ok(());
        }
        let _ = kill(self.pid, Signal::SIGCONT);
        let _ = kill(self.pid, Signal::SIGTERM);
        // Give the process up to 1s to exit cleanly.
        let deadline = Instant::now() + Duration::from_millis(1000);
        while Instant::now() < deadline {
            if self.exit_status.lock().unwrap().is_some() || self.eof.load(Ordering::Acquire) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = kill(self.pid, Signal::SIGKILL);
        Ok(())
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
