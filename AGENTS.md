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
- **Run-once multiplexer** (`socket_proxy.rs`, `socket_proxy_*.rs`): accepts many client connections
  on the per-server socket, forwards requests to the single upstream, and routes
  responses back by JSON-RPC `id`. Messages with no `id` (notifications) are
  broadcast to all clients. Stale request entries are TTL-cleaned.
- **Upstream abstraction** (`upstream.rs`, `upstream_stdio.rs`, `upstream_http*.rs`): one backend interface, two impls —
  `Stdio` (own a child process tree and stdin/stdout) and `Http` (one client;
  session-aware POST or legacy SSE GET/POST; JSON or SSE responses routed by
  `id`). The multiplexer is backend-agnostic and reuses id-routing for both.
- **Verified retirement.** Stop/restart/shutdown await backend and local task
  retirement. An unverified retirement poisons the generation and prohibits
  replacement. Windows owns a Job Object; Unix owns a process group. Servers
  must not detach workers from that process group.
- **Readiness.** `Starting` is transport setup; `Running` confirms setup, not MCP
  initialization. Status exposes local socket, upstream transport, and received
  initialize-result readiness separately, plus startup/retirement errors.
- **Socket discovery / reattach** (`pool.rs::discover_existing_sockets`): on
  daemon start, live Unix sockets owned by another process are registered as
  external endpoints. Discovery does not transfer child ownership. Windows
  named pipes cannot be enumerated. Daemon shutdown retires its owned upstreams.

## Transports supported
- **stdio** — local command MCPs (full support).
- **HTTP** — Streamable HTTP POST, session/version headers, JSON or incremental
  SSE responses, bounded concurrency and deadlines, correlated transport errors.
- **SSE** — legacy GET stream with same-origin POST endpoint discovery.
- No OAuth, configurable auth headers, optional Streamable GET, or resumable
  event replay. Requests with unknown outcomes are never replayed automatically.

## Shared session semantics
- Pools are keyed by configured name, not executable identity.
- One pool shares upstream state across clients. Stateful multi-call sequences
  need caller coordination; JSON-RPC ID routing does not isolate server state.
- Successful initialization and unpaginated tool discovery are cached. Tool-list
  change notifications invalidate discovery; upstream notifications broadcast.
- Sampling/roots requests prefer a capable, most-recently-active client. This
  heuristic does not prove callback causality or provide per-client isolation.

## CLI surface
`serve | start | stop | restart | status [name] | list | add | remove | proxy | shutdown`.
- `add NAME -- COMMAND [ARGS...]` (stdio) or `add NAME --url URL [--transport http|sse]`.
- `proxy NAME` is what an agent's MCP config invokes.

## Paths & identity (XDG-aware)
- Config: `<config_dir>/mcp-pool/config.toml` (`%APPDATA%` on Windows).
- State / sockets: `<state_dir>/mcp-pool/run/` (`%LOCALAPPDATA%` on Windows).
- Control socket: `<state_dir>/mcp-pool/mcp-pool-control.sock` /
  `\\.\pipe\mcp-pool-<home-hash>-control`.
- Per-server socket: `mcp-pool-<name>.sock` /
  `\\.\pipe\mcp-pool-<home-hash>-<name>`.
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
