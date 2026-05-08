//! `pty-mcp` — entry point.
//!
//! Two modes:
//!
//! * **Client (default)**. Reads JSON-RPC from stdin, writes responses
//!   to stdout. Connects to a daemon over a Unix socket and forwards
//!   messages verbatim. If no daemon is running, auto-spawns one as a
//!   detached child and waits for it to come up. This is what
//!   Dario / Claude Code talk to.
//!
//! * **Daemon (`--daemon`)**. Owns the actual PTY sessions. Binds a
//!   Unix socket and serves any number of concurrent clients. Sessions
//!   live in the daemon process, so an MCP-server (client) restart
//!   does not kill them — the next client to connect inherits the
//!   live session list. Daemon shutdown destroys all sessions.
//!
//! Socket path is `$PTY_MCP_SOCKET` if set, else
//! `$XDG_RUNTIME_DIR/pty-mcp.sock`, else `/tmp/pty-mcp-<uid>.sock`.

mod keys;
mod server;
mod session;

use std::env;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use server::{Server, handle_message};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use tracing::{info, warn};

fn main() -> Result<()> {
    init_logging();
    let args: Vec<String> = env::args().collect();
    let mut daemon = false;
    let mut socket_override: Option<PathBuf> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--daemon" => daemon = true,
            "--socket" => {
                i += 1;
                socket_override = Some(args.get(i).context("--socket needs a path")?.into());
            }
            "--help" | "-h" => {
                println!("usage: pty-mcp [--daemon] [--socket <path>]");
                return Ok(());
            }
            "--version" | "-V" => {
                println!("pty-mcp {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let socket_path = socket_override.unwrap_or_else(default_socket_path);

    if daemon {
        run_daemon(&socket_path)
    } else {
        run_client(&socket_path)
    }
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("PTY_MCP_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

fn default_socket_path() -> PathBuf {
    if let Ok(explicit) = env::var("PTY_MCP_SOCKET") {
        return PathBuf::from(explicit);
    }
    if let Ok(rt) = env::var("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(rt);
        if p.is_dir() {
            return p.join("pty-mcp.sock");
        }
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/pty-mcp-{uid}.sock"))
}

// --- Daemon -----------------------------------------------------------------

fn run_daemon(socket_path: &Path) -> Result<()> {
    // Refuse to start if another daemon is already listening on the socket.
    if let Ok(_conn) = UnixStream::connect(socket_path) {
        return Err(anyhow!(
            "another pty-mcp daemon is already listening on {}",
            socket_path.display()
        ));
    }
    // Stale socket file from a crashed previous daemon — remove before bind.
    if socket_path.exists() {
        std::fs::remove_file(socket_path).context("removing stale socket")?;
    }
    if let Some(parent) = socket_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).context("creating socket parent dir")?;
        }
    }
    let listener =
        UnixListener::bind(socket_path).context("UnixListener::bind on socket path")?;
    // Tighten permissions: only the owning UID may connect.
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
        .context("chmod 0600 on socket")?;

    info!("daemon listening on {}", socket_path.display());

    let server = Arc::new(Server::new());
    let shutdown = Arc::new(AtomicBool::new(false));

    // Install SIGTERM/SIGINT/SIGHUP handler. On the first signal we
    // flip the flag and self-connect to the listener — that wakes up
    // the blocked `accept()` so the main loop can re-check the flag
    // and exit cleanly.
    {
        let shutdown = shutdown.clone();
        let socket_path = socket_path.to_path_buf();
        let mut signals = Signals::new([SIGTERM, SIGINT, SIGHUP])
            .context("installing signal handler")?;
        thread::spawn(move || {
            for sig in signals.forever() {
                info!("received signal {sig}, initiating shutdown");
                shutdown.store(true, Ordering::SeqCst);
                // Wake the blocked accept(). We don't need the
                // connection — peer will see EOF immediately.
                let _ = UnixStream::connect(&socket_path);
                break;
            }
        });
    }

    for conn in listener.incoming() {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                warn!("accept failed: {e}");
                continue;
            }
        };
        let server = server.clone();
        thread::spawn(move || {
            if let Err(e) = handle_connection(server, conn) {
                warn!("connection error: {e:#}");
            }
        });
    }

    info!("draining sessions and removing socket");
    server.close_all();
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

fn handle_connection(server: Arc<Server>, stream: UnixStream) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone().context("clone stream for reader")?);
    let mut writer = stream;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(()), // peer disconnected
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(response) = handle_message(server.as_ref(), trimmed) {
            let serialized = serde_json::to_string(&response)?;
            writeln!(writer, "{serialized}")?;
            writer.flush()?;
        }
    }
}

// --- Client -----------------------------------------------------------------

fn run_client(socket_path: &Path) -> Result<()> {
    info!("pty-mcp v{} client starting", env!("CARGO_PKG_VERSION"));

    let stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(_) => {
            info!("no daemon at {} — spawning one", socket_path.display());
            spawn_daemon(socket_path)?;
            wait_for_daemon(socket_path, Duration::from_secs(3))
                .context("waiting for daemon to come up")?
        }
    };

    // Bidirectional forwarder: stdin → socket on a background thread,
    // socket → stdout on the main thread (so we exit when the daemon
    // closes the connection).
    let socket_writer = stream.try_clone().context("clone socket for writer")?;
    let stdin_to_socket = thread::spawn(move || -> Result<()> {
        let stdin = io::stdin();
        let mut stdin = stdin.lock();
        let mut out = socket_writer;
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.read_line(&mut line) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(e) => return Err(e.into()),
            }
            out.write_all(line.as_bytes())?;
            out.flush()?;
        }
    });

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // daemon closed
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }
        stdout.write_all(line.as_bytes())?;
        stdout.flush()?;
    }
    // We're exiting — drop the stdin thread (it'll error on closed
    // socket the next time it tries to write). join() best-effort.
    drop(stdin_to_socket);
    Ok(())
}

fn spawn_daemon(socket_path: &Path) -> Result<()> {
    let exe = env::current_exe().context("current_exe")?;
    let mut cmd = Command::new(&exe);
    cmd.arg("--daemon")
        .arg("--socket")
        .arg(socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Become a new session leader so the daemon is detached from the
    // parent's controlling terminal — closing the parent shell or
    // disconnecting from the MCP host won't deliver SIGHUP to it.
    unsafe {
        cmd.pre_exec(|| {
            // setsid() returns -1 on error but the only failure mode
            // is "already a session leader", which is fine.
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().context("failed to spawn daemon")?;
    // We deliberately don't wait — the daemon runs forever.
    drop(child);
    Ok(())
}

fn wait_for_daemon(socket_path: &Path, timeout: Duration) -> Result<UnixStream> {
    let deadline = Instant::now() + timeout;
    let mut last_err: Option<io::Error> = None;
    while Instant::now() < deadline {
        match UnixStream::connect(socket_path) {
            Ok(s) => return Ok(s),
            Err(e) => last_err = Some(e),
        }
        thread::sleep(Duration::from_millis(40));
    }
    Err(anyhow!(
        "daemon at {} did not become reachable in {:?}: {}",
        socket_path.display(),
        timeout,
        last_err.map(|e| e.to_string()).unwrap_or_default()
    ))
}

// --- Standalone-mode helpers (kept for tests) -------------------------------

#[cfg(test)]
#[allow(dead_code)]
fn run_inline_stdio() -> Result<()> {
    let server = Server::new();
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let mut stdin = stdin.lock();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.read_line(&mut line) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(response) = handle_message(&server, trimmed) {
            writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
            stdout.flush()?;
        }
    }
}
