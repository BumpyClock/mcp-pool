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
- **Daemon + control socket.** `mcp-pool pool serve` runs the daemon holding
  the `Pool` registry. Native `start`/`stop`/`restart`/`status` use its local
  control socket through `daemon_client.rs` and auto-launch it when absent.
  `pool list` reads native config without connecting.
  `daemon status` and `daemon stop` do not auto-launch.
- **MCP commands** (`mcp_cli.rs`, `tool_discovery.rs`, `tool_arguments.rs`,
  `tool_output.rs`): discovery, calls, and resources use pooled connections through
  `mcp_client.rs`. `server_config.rs` reads existing mcporter JSONC directly.
  Editor imports and config conversion are deferred; this is not complete mcporter parity.
  These commands reuse the native daemon and multiplexer, not a parallel ephemeral broker.
  mcporter is the command/config inspiration, not a runtime dependency.
- **Tool bridge** (`mcp_bridge.rs`, `mcp_bridge_*.rs`): top-level `serve` exposes
  keep-alive entries over stdio or HTTP. HTTP POST `/mcp` combines namespaced
  `server__tool` tools; `/mcp/<server>` exposes bare tool names. The bridge
  supports JSON POST responses and modern `server/discover` (`2026-07-28`).
  Modern HTTP requires matching version/method/name headers and protocol-version
  metadata. POST `subscriptions/listen` streams acknowledgements, subscription-ID
  tagged tool-list invalidations, and a completion on graceful shutdown over SSE.
  Listeners use dedicated local connections to the same pooled upstream;
  reconnecting does not replay requests or missed events.
  GET, DELETE, downstream sessions, and ordinary tool-call related-message SSE
  are unsupported. Stdio has discovery but no subscription streams.
  This is a tool subset, not full mcporter `serve` parity.
  It does not expose resources, prompts, or elicitation.
  It acknowledges downstream notifications without forwarding them and does
  not relay upstream sampling/roots requests to bridge clients.
  HTTP defaults to loopback and has no downstream authentication.
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
- Configured headers and associated OAuth credentials are supported for servers
  resolved from JSON config. Only explicit `auth`/`config login` starts consent.
  HTTP headers, `bearerToken`, and `bearerTokenEnv` resolve environment values
  from configured `env` overrides before the caller's environment.
  Discovery, calls, resources, and proxy can reuse credentials or refresh tokens;
  they do not launch consent. `--no-oauth` permits valid cached tokens only.
  Authorization requires advertised PKCE S256; signed token methods are unsupported.
  Without dynamic registration, authorization requires a legitimate registered client ID
  or supported HTTPS client metadata URL.
- Optional Streamable GET and resumable event replay are unsupported.
  Requests with unknown outcomes are never replayed automatically.

## Shared session semantics
- Native pools are keyed by configured name, not executable identity.
  JSON-config pool identities also include config source and resolved definition.
- One pool shares upstream state across clients. Stateful multi-call sequences
  need caller coordination; JSON-RPC ID routing does not isolate server state.
- Successful initialization and unpaginated tool discovery are cached. Tool-list
  change notifications invalidate discovery; upstream notifications broadcast.
- Sampling/roots requests prefer a capable, most-recently-active client. This
  heuristic does not prove callback causality or provide per-client isolation.
- MCP command deadlines are per caller, not pool identity. `list` bounds each
  server's credential preparation, connection, and paginated discovery together.
  Calls/resources use separate connection, discovery, and request budgets;
  their credential preparation precedes the connection deadline.
  An interrupted request closes its local connection, not the shared upstream.
- Concurrent initialization and cacheable unpaginated `tools/list` share one
  leader request. Longer followers extend the shared backend wait without
  replay; each caller retains its local deadline. Shared waits retain the
  backend default/configured budget as a minimum. Leader disconnect does not
  cancel followers. Calls are not coalesced.
  Pending-route cleanup defaults to five minutes; longer caller/shared budgets
  can extend retention. Cleanup is distinct from local operation deadlines.
- Bridge operations include queue waits and reconnect setup in configured
  `timeoutMs` (60 seconds by default, no 120-second cap). Established HTTP
  subscriptions have no idle operation deadline. They forward tool-list
  invalidations only, with bounded queues that can coalesce invalidations.
  HTTP permits 64 in-flight requests including streams, plus at most 64
  dedicated notification connections across subscriptions.
