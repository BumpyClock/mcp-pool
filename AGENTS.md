# AGENTS.md — mcp-pool

## Core goal
`mcp-pool` is a standalone, platform-independent CLI that lets users create and
manage **MCP (Model Context Protocol) server pools**. Each pooled MCP server
runs **exactly once** as a single upstream; many agent clients share it over a
local socket, with JSON-RPC requests multiplexed by `id`.

It is intentionally decoupled from any terminal emulator or agent runner. An
agent (claude-code, codex, etc.) that would normally spawn its own MCP instead
points its MCP config at `mcp-pool proxy <name>`, which bridges the agent's
stdio to the shared pool socket. Net effect: N parallel agent sessions reuse one
upstream process/connection instead of launching N copies.

## Architecture
- **Daemon + control socket.** `mcp-pool serve` is a long-lived daemon holding
  the `Pool` (registry of `SocketProxy` entries). All other subcommands
  (`start`/`stop`/`restart`/`status`/`list`) talk to it over a local control
  socket (Unix domain socket file / Windows named pipe), auto-launching the
  daemon if it is not already running.
- **Run-once multiplexer** (`socket_proxy.rs`): accepts many client connections
  on the per-server socket, forwards requests to the single upstream, and routes
  responses back by JSON-RPC `id`. Messages with no `id` (notifications) are
  broadcast to all clients. Stale request entries are TTL-cleaned.
- **Upstream abstraction** (`upstream.rs`): one backend interface, two impls —
  `Stdio` (spawn one child, own stdin/stdout) and `Http` (one persistent client;
  per-request POST; JSON or SSE responses routed by `id`). The multiplexer is
  backend-agnostic and reuses the same id-routing for both.
- **Socket discovery / reattach** (`pool.rs::discover_existing_sockets`): on
  daemon start, live sockets left by a previous run are re-registered, so pools
  survive daemon restarts.

## Transports supported
- **stdio** — local command MCPs (full support).
- **HTTP / SSE** — remote MCPs (v1: per-request POST + id routing + JSON/SSE
  response parsing; full MCP *Streamable HTTP* session lifecycle is a future goal).

## CLI surface
`serve | start | stop | restart | status [name] | list | add | remove | proxy`.
- `add NAME -- COMMAND [ARGS...]` (stdio) or `add NAME --url URL [--transport http|sse]`.
- `proxy NAME` is what an agent's MCP config invokes.

## Paths & identity (XDG-aware)
- Config: `<config_dir>/mcp-pool/config.toml` (`%APPDATA%` on Windows).
- State / sockets: `<state_dir>/mcp-pool/run/` (`%LOCALAPPDATA%` on Windows).
- Control socket: `<state_dir>/mcp-pool/control.sock` / `\\.\pipe\mcp-pool-control`.
- Per-server socket: `mcp-pool-<name>.sock` / `\\.\pipe\mcp-pool-<name>`.
- `MCP_POOL_HOME` overrides the whole home (config+state) — used for tests/isolation.
- `MCP_POOL_DEBUG=1` enables diagnostic logging to the state dir.

## Coding conventions (mandatory)
- Rust edition **2024**. No `mod.rs`, no `lib.rs` — modules declared from `src/main.rs`.
- **No panicking APIs**: no `unwrap()`/`expect()`/indexing that can panic. Propagate
  with `?`; use `.log_err()` or explicit `match`/`if let Err(...)` when ignoring.
  Never `let _ =` on fallible ops silently.
- Full-word variable names. Keep files ≤ ~500 LOC; split when they grow.
- **Cross-platform**: preserve `#[cfg(unix)]` / `#[cfg(windows)]` splits in
  `transport.rs`, `config.rs` paths, and `pool.rs::socket_alive`. The unified
  stream type is `transport::LocalStream` (boxed `LocalIo` trait object).
- Comments explain *why*, not *what*. No organizational/summary comments.

## Build / test
- `cargo build` / `cargo test`.
- Smoke: `mcp-pool add echo -- npx -y @modelcontextprotocol/server-everything stdio`,
  then `start` / `status` / `proxy echo` (feed a JSON-RPC `initialize` on stdin).
- Two concurrent `proxy` clients must share one upstream without cross-wiring.
