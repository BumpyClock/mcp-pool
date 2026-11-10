# mcp-cli

`mcp-cli` pools Model Context Protocol (MCP) servers. Each pool owns one upstream
process or connection; multiple agent clients share it over a local socket.
JSON-RPC IDs route responses to the right client. The CLI also supports tool
discovery, calls, resources, and authentication.

## Install

Build from a Rust checkout with `cargo build --release`, or install the current
checkout with `sh ./install.sh` on macOS/Linux or `.\install.ps1` on Windows. See
[installation](docs/installation.md) for prerequisites, options, and upgrade
precautions.

## Quick start

Inspect the destination with `mcp-cli config list` and choose an unused name.
`add` replaces an existing entry of the same name in the destination. Replace the
example command and arguments with those for your MCP server. This example runs
an npm package and may download it when started.

```sh
mcp-cli add echo -- npx -y @modelcontextprotocol/server-everything stdio
mcp-cli pool list
mcp-cli list echo
```

`pool list` reads configuration without connecting. `list echo` starts or attaches
to the pool and discovers tools. To let an agent use the same pool, add an entry
in the agent's MCP configuration using its required syntax. For JSON-based agent
configs, the entry generally looks like this, with `/absolute/path/to/project`
replaced by the absolute path to the project used above. On Windows, use an
absolute path such as `C:/path/to/project`.

```json
{"mcpServers":{"echo":{"command":"mcp-cli","args":["--root","/absolute/path/to/project","proxy","echo"]}}}
```

When the agent runs from a different working directory, select the same project
with `--root` or the same explicit file with `--config`. Put these global flags
before `proxy`. If using a home override, give the shell and agent the same
absolute `MCP_CLI_HOME` path, or `MCP_POOL_HOME` fallback, to reach the same daemon.
`--root` does not select daemon state. The agent must be able to find `mcp-cli` on its
PATH. `proxy` starts the daemon and upstream if needed; `mcp-cli start echo` is
optional pre-warming. See [pool lifecycle](docs/pools.md) and
[configuration](docs/configuration.md).

## Before sharing a pool

Clients share upstream state as well as a process. Coordinate stateful sequences
between agents. A timed-out call may have succeeded; inspect the target before
repeating it. Requests with unknown outcomes are never replayed automatically.
See [shared sessions](docs/shared-sessions.md).

Local sockets are restricted to the same user, but raw proxy access bypasses tool
filters. The HTTP bridge has no downstream authentication. Even on loopback,
other local users can connect and invoke tools. Binding outside loopback can also
expose tools to other hosts. Use separate access isolation when local users are
not trusted. See
[pool security](docs/pools.md#local-endpoint-security) and
[bridge security](docs/bridge.md).

## Guides

| Guide | Use it to |
| --- | --- |
| [Installation](docs/installation.md) | Build, install, or upgrade from mcp-pool. |
| [Configuration](docs/configuration.md) | Select config files, define servers, and manage settings. |
| [Imports](docs/imports.md) | Copy definitions or opt in to mcporter credential migration. |
| [Commands](docs/commands.md) | Discover tools, call them, and read resources. |
| [Authentication](docs/authentication.md) | Manage OAuth, credential scope, and read-only policy. |
| [Pools](docs/pools.md) | Control the daemon, lifecycle, proxy, and local endpoints. |
| [Bridge](docs/bridge.md) | Expose keep-alive tools over stdio or HTTP. |
| [Transports](docs/transports.md) | Configure protocol negotiation and remote HTTP/SSE. |
| [Shared sessions](docs/shared-sessions.md) | Understand caching, routing, deadlines, and shared state. |
| [Development](docs/development.md) | Run tests, opt-in hooks, and benchmarks. |
| [Architecture](docs/architecture.md) | Find source ownership and runtime boundaries. |

The [CLI UX audit](docs/cli-ux-audit.md) and
[rename audit](docs/mcp-cli-rename-audit.md) are historical reports, not current
how-to guides.
