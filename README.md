# pty-mcp

MCP server that exposes a freeze-able PTY tool to LLM agents. Solves the
fundamental impedance mismatch between an LLM (turn-based, 5–30 s per
turn) and a PTY-attached process (continuous, real-time, ms-scale) by
`SIGSTOP`-ing the child between agent turns. The agent works with a
frozen process state, takes its time, and resumes the child with the
next `pty_send`.

Inspired by how debuggers (`gdb`) freeze the inferior to inspect state.

## Why

Plain Bash-style "exec, wait, return output" is fine for one-shots
(`ls`, `make`, scripts). But it falls over the moment a TTY is needed:
`sudo`, `ssh` interactive shell, `vim`, `htop`, `gdb`, `pytest -x` with
prompts, `npm init`, anything that calls `isatty(stdin)` and refuses to
run otherwise.

The naive solution — "give the agent a PTY and let it react in
real-time" — doesn't work because LLMs have ~10 s of latency per turn.
By the time the agent decides to press `q`, `less` has scrolled past or
`top` has redrawn three times.

`pty-mcp` fixes this by **kadrirovanie** (frame-stepping) the process:
between every agent turn, the child is `SIGSTOP`-ped. Real time stops
inside the process. The agent thinks for 30 seconds; the process sees
zero of those seconds. Next `pty_send` resumes execution. Next
`pty_screenshot` settles output, freezes again.

## Tools

| Tool | What it does |
|---|---|
| `pty_open` | Spawn a command in a fresh PTY. Returns a handle. `freeze: true` (default) enables SIGSTOP-between-turns. |
| `pty_send` | Send keys to the session. Auto-resumes if frozen. Supports `<Enter>`, `<C-c>`, `<Up>`, `<F5>`, etc. |
| `pty_screenshot` | Wait for output to settle (no bytes for `settle_ms`), `SIGSTOP` the child, return rendered screen + cursor + raw bytes since last screenshot. |
| `pty_tail` | Drain raw output WITHOUT freezing or rendering. For live-monitoring long commands (`make`, `pytest`, `cargo build`) without disturbing the process. |
| `pty_resize` | Change cols/rows. SIGWINCH delivered to the child. |
| `pty_close` | SIGCONT (in case it was paused), SIGTERM, SIGKILL after 1 s grace. |
| `pty_list` | Enumerate live sessions. |
| `pty_resume` | Explicit SIGCONT. Rarely needed — `pty_send` resumes automatically. |

`pty_screenshot` returns:

```json
{
  "handle": "pty-1",
  "state": "paused",
  "exit_code": null,
  "cursor": [3, 12],
  "alternate_screen": false,
  "size": [120, 40],
  "screen": "┌─────────────┐\n�� vim foo.txt │\n…",
  "raw_since_last": "<raw bytes captured since last screenshot>"
}
```

`screen` is rendered through a vt100 emulator — alternate-screen, ANSI
escapes, CSI sequences, all collapsed to what a human would see.

## Key parsing

`pty_send` accepts mixed literal text and named keys:

```
hello world<Enter>             -> "hello world\r"
:wq<Enter>                     -> ":wq\r"
<C-c>                          -> 0x03
<C-d>                          -> 0x04
<Esc>                          -> 0x1b
<Up><Up><Down><Enter>          -> 0x1b[A 0x1b[A 0x1b[B 0x0d
:%s/foo/bar/g<Enter>           -> ":%s/foo/bar/g\r"
```

Recognized: `<Enter>`, `<CR>`, `<LF>`, `<Tab>`, `<Esc>`, `<Backspace>`,
`<Space>`, `<C-x>` (any letter), `<Up>`, `<Down>`, `<Left>`, `<Right>`,
`<Home>`, `<End>`, `<PageUp>`, `<PageDown>`, `<Delete>`, `<Insert>`,
`<F1>`..`<F12>`. Unknown names error out instead of being passed through
literally.

## Limits of `freeze`

`SIGSTOP` is kernel-level — strictly more reliable than user-space flow
control (`Ctrl+S`/`Ctrl+Q`). But the **outside world** doesn't pause:

* Network peers don't know we're frozen — long SSH/HTTP/gRPC sessions
  can drop on remote keepalive timeouts.
* `cron`, `systemd-timer`, kernel `setitimer` keep ticking.
* Locks held by the frozen child block other processes that need them.

For these cases set `freeze: false` on `pty_open` — the session behaves
as a plain streaming PTY without SIGSTOP between turns. You lose the
"unlimited think time" but keep real-time semantics.

## Architecture: client + daemon

The same binary runs in two modes:

* **Client (default)** — a stateless MCP server that speaks JSON-RPC
  over stdin/stdout. Hosted by your agent (Claude Code, Dario, etc).
  Connects to the daemon over a Unix socket; auto-spawns the daemon
  if it isn't running.

* **Daemon (`pty-mcp --daemon`)** — a long-lived process that owns
  every PTY session. Sessions live here, not in the client. When the
  agent host restarts (Dario reboot, IDE reload, …) the daemon
  doesn't notice; the new client connects and inherits the live
  session list.

```
+--------+   stdin/stdout    +-----------------+   unix socket    +-----------+
| Dario  | <───── JSON-RPC ─> | pty-mcp client  | <─── JSON-RPC ─> | pty-mcp   |
| Claude |                    | (per-session)   |                  | daemon    |
+--------+                    +-----------------+                  | (PTYs)    |
                                                                   +-----------+
```

Socket path resolution: `$PTY_MCP_SOCKET` if set, else
`$XDG_RUNTIME_DIR/pty-mcp.sock`, else `/tmp/pty-mcp-<uid>.sock`.

Killing the daemon kills every session it owns. The client process
exiting does not.

## Build

```sh
cargo build --release
./target/release/pty-mcp   # speaks JSON-RPC over stdio
```

## Test

```sh
cargo test                 # 7 unit + 6 integration tests
```

Integration tests exercise the full client/daemon protocol against
the compiled binary, including:

* echo command renders correctly through the vt100 emulator
* interactive bash session round-trips a `uname -s` query
* `freeze: true` actually pauses the child (700 ms sleep between
  screenshots → 0 bytes from a `tick` loop — proves real SIGSTOP, not
  flow control)
* `pty_close` kills runaway loops
* `pty_tail` streams output WITHOUT freezing the child
* **session persists across client restart** — open in client A,
  drop the entire client process, connect a fresh client B to the
  same socket, the session is still there with its render buffer
  intact

## Wire it into Claude Code / Dario

Add to `~/.claude.json` or equivalent:

```json
{
  "mcpServers": {
    "pty": {
      "command": "/path/to/pty-mcp",
      "args": []
    }
  }
}
```

Tools become available as `mcp__pty__pty_open`, `mcp__pty__pty_send`,
etc.

## License

MIT.
