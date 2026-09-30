# mcp-pool

Standalone, platform-independent CLI for pooling MCP (Model Context Protocol) servers.
Each pooled MCP server runs **once** as a single upstream; many agent clients share it
over a local socket, with JSON-RPC requests multiplexed by `id`.

## Why

Tools like `claude-code` spawn every configured MCP server themselves. If you run
several agent sessions in parallel, the same heavy MCP (e.g. an `npx` server) gets
launched once per session. `mcp-pool` deduplicates that: one upstream, many clients.

## Install / build

```sh
cargo build --release
# binary at target/release/mcp-pool
```

## Development

```sh
cargo test
cargo clippy --all-targets
bacon --job run
bacon --job clippy
```

On Windows, `.\dev.ps1` starts the `bacon --job run` watcher. On macOS/Linux,
use `./dev.sh`.

## Usage

```sh
# Define a stdio MCP (command + trailing args after --)
mcp-pool add echo -- npx -y @modelcontextprotocol/server-everything stdio

# Define a remote HTTP/SSE MCP
mcp-pool add remote --url https://example.com/mcp --transport http

# Lifecycle (drives a long-lived daemon over a control socket)
mcp-pool start echo        # pre-warm the pooled upstream (optional)
mcp-pool status            # show all pools
mcp-pool restart echo
mcp-pool stop echo
mcp-pool list              # configured servers
mcp-pool remove echo

# Run the daemon explicitly (otherwise auto-launched on first command)
mcp-pool serve

# Bridge an agent's stdio to a pool socket. Put this in the agent's MCP config:
#   command = "mcp-pool", args = ["proxy", "echo"]
# proxy is self-starting: if the upstream is already running it just attaches
# (shared), otherwise it auto-starts the upstream (and the daemon) first. No
# separate `start` step is required — `start` is only for pre-warming.
mcp-pool proxy echo
```

Set `MCP_POOL_DEBUG=1` to enable diagnostic logging to the state dir.

## Lifecycle and readiness

`start` confirms upstream transport setup, not MCP initialization. Status reports
`starting` during setup, `running` after setup, and `stopping` during retirement.
The table's readiness column distinguishes a bound local `socket`, an available
upstream `transport`, and a received `mcp` initialization result. JSON status
includes these three readiness flags and startup or retirement errors.

`stop`, `restart`, and `shutdown` wait for owned upstream retirement. A failed
retirement leaves the pool `failed` and blocks replacement. Windows children run
inside a Job Object. Unix children run inside an owned process group. Unix
children that deliberately leave that group are outside this ownership boundary.
Do not configure servers that daemonize or detach their workers.

Windows executable resolution honors configured PATH and PATHEXT. Native programs
launch directly; batch launchers use Rust's batch argument encoding. Batch
arguments containing carriage returns or newlines are rejected before launch.
Diagnostic stderr is decoded lossily, so non-UTF-8 output does not stop a server.

If daemon shutdown fails, failed pools remain blocked and visible in status.
The daemon resumes accepting lifecycle commands for unrelated pools.

Restart creates fresh routing and discovery state. It disconnects existing proxy
clients; agents must reconnect. Pools share a configured name, not an executable
fingerprint. Configuring the same command under two names creates two pools.

## Remote transports

`--transport http` uses Streamable HTTP POST requests. Initialization captures
the session ID and negotiated protocol version for later requests. JSON responses
and incremental SSE responses are supported. `--transport sse` uses the legacy
GET event stream, discovers its same-origin message endpoint, and sends POSTs
there. It does not use a POST-only approximation of legacy SSE.

HTTP transport limits are 32 concurrent requests, 1 MiB per JSON response or SSE
frame, a 60-second request deadline, and a 30-second stream read deadline. Empty
202/204 responses are accepted for notifications. Transport failures produce
JSON-RPC errors with the requesting client's original ID.

Deliberate HTTP shutdown sends a session DELETE when a session ID exists. DELETE
has a two-second deadline. Unsupported DELETE or a timeout is logged without
blocking confirmed local retirement. Remote cleanup is not guaranteed.

An expired HTTP session returns an error without replaying the request. Restart
the pool and reconnect before issuing new requests. Optional Streamable HTTP GET
streams and resumable event replay are not implemented. OAuth and configurable
authorization headers are also not implemented.

## Shared session contract

A pool shares one MCP session, not just one executable. Clients have separate
JSON-RPC request IDs, but they do not have separate upstream state. A tool that
selects a browser page, changes a working directory, or modifies server settings
can affect other clients. Coordinate stateful select-then-act sequences between
agents. Request multiplexing does not make those sequences atomic.

Successful `initialize` and unpaginated `tools/list` results are cached for later
clients. A `notifications/tools/list_changed` notification invalidates tool
discovery. Upstream notifications are broadcast to connected clients.

Server requests for sampling or roots go to a client that advertises the required
capability. The pool prefers the most recently active capable client. This is a
routing heuristic, not proof that the selected client caused the callback.
Other server requests also use a most-recently-active fallback. Do not assume
per-client callback isolation.

A failed or timed-out tool call can have an unknown outcome. The pool does not
replay it automatically. Check the affected system before repeating an operation
that changes state.

## Layout

- `transport.rs` — unified local transport (Unix domain sockets / Windows named pipes)
- `config.rs` — server definitions + paths (XDG-aware)
- `upstream.rs` — upstream ownership and confirmed shutdown contract
- `upstream_stdio.rs` / `upstream_http.rs` — stdio and remote transport backends
- `jsonrpc.rs` — JSON-RPC id translation (per-client ids rewritten to pool-unique ids)
- `socket_proxy.rs` / `socket_proxy_*.rs` — lifecycle, client handling, routing, and discovery caching
- `pool.rs` — registry of pooled servers + socket discovery for daemon reattach
- `control.rs` / `daemon.rs` — control protocol + long-lived daemon
- `proxy.rs` — per-agent stdio bridge to a pool socket (self-starts the upstream)
- `cli.rs` / `cli_output.rs` / `main.rs` — subcommand dispatch and output
