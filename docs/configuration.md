# Configure servers

[README](../README.md) | [Imports](imports.md) | [Authentication](authentication.md) |
[Commands](commands.md)

## Select configuration

All commands use JSONC at `~/.mcp/config.json` and `PROJECT/.mcp/config.json`.
Project entries replace same-named home entries; other home entries remain. Project
lookup uses the nearest `.mcp` directory or Git root, falling back to the current
directory. `--root DIR` selects an existing project directory and resolves relative
config paths against it, but does not change the working directory. `--config PATH`
or `MCP_CLI_CONFIG` selects only that file. `MCPORTER_CONFIG` is not read at runtime.

The reader accepts comments, trailing commas, an `mcpServers` object, stdio
commands, HTTP/SSE URLs, environment values, headers, and working directories.
Relative command paths and `cwd` resolve against the defining config directory;
absent stdio `cwd` uses that directory. HTTP endpoints, headers, `bearerToken`,
`bearerTokenEnv`, OAuth client metadata, and credential-cache paths resolve
environment values from configured `env` overrides before the caller's environment.
External configs are read only during explicit imports; legacy `imports` arrays
produce a warning and are not followed.

```json
{
  "mcpServers": {
    "echo": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-everything", "stdio"]},
    "remote": {"url": "https://example.com/mcp", "lifecycle": "keep-alive"}
  }
}
```

Replace example endpoints and commands with your own. On Windows, extensionless
commands such as `npx` resolve using `PATH` and `PATHEXT`, preferring launchers
such as `npx.cmd` over sibling Unix scripts. See [transports](transports.md) for
`protocolVersion` and remote behavior.

## Tool filters

`allowedTools` or `blockedTools` arrays filter discovery, calls, and bridge
exposure; `allowed_tools` and `blocked_tools` are aliases. Names match exact
upstream tool names, not patterns or bridge-prefixed names. An empty allowed array
hides all tools. Both fields together, or a field that is not a string array, are
errors. Raw `proxy` and direct upstream access bypass filters: they are not a
global access-control boundary.

## Manage config

```sh
mcp-cli config list --json
mcp-cli config get docs --json
mcp-cli config doctor --json
mcp-cli config add echo --command npx --arg -y --arg @modelcontextprotocol/server-everything --arg stdio --dry-run
mcp-cli config add remote https://example.com/mcp --scope project
mcp-cli config add docs-copy --copy-from cursor:docs --scope project --dry-run
mcp-cli config add docs-copy --copy-from vscode:docs --from PATH --protocol-version legacy
mcp-cli config remove remote
mcp-cli config login docs
mcp-cli config logout docs
mcp-cli config help
```

`docs`, `remote`, and `PATH` are placeholders.

### Inspect configuration

`config list` and `config get` show transport, source, arguments, and
auth/protocol/lifecycle settings. Environment and header values are redacted, but
resolved command arguments are not, including secrets expanded from environment
references. Pass secrets through the child environment instead of arguments when
supported, and review output before sharing. Human reports escape control
characters; JSON preserves values.

`config doctor` checks credential health without connecting or refreshing. It
identifies missing or invalid credentials and emits named authorization commands
for simple ASCII aliases; other names point to `auth --help` instead of embedding
untrusted text in shell commands. Inspections run in read-only batches of at most
four, with output in server-name order. A failed batch drains started inspections
and returns the first name-ordered error without partial output or another batch.

### Add, copy, or remove

`config add` and `config remove` share the JSON config with native `add` and
`remove`. They write an existing project config by default, otherwise home.
Removing a project override exposes the home entry. `--persist PATH` selects an
add destination; `--scope home|project` selects a home or project config.
`--dry-run` validates and prints a redacted definition without writing.

Copying with `--copy-from SOURCE:NAME` uses the import converter, then applies target,
environment, header, auth, description, and protocol overrides. `--from PATH`
selects the source; the destination alias may differ. Identical copies preserve
bytes and modification time; different definitions require `--replace`. Copying
does not transfer OAuth credentials.

Mutations preserve unrelated JSON fields, but writes other than unchanged imports
and copies reformat the file and remove JSONC comments. Replacement and removal
retire matching prior pool views before committing; a retirement failure leaves
config unchanged.

## Paths and environment

Default state is `~/.mcp/` on all platforms (`%USERPROFILE%\.mcp\` on Windows).
Home config and credentials are there; logs use `logs/`, Unix server sockets `run/`,
and the control socket `mcp-cli-control.sock`. Windows uses home-scoped named
pipes. Unix application socket directories are private (`0700`) and reject
symlinks. Long Unix home paths may require a shorter `MCP_CLI_HOME`.

`MCP_CLI_HOME` moves home config and credentials to its `config/` directory and
daemon state, sockets, and logs to its `state/` directory. It does not move project
config or credentials. `MCP_CLI_CONFIG` or `--config` selects an explicit config
and adjacent credential scope.

`MCP_CLI_HOME`, `MCP_CLI_CONFIG`, `MCP_CLI_DEBUG`, `MCP_CLI_NO_PROGRESS`, and
`MCP_CLI_RAW_UPSTREAM_STDERR` have corresponding `MCP_POOL_*` fallback aliases; a
set new value wins even when empty. Credential read-only aliases have a different
policy; see [authentication](authentication.md#read-only-credentials).
