# Architecture and source ownership

[README](../README.md) | [Development](development.md) | [Pools](pools.md) |
[Shared sessions](shared-sessions.md)

The persistent native daemon owns pools; MCP commands and agent proxies use its
multiplexer rather than an ephemeral broker. Each pool holds one upstream and
routes concurrent JSON-RPC messages by ID. Callers resolve definitions and
credentials before requesting pools; the daemon does not load config or warm
servers on launch. The bridge is a separate tool-facing interface backed by the
same pools. See [pools](pools.md), [transports](transports.md), and
[shared sessions](shared-sessions.md) for runtime contracts.

## Source map

Source files live in `src/`:

- `transport.rs` provides Unix sockets and Windows named pipes; `config.rs` defines
  server settings and runtime paths.
- `server_config.rs` and `server_config_entry.rs` normalize JSONC and decode
  entries; `server_config_settings.rs` interprets borrowed settings without
  duplicating mutable config.
- `mcp_cli.rs`, `tool_arguments.rs`, `tool_output.rs`, and `tool_discovery.rs`
  handle MCP commands, output, and discovery.
- `config_commands_plan.rs` shares conflict and retirement planning for copy and
  batch imports; `config_commands_input.rs` adapts CLI arguments.
- `daemon_commands.rs` orchestrates auth, vault, and daemon commands.
- `mcp_client_endpoint.rs` prepares credentials and pool identity; `mcp_client.rs`
  and `mcp_client_wire.rs` own local MCP sessions.
- `mcp_bridge.rs` and `mcp_bridge_*.rs` implement the stdio/HTTP keep-alive bridge.
- `tool_filter.rs` normalizes catalogs and filters discovery, calls, and bridge
  exposure. `tool_schema_view.rs` handles bounded schema references and option
  aliases; `tool_inputs.rs` handles ordered input provenance.
- `oauth.rs` and `oauth_*.rs` handle authorization, reuse, storage, and refresh.
- `upstream.rs` defines upstream ownership and confirmed shutdown;
  `upstream_stdio.rs` and `upstream_http.rs` implement transport backends.
- `jsonrpc.rs` translates per-client IDs to pool-unique IDs. `socket_proxy.rs` owns
  generation lifecycle; `socket_proxy_cache.rs` owns route/cache transactions with
  delivery outside locks. `mcp_session.rs` and `session_cache.rs` own shared-request
  transitions and cache state.
- `pool.rs` and `pool_endpoint.rs` separate owned servers from discovered external
  endpoints. `control.rs` and `daemon.rs` implement the control protocol and daemon.
- `daemon_client.rs` owns control exchange and launch with distinct connection
  policies. `daemon_configuration_ownership.rs` selects pool views for native
  control and mutation retirement.
- `proxy.rs` bridges agent stdio to a pool socket and starts the upstream if needed.
- `cli_syntax.rs` derives global option arity from Clap; `cli.rs`, `cli_output.rs`,
  and `main.rs` own dispatch, output, and runtime lifetime.