- `allowedTools`/`blockedTools` filter discovery, calls, and bridge exposure by
  exact upstream names. They are mutually exclusive string arrays, not patterns.
  `allowed_tools`/`blocked_tools` are aliases. All paths use `tool_filter.rs`;
  there is no separate bridge filter.
  Raw `proxy` and direct upstream access bypass these filters.

## CLI surface
- MCP commands: `list`/`describe`/`list-tools`, `call`, `resource`/`resources`,
  `auth`, `vault set|clear`, `config list|get|add|remove|login|logout|doctor|help`,
  `daemon start|status|stop|restart|migrate|help`, and `serve`.
- Native commands: `pool list`, `pool serve`, `start`, `stop`, `restart`,
  `status [name]`, `add`, `remove`, `proxy`, and `shutdown`.
- `add NAME -- COMMAND [ARGS...]` (stdio) or `add NAME --url URL [--transport http|sse]`.
- Top-level `list` performs live discovery, not a native config listing.
  `running` pool status confirms transport setup, not discovery or MCP readiness.
- Dotted tool selectors split at the first dot. `docs.search.v2` selects server
  `docs` and tool `search.v2` for calls and tool-specific discovery.
- `call --output raw` deliberately emits a JSON result envelope, not Node inspect text.
- `--oauth-timeout` is accepted only for `auth` and `config login`.
  Cached refresh keeps its fixed OAuth deadline; `--timeout` bounds operation waits.
- Daemon logging options apply on launch; restart applies changed options.
  `--log-servers` selects exact logical configured names.
- Ad-hoc `--persist PATH` uses the existing config writer and rejects existing names.
  `--save-images DIR` writes returned images; `--tail-log` prints a bounded log tail,
  not a continuous stream.
- `serve [--stdio | --http PORT [--host HOST]] [--servers NAME,...]` requires
  keep-alive entries. It is not the native daemon command.
- `proxy NAME` prefers native config, then the selected JSON config.
  With `MCP_POOL_HOME`, fallback requires explicit `MCPORTER_CONFIG`.
  Proxy config selection uses the environment, not `--config`.
  `pool proxy NAME` uses native config only.
- `config import` is deferred. `daemon migrate` inventories legacy owners.
  Retirement requires `--stop-legacy --confirmed-drained`, a same-user local
  handshake, and verified process identities/tree ownership.
  It asks the owning runtime to stop, not arbitrary PIDs.
  Failed retirement retains a journal and blocks cutover.
- `emit-ts`, `generate-cli`, `inspect-cli`, `record`, `replay`, and an SDK are out of scope.

## Paths & identity (XDG-aware)
- Native config: `<config_dir>/mcp-pool/config.toml` (`%APPDATA%` on Windows).
- JSON config selection: `--config PATH`, then `MCPORTER_CONFIG`, then
  `~/.mcporter/mcporter.json` (`%USERPROFILE%\.mcporter\mcporter.json` on Windows).
  Only the selected file's `mcpServers` entries are loaded.
- Credentials: `$XDG_DATA_HOME/mcporter/credentials.json` when configured,
  otherwise `~/.mcporter/credentials.json`; per-server caches and `tokenCacheDir`
  are also read. These stores are used directly, not migrated.
- State / sockets: `<state_dir>/mcp-pool/run/` (`%LOCALAPPDATA%` on Windows).
- Control socket: `<state_dir>/mcp-pool/mcp-pool-control.sock` /
  `\\.\pipe\mcp-pool-<home-hash>-control`.
- Per-server socket: `mcp-pool-<name>.sock` /
  `\\.\pipe\mcp-pool-<home-hash>-<name>`.
- `MCP_POOL_HOME` isolates native config and daemon state, not JSON config or credentials.
- `MCP_POOL_CREDENTIALS_READ_ONLY=1` allows valid cached credentials but blocks
  refresh, consent, and vault mutations. The caller's policy is sent to the daemon,
  including an existing daemon started without the flag.
  It does not block explicit config writes.
- Config replacement/removal and credential mutations retire matching prior pool views.
  Failed retirement blocks the mutation; clients can be disconnected.
- Credential reconciliation uses committed generation ordering, not token expiration.
  Divergent legacy rotating tokens without verified ordering fail closed.
  Explicit `auth --reset` replaces credentials; it is not a read-only validation step.
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
