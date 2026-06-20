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

## Usage

```sh
# Define a stdio MCP (command + trailing args after --)
mcp-pool add echo -- npx -y @modelcontextprotocol/server-everything stdio

# Define a remote HTTP/SSE MCP
mcp-pool add remote --url https://example.com/mcp --transport http

# Lifecycle (drives a long-lived daemon over a control socket)
mcp-pool start echo        # start the pooled upstream
mcp-pool status            # show all pools
mcp-pool restart echo
mcp-pool stop echo
mcp-pool list              # configured servers
mcp-pool remove echo

# Run the daemon explicitly (otherwise auto-launched on first command)
mcp-pool serve

# Bridge an agent's stdio to a pool socket. Put this in the agent's MCP config:
#   command = "mcp-pool", args = ["proxy", "echo"]
mcp-pool proxy echo
```

Set `MCP_POOL_DEBUG=1` to enable diagnostic logging to the state dir.

## Layout

- `transport.rs` — unified local transport (Unix domain sockets / Windows named pipes)
- `config.rs` — server definitions + paths (XDG-aware)
- `upstream.rs` — single-upstream backend (stdio child or HTTP/SSE client)
- `socket_proxy.rs` — multiplexer: N agent clients share one upstream, routed by JSON-RPC `id`
- `pool.rs` — registry of pooled servers + socket discovery for daemon reattach
- `control.rs` / `daemon.rs` — control protocol + long-lived daemon
- `proxy.rs` — per-agent stdio bridge to a pool socket
- `cli.rs` / `main.rs` — subcommand dispatch
