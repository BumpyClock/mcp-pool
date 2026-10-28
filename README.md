# mcp-pool

Standalone, platform-independent CLI for pooling MCP (Model Context Protocol) servers,
with discovery, call, resource, and auth commands inspired by mcporter.
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

Build and install the current checkout explicitly:

```powershell
.\install.ps1
.\install.ps1 -BinDir "$HOME\local\.bin"
```

```sh
sh ./install.sh
sh ./install.sh --bin-dir "$HOME/.local/bin"
```

The installers default to `~/.local/bin`, build the host target from this
repository even when invoked elsewhere, and verify the staged executable before
replacing an existing file. A locked Windows executable fails without stopping its
daemon. The scripts do not change `PATH`; they warn if another executable would
still be selected. `mcp-pool --version` includes the Git revision and a source
fingerprint, so two different dirty builds no longer look identical.

## Development

```sh
cargo test
cargo clippy --all-targets
bacon --job run
bacon --job clippy
```

On Windows, `.\dev.ps1` starts the `bacon --job run` watcher. On macOS/Linux,
use `./dev.sh`.

## Configuration and imports

All commands use the same JSONC format at `~/.mcp/config.json` and
`PROJECT/.mcp/config.json`. Project entries override same-named home entries;
other home entries remain available. Project lookup uses the nearest `.mcp`
directory or Git root, falling back to the current directory. `--root DIR`
selects a project explicitly. `--config PATH` or `MCP_POOL_CONFIG` selects just
one file instead of merging. `MCPORTER_CONFIG` is no longer read.

Import existing definitions once, then use only the copied configuration:

```sh
mcp-pool import claude --dry-run
mcp-pool import claude
mcp-pool import codex --scope project
mcp-pool import project --scope project
mcp-pool import mcporter --from PATH
mcp-pool import cursor --scope project
mcp-pool import vscode --from PATH --dry-run
mcp-pool config import claude docs --scope project
mcp-pool import mcp-pool --from OLD_CONFIG_TOML
```

`import` and `config import` are equivalent. Imports default to the home config;
use `--scope project` for `PROJECT/.mcp/config.json`, or `--config PATH` for an
explicit destination. `--from PATH` selects a source file, and optional server
names select a subset. Existing identical definitions are safe to reimport;
different definitions require `--replace`. Conflicts leave the whole destination
unchanged. `--dry-run` makes no files or connections. Import output reports names
and paths, not credential values.

| Source | Files read during explicit import |
| --- | --- |
| `claude` / `claude-code` | `~/.claude.json`, Claude project-local entries in that file, `PROJECT/.mcp.json`, and legacy `.claude/mcp.json` / `settings.json` / `settings.local.json` files. Local overrides project, then user. |
| `claude-desktop` | The platform's `Claude/claude_desktop_config.json`. |
| `codex` | `~/.codex/config.toml` and `PROJECT/.codex/config.toml`. Project wins. `CODEX_HOME` overrides the user directory. |
| `mcporter` | `~/.mcporter/mcporter.json` and `PROJECT/config/mcporter.json`. Project wins. |
| `cursor` | `PROJECT/.cursor/mcp.json`, `~/.cursor/mcp.json`, and platform `Cursor/User/mcp.json` locations. Project wins. |
| `windsurf` | `.codeium/windsurf`, `.codeium/windsurf-next`, `.windsurf`, and platform Codeium locations containing `mcp_config.json`. Use `--from` for another location. |
| `vscode` / `vs-code` | `PROJECT/.vscode/mcp.json` and the platform's `Code/User/mcp.json` or `Code - Insiders/User/mcp.json`. Reads the VS Code `servers` container. |
| `opencode` | Project `opencode.jsonc` / `opencode.json`, platform OpenCode configuration, and supported legacy OpenAI locations. Honors `OPENCODE_CONFIG`, `OPENCODE_CONFIG_DIR`, and `OPENAI_WORKDIR` during explicit import. |
| `project` | Claude project files, `.codex/config.toml`, and `config/mcporter.json`. Cross-tool name conflicts fail rather than choosing silently. |
| `mcp-pool` | The former native `mcp-pool/config.toml`, or `--from PATH`. |

Sources are never modified, executed, or followed during normal commands.
Relative working directories and executable paths are rebased while copying.
Environment references, headers, OAuth settings, and tool filters remain in the
copy; ambient environment values are not saved. Codex disabled servers are
omitted with a warning. Unsupported authentication helpers and transports fail
explicitly instead of dropping authentication. OAuth tokens are not imported;
authorize each scope explicitly. Legacy `imports` arrays are not followed.
Literal `bearerToken` credentials and masked credential placeholders are rejected
atomically; use `bearerTokenEnv` or an unresolved environment reference instead.
After upgrading, stop an old daemon before starting pools with the new config.

Windows resolves extensionless commands such as `npx` using `PATH` and
`PATHEXT`, preferring Windows launchers such as `npx.cmd` over sibling
extensionless Unix scripts. Configs do not need a Windows-specific suffix.

Leading global flags include `--json`, `--debug`, `--config PATH`,
`--root DIR`, `--log-level LEVEL`, and `--oauth-timeout MILLISECONDS`.
Leading `--json` selects JSON output; it is different from a call's
`--json ARGUMENT_OBJECT`. `--root` resolves relative config paths against an
existing directory and selects its `.mcp` project layer and credentials.
It does not change the process working directory.
Log levels `trace` and `debug` enable diagnostic logging; `info`, `warn`,
`error`, and `silent` disable verbose diagnostics. `--oauth-timeout` bounds
explicit authorization for `auth` and `config login`, including stdio helpers.
Other commands reject it. Cached-token refresh retains its fixed OAuth deadline;
use `--timeout` for a call or discovery operation's wait budget.

```sh
mcp-pool list                         # connect and discover configured servers
mcp-pool list docs --schema           # show tool signatures and input schemas
mcp-pool describe docs                # alias for list
mcp-pool list-tools docs              # alias for list
mcp-pool list docs.search --json       # one tool's metadata
mcp-pool call docs.search query="MCP" --output json
mcp-pool call --server docs --tool search --args '{"query":"MCP"}'
mcp-pool resources docs               # list resources
mcp-pool resource docs "docs://guide"  # read a resource
```

Root `--help` exposes both MCP commands and native pool controls. `help call`
and `call --help` describe the same command. Configured servers also support
short forms:

```sh
mcp-pool docs
mcp-pool docs list --schema
mcp-pool docs.search query="MCP"
mcp-pool docs search query="MCP"
mcp-pool 'docs.search(query: "MCP")'
```

The first two discover tools; the remaining forms call a tool. A server-first
tool token is never silently ignored. Explicit commands such as `status` and
the `pool` namespace take precedence over same-named servers; use explicit
`list NAME` or `call --server NAME --tool TOOL` for those servers.

`docs`, `search`, and `docs://guide` are placeholders. Use names and URIs returned
by your configured server. Discovery connects to servers and can start stdio
processes; it is not a local config listing. Use `config list` to inspect config
without connecting, or `pool list` to list the same configured servers.

`list`, `describe`, and `list-tools` accept `--schema`, `--brief`/`--signatures`,
`--all-parameters`, `--json`, `--status`, `--sources`, and `--timeout MILLISECONDS`.
`--brief` cannot be combined with `--schema`, `--json`, or `--status`.
Discovery reports server failures but exits successfully unless `--exit-code`
is set, matching mcporter for both single-server and all-server lists. `--quiet`
suppresses stdout and stderr and enables that failure exit code. Invalid
arguments, unknown servers or tool selectors, and invalid configuration still
exit unsuccessfully.

`list SERVER` shows a formatted tool reference: wrapped descriptions, parameter
documentation, typed signatures, examples, and a transport summary. Interactive
output highlights tool names and parameters and dims explanatory text.
`--no-color` or `NO_COLOR` disables colors; redirected output contains no color
codes. The default shows up to five parameters plus any additional required
ones. A notice identifies hidden optional fields; use `--all-parameters` to
show them. `--brief` prints required-only signatures, and `--schema` adds the
complete input schemas. `list SERVER.TOOL` selects one tool; a second positional
argument is not accepted.

Interactive discovery, calls, resources, and native pool-control commands show
a waiting indicator on stderr with the current phase and elapsed time.
Discovery also shows the number of completed servers.
The indicator clears before results or errors are printed. JSON, raw, plain,
quiet, redirected output, CI, and `TERM=dumb` remain free of progress output.
Set `MCP_POOL_NO_PROGRESS=1` to disable the indicator; the existing
`MCPORTER_NO_SPINNER=1` setting is also honored. Proxy and bridge streams are
never decorated with progress output.
For MCP commands, `--plain` suppresses progress without changing the selected
result format. Native status and `pool list` use it for stable line-based rows.

`call` accepts `SERVER.TOOL`, `SERVER TOOL`, or `--server NAME --tool NAME`.
Dotted tool selectors split at the first dot: `docs.search.v2` selects server
`docs` and tool `search.v2`. This also applies to tool-specific discovery.
Arguments can use `key=value`, `key:value`, JSON through `--args`/`--params`, or
`--args -` for a JSON object on stdin. Positional arguments use the tool schema's
property declaration order. Named `key=@FILE` values read a regular UTF-8 file
as literal text, up to 16 MiB; `key=@@text` sends literal `@text`.
Tool-option flags must match schema properties or their kebab-case spelling;
exact schema property names take precedence over aliases. Unknown options are rejected.
Tool names resolve exactly first, then through a unique case-insensitive alias
that ignores punctuation. For example, `list-pages` can select upstream
`list_pages`. Ambiguous aliases fail; near-miss spellings only suggest a name and
never execute the suggested tool. Tool-specific discovery uses the same resolver.
`call SERVER.list_tools` and `call SERVER.help` fall back to discovery only when
there is no corresponding real tool. Real tools, including excluded tools, are
never bypassed by these fallbacks. Tool arguments are not accepted by a discovery
fallback.
`--raw-strings`/`--no-coerce` preserves string values instead of coercing JSON
numbers, booleans, or collections. Calls and resources accept
`--output auto|text|markdown|json|raw` and `--timeout MILLISECONDS`.
For calls, `--json` takes an argument object; use `--output json` for JSON output.
`--output raw` preserves the MCP result envelope; other formats can extract
structured content or text. An MCP error result makes the call exit unsuccessfully.
Timeout precedence is `--timeout`, then a positive `MCPORTER_CALL_TIMEOUT` or
`MCPORTER_LIST_TIMEOUT` value, then configured `timeoutMs`, then the default.
The defaults are 60 seconds for calls and 30 seconds for discovery.

The selected timeout bounds the caller's operation; it does not change pool identity.
A `list` deadline covers each server's credential preparation, pool connection,
initialization, and all discovery pages. Calls and resources instead apply the
selected duration separately to connection/initialization, paginated discovery,
and the final request; it is not a single whole-command deadline. Credential
preparation happens before their connection deadline.
Changing `--timeout` does not create another pool. A timed-out or interrupted
request closes that caller's local connection, not the shared upstream, and
does not replay the request.

`call --save-images DIR` writes returned base64 image content to collision-free
files and reports their paths on stderr. `--tail-log` prints the last 20 lines
of the absolute regular-file path returned as `logPath`, `logFile`, or
`logfile`, reading at most 1 MiB. It does not follow the log indefinitely.
The tail adds non-JSON text on stdout; omit it when parsing JSON output.

### Ad-hoc targets

Discovery, calls, and auth can select `--http-url URL` or `--stdio COMMAND`,
with `--stdio-arg ARG`, `--cwd DIR`, `--env KEY=value`, `--header KEY=value`,
`--name NAME`, and `--description TEXT` as applicable. `--stdio` tokenizes the
command before appending repeated `--stdio-arg` values. Ad-hoc HTTP URLs require
HTTPS unless `--allow-http` is explicit; URL userinfo credentials are rejected.

`--persist PATH` writes the ad-hoc definition to a JSON config through the
existing config writer. It requires an ad-hoc target and refuses to overwrite
an existing entry with the selected name. Calls persist before execution;
auth persists after successful authorization. Persistence is an explicit
config write, not an import or a separate broker.

The config reader accepts JSONC comments and trailing commas, an `mcpServers`
object, stdio commands, HTTP/SSE URLs, environment values, headers, and working
directories. Relative command paths and `cwd` resolve against the selected
config file's directory; absent stdio `cwd` uses that directory.
HTTP headers, `bearerToken`, and `bearerTokenEnv` resolve environment values
from the server's configured `env` overrides before the caller's environment.
External configs are read only by explicit `import` commands. Legacy `imports`
arrays in an owned config produce a warning and are not followed.

### Upstream protocol negotiation

Server entries accept `protocolVersion` (`protocol_version` is an alias):

| Value | Behavior |
| --- | --- |
| `auto` (default) | Probe `server/discover` for `2026-07-28`; fall back to legacy initialization only after a definitive unsupported-method/version response or an initial HTTP protocol mismatch. |
| `legacy` | Skip the modern probe and send `initialize`. Supports `2025-11-25`, `2025-06-18`, `2025-03-26`, and `2024-11-05`. |
| `2026-07-28` | Require modern discovery. No legacy fallback. |

Modern requests carry protocol-version metadata and HTTP method/name headers.
They do not send a legacy `initialize` or `notifications/initialized` handshake.
Legacy SSE uses legacy negotiation. Pools with incompatible configured protocol
preferences have distinct identities. JSON discovery reports the negotiated
`protocolVersion`, `era`, and any server instructions.

Negotiation uses the same upstream, not a temporary probe process. Authentication
failures, malformed replies, disconnects, and unanswered probes do not trigger
restarts or replay. If an older stdio server exits or ignores unknown methods,
set its `protocolVersion` to `legacy`. An operation whose outcome is unknown is
never retried automatically.

Server entries can use `allowedTools` or `blockedTools` arrays, with
`allowed_tools` and `blocked_tools` as aliases, to filter discovery, calls,
and bridge exposure. All three paths use the same filter. Names match upstream tool
names exactly, not patterns or bridge-prefixed names. An empty `allowedTools`
array hides all tools. Specifying both fields, or a field that is not an array
of strings, is an error. These filters do not constrain a raw `proxy` connection
or direct access to the upstream; they are not a global access-control boundary.

### Manage config explicitly

```sh
mcp-pool config list --json           # local definitions, without discovery
mcp-pool config get docs --json
mcp-pool config doctor --json         # config and credential status, not a live probe
mcp-pool config add echo --command npx --arg -y --arg @modelcontextprotocol/server-everything --arg stdio --dry-run
mcp-pool config add remote https://example.com/mcp --scope project
mcp-pool config add docs-copy --copy-from cursor:docs --scope project --dry-run
mcp-pool config add docs-copy --copy-from vscode:docs --from PATH --protocol-version legacy
mcp-pool --config .\.mcp\config.json config remove remote
mcp-pool config login docs            # explicit auth
mcp-pool config logout docs           # clear stored OAuth credentials
mcp-pool config help
```

`config add` and `config remove` change the selected JSON config, shared with
native `add` and `remove`. By default they write an existing project config,
otherwise the home config. Removing a project override exposes the inherited
home entry again. `--persist PATH` chooses an add destination;
`--scope home|project` selects the home or project `.mcp/config.json`.
`--dry-run` validates and prints the definition without writing; environment
and header values are redacted.
`config add --copy-from SOURCE:NAME` starts with the same normalized definition
as an explicit import, then applies the supplied target, environment, header,
auth, description, and protocol overrides. The destination alias can differ
from the source name. `--from PATH` selects the source file. Identical copies are
idempotent; a different existing destination requires `--replace`. Copying does
not read or transfer OAuth vault/cache credentials.
Config mutations preserve unrelated JSON fields but rewrite formatting and
remove JSONC comments. Use a separate file when the original must remain unchanged.
Replacement and removal retire matching prior pool views before committing.
If retirement fails, the config remains unchanged.

### Config, credentials, and isolation

`MCP_POOL_HOME` redirects the home config and home credential layer to
`MCP_POOL_HOME/config/`, and isolates daemon state, sockets, and logs. It does
not relocate project config or project credentials. Use `MCP_POOL_CONFIG` or
`--config PATH` for a fully explicit configuration and adjacent credential scope.

Set `MCP_POOL_CREDENTIALS_READ_ONLY=1` in the calling process to
allow use of valid cached credentials without refresh or credential writes.
Expired credentials cannot refresh in this mode; auth and vault mutations fail.
The read-only policy travels with the server request, including to an existing
daemon that was started without the flag.
This setting does not prevent explicit `config add` or `config remove` writes.

For example, this PowerShell sequence selects the home config explicitly while
keeping daemon state in the repository and credentials read-only:

```powershell
$env:MCP_POOL_HOME = Join-Path $PWD "target\mcp-validation"
$env:MCP_POOL_CONFIG = Join-Path $HOME ".mcp\config.json"
$env:MCP_POOL_CREDENTIALS_READ_ONLY = "1"
mcp-pool list --json --exit-code
mcp-pool daemon stop
```

Authentication is explicit. `list`, `call`, resources, and `proxy` do not open
a consent flow. They can use existing credentials; normal operation can refresh
an expired token when a refresh token is available. Missing credentials require
`mcp-pool auth SERVER` rather than automatic authorization. Provider availability,
account permissions, and consent are not guaranteed by CLI compatibility.
Discovery, calls, and resources accept `--no-oauth` to use valid cached OAuth
tokens without refresh. This does not remove headers explicitly supplied in config.

```sh
mcp-pool auth docs
mcp-pool auth docs --no-browser        # do not launch a browser automatically
mcp-pool vault set docs --tokens-file tokens.json
mcp-pool vault clear docs
```

Credentials use `~/.mcp/credentials.json` and
`PROJECT/.mcp/credentials.json`. Project credentials override home credentials
independently of where a server is defined, so a project can authenticate an
inherited home server as a different account without copying its definition.
An expired or invalid project credential never falls back to a home account.
Refresh updates the selected credential store, not a copy of a rotating token.
`auth`, `vault set`, and logout/clear target the active project scope;
`--config PATH` targets credentials beside that config. To authorize at home,
run `mcp-pool --config ~/.mcp/config.json auth SERVER`.
Clearing a project credential restores home fallback. Credential scopes also
separate pooled connections, including projects using different accounts.
Explicit `tokenCacheDir` still opts into that cache and can intentionally share
credentials; legacy mcporter credential paths are not discovered automatically.
Do not use `auth --reset`, `vault set`, `vault clear`, or config login/logout
against credentials that must remain unchanged. Pass token material through a
file or stdin, not command-line arguments. Never commit credential files.
Explicit auth and credential mutations retire matching pooled views first.
This can disconnect their clients; reconnect after authorization or a vault change.

`vault set` accepts either a token object or a credential object containing
`tokens`. Tokens require a nonempty `access_token`. The `token_type` field must
identify Bearer authorization.
Use `--stdin` instead of `--tokens-file` to read the JSON from stdin.
If supplying expiration, use absolute Unix seconds in `expires_at` or
`expiresAt`. `vault set` converts `expires_in` to absolute expiration at import
time, so use a relative duration only for newly issued token responses.
Existing cache entries with `expires_in` but no absolute expiration are treated
as expired because their issue time cannot be reconstructed safely.
A vault payload containing a refresh token must also contain its legitimate,
bound `clientInfo` registration. Refresh needs associated issuer metadata;
an access token alone does not establish a refreshable OAuth session.

Credential stores reconcile partial writes using committed generation ordering,
not token expiration. Registration metadata is not combined across different
token generations. Conflicting legacy rotating refresh tokens without verified
ordering fail closed. Use explicit `auth SERVER --reset` to reauthorize only
when authorized to replace those credentials; the pool does not guess or replay
an older refresh token.

HTTP authorization uses OAuth metadata discovery, PKCE S256, and a loopback
callback. `--no-browser` displays the URL and still waits for that callback;
it does not replace authorization with a token-printing operation.
The issuer must advertise PKCE S256 support; otherwise authorization fails.
Existing registered client information can be reused. A provider
without dynamic client registration needs a legitimate configured
`oauthClientId` or a supported HTTPS client metadata document. Unsupported
signed client-authentication methods fail explicitly. This does not provide
credentials for a provider or bypass its consent and registration requirements.

### Daemon control

```sh
mcp-pool daemon start
mcp-pool daemon status --json
mcp-pool daemon restart
mcp-pool daemon stop
```

These commands use the same native pool broker, not a second mcporter daemon.
`daemon status` checks the existing daemon without launching one; native
`status` can auto-launch it. `daemon stop` retires owned pools and disconnects
their clients. `daemon restart` waits for retirement before replacement.

`daemon start` and `daemon restart` accept `--foreground`, `--log`,
`--log-file PATH`, and `--log-servers NAME,...`. Launch options apply when
starting a daemon, not when attaching to an already running one; use restart
to change them. These options retain the same native broker.
`--log` uses `logs/daemon.log` in the state directory unless `--log-file` selects
another path. The sink rotates its retained tail at a 5 MiB threshold.
`--log-servers` selects exact logical configured names, not hashes or executable
names. General daemon records remain available; unidentified server records are
suppressed when a server filter is active.

`daemon migrate --json` inventories observed legacy owners as `pid` and
`verified` records. Config conversion is a separate explicit `import` operation.
Legacy retirement requires both `--stop-legacy` and `--confirmed-drained`;
drain clients and obtain authorization before invoking it. Retirement requires
a same-user local handshake and verified process identities/tree ownership.
Unverified owners are not stopped. The CLI asks the verified legacy runtime
to stop, records a retirement journal, and confirms process-tree retirement.
It does not signal arbitrary PIDs or guess ownership. An unverified retirement
retains the journal and blocks cutover rather than launching a replacement.

### Serve an MCP bridge

Top-level `serve` exposes selected keep-alive servers through an MCP bridge.
It is different from `pool serve`, which runs the pool daemon's control socket.

```sh
mcp-pool serve --stdio --servers docs
mcp-pool serve --http 3333 --servers docs
```

Stdio is the default. HTTP accepts `--http PORT` and an optional `--host HOST`;
the default host is `127.0.0.1`. Keep the bridge on loopback unless its access is
controlled separately. The HTTP bridge has no downstream authentication.
Binding a non-loopback `--host` exposes its tools without authentication.
`--stdio` and `--http` are mutually exclusive, and
`--host` requires HTTP mode.

`--servers` is a comma-separated selection, not a switch that enables keep-alive.
Selected entries must qualify for keep-alive, for example through a
`"lifecycle": "keep-alive"` field on each server definition. Without
`--servers`, the bridge selects qualifying entries automatically. An empty
selection is an error. `MCPORTER_KEEPALIVE` can enable named entries or `*`;
`MCPORTER_DISABLE_KEEPALIVE` and `MCPORTER_NO_KEEPALIVE` can disable them.

Bridge tool names include their server, such as `docs__search`. Reserved name
separators are escaped. Use the names returned by the bridge's `tools/list`
rather than constructing names for unusual server or tool identifiers.

HTTP mode exposes the combined bridge at `http://127.0.0.1:3333/mcp` in the
example above. `/mcp/docs` exposes only that selected server with bare tool
names, such as `search`. It accepts Streamable HTTP POSTs with JSON responses.
The bridge supports older `initialize` negotiation and modern `server/discover`
requests using protocol version `2026-07-28`. Modern HTTP requests require
`MCP-Protocol-Version` and a matching `MCP-Method` header, plus `MCP-Name` when
`params.name` is present. The same version must appear in
`params._meta["io.modelcontextprotocol/protocolVersion"]`.
Ordinary requests require `Accept: application/json, text/event-stream` and
return JSON. Notifications receive an empty HTTP 202 response.

Modern HTTP `subscriptions/listen` accepts a `notifications` filter containing
`"toolsListChanged": true` and returns a POST SSE stream. It requires
`Accept: text/event-stream`. The stream sends a subscription acknowledgement,
then `notifications/tools/list_changed` messages tagged with that subscription's
ID. Graceful bridge shutdown sends a completion result. Each subscribed server
gets a dedicated local pool connection for notifications, separate from the
bridge's request connection, but both attach to the same upstream. Listeners
can reconnect to the pool without replaying requests or missed events.
Tool-list invalidations can be coalesced when the stream's queue is full.

This is a sessionless tool bridge, not a complete MCP implementation or a claim
of full mcporter `serve` parity. It creates no session IDs; a supplied session
ID is rejected. GET and DELETE are unsupported. Subscription SSE is limited to
tool-list changes, not arbitrary notifications or related messages for ordinary
tool calls. Stdio supports `server/discover` but does not expose subscription
streams. The bridge advertises tools only, not resources, prompts, or elicitation.
It acknowledges downstream notifications without forwarding them and does not
relay upstream sampling or roots requests to bridge clients.
Use the direct `resource` command or
`proxy` when you need the corresponding upstream protocol surface.

HTTP bodies and stdio frames are capped at 1 MiB. HTTP allows 64 in-flight
requests, including open subscription streams; excess requests receive HTTP 503.
There is also a limit of 64 dedicated notification connections across
subscriptions, and each SSE message is capped at 1 MiB.
The per-server operation timeout includes queue-wait time and reconnect setup,
uses configured `timeoutMs`, and defaults to 60 seconds. It has no 120-second cap.
An established subscription has no idle operation deadline. Bridge clients
still share the pooled upstream's state; an HTTP endpoint does not provide
per-client state isolation.

### Command scope

The command surface includes `list`/`describe`/`list-tools`, `auth`, `call`,
`resource`/`resources`, `vault set|clear`,
`config list|get|add|remove|import|login|logout|doctor|help`, `import`, `daemon`, and `serve`.
mcporter is the command and config inspiration, not a second runtime or a promise
of complete parity.
Imports copy definitions; they do not enable live editor-config dependencies.
`emit-ts`, `generate-cli`, `inspect-cli`, `record`, `replay`, and an SDK are
outside this implementation's scope.
Pools remain owned by the persistent native daemon, not an ephemeral per-command
runtime. This lifetime is intentional:
commands and agent clients reuse the same daemon and multiplexer, without a
second mcporter-style broker. Concurrent routing, verified retirement, and
the prohibition on replaying unknown outcomes remain part of the pool contract.

Output formatting is an adapter, not a separate execution path. `--output raw`
deliberately emits a JSON result envelope rather than Node inspect text.
Consumers can parse that output as JSON; byte-for-byte text parity is not promised.

## Native pool usage

Native lifecycle commands manage the pool daemon using the same `.mcp` JSON config.
Configuration is resolved by each caller, not the daemon's launch directory.
Daemon launch does not execute configured servers automatically; use `start`
without a name to start all servers in the caller's merged configuration.
The former top-level native `list` and `serve` are now `pool list` and
`pool serve`; top-level `list` performs live MCP discovery, and top-level
`serve` is the compatibility MCP bridge.

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
mcp-pool pool list         # configured servers; no discovery
mcp-pool remove echo

# Run the daemon explicitly (otherwise auto-launched on first command)
mcp-pool pool serve

# Bridge an agent's stdio to a pool socket. Put this in the agent's MCP config:
#   command = "mcp-pool", args = ["proxy", "echo"]
# proxy is self-starting: if the upstream is already running it just attaches
# (shared), otherwise it auto-starts the upstream (and the daemon) first. No
# separate `start` step is required — `start` is only for pre-warming.
mcp-pool proxy echo
```

Set `MCP_POOL_DEBUG=1` to enable diagnostic logging to the state dir.

`proxy NAME` and `pool proxy NAME` both resolve the merged `.mcp` configuration,
including scoped credentials. Both accept `--config` and `--root`.
No legacy TOML or mcporter configuration is read as a fallback.

## Local endpoint security

Unix sockets use mode `0600` and verify same-user peers. Application socket
directories use mode `0700`. Binding inside a private directory does not change
the process-wide umask; sticky-shared parents still require a temporary restrictive
umask during bind.

Windows named pipes use an owner-only DACL, reject remote clients, and verify the
server's user. Cancelled accepts retain their pending instance. A client that
disconnects before accept does not prevent later clients from connecting.

## Lifecycle and readiness

`start` confirms upstream transport setup, not MCP initialization. Status reports
`starting` during setup, `running` after setup, and `stopping` during retirement.
The table's readiness column distinguishes a bound local `socket`, an available
upstream `transport`, and a received `mcp` initialization result. JSON status
includes these three readiness flags and startup or retirement errors.
Compatibility `list` reports completed tool discovery or a discovery error;
it is not the native pool lifecycle status. A pool marked `running` can still
fail discovery or lack an initialization result.

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
clients; agents must reconnect. Pool identities include the configured name,
config source, credential scope, and resolved definition. Status shows logical
names and includes the internal identity as `pool_name` in JSON.
Configuring the same command under two names,
or resolving it through different config files, can create separate pools.

## Remote transports

`--transport http` uses Streamable HTTP POST requests. Initialization captures
the session ID and negotiated protocol version for later requests. JSON responses
and incremental SSE responses are supported. `--transport sse` uses the legacy
GET event stream, discovers its same-origin message endpoint, and sends POSTs
there. It does not use a POST-only approximation of legacy SSE.

HTTP transport limits are 32 concurrent requests and 1 MiB per JSON response or
SSE frame. The default request deadline is 60 seconds and the default stream
read deadline is 30 seconds; configured `timeoutMs` changes these budgets.
MCP command requests carry their caller's operation budget through the local
pool to the HTTP backend, rather than inheriting a fixed 60-second ceiling.
Coalesced initialization and tool discovery use a shared transport deadline
as described below. Empty 202/204 responses are accepted for notifications.
Transport failures produce
JSON-RPC errors with the requesting client's original ID.

Deliberate HTTP shutdown sends a session DELETE when a session ID exists. DELETE
has a two-second deadline. Unsupported DELETE or a timeout is logged without
blocking confirmed local retirement. Remote cleanup is not guaranteed.

An expired HTTP session returns an error without replaying the request. Restart
the pool and reconnect before issuing new requests. Optional Streamable HTTP GET
streams and resumable event replay are not implemented. Servers resolved from
the `.mcp` config can use configured headers and scoped OAuth credentials.
OAuth consent remains an explicit `auth` operation.

## Shared session contract

A pool shares one MCP session, not just one executable. Clients have separate
JSON-RPC request IDs, but they do not have separate upstream state. A tool that
selects a browser page, changes a working directory, or modifies server settings
can affect other clients. Coordinate stateful select-then-act sequences between
agents. Request multiplexing does not make those sequences atomic.

Successful `initialize` and unpaginated `tools/list` results are cached for later
clients. A `notifications/tools/list_changed` notification invalidates tool
discovery. Upstream notifications are broadcast to connected clients.

Concurrent initialization and cacheable, unpaginated `tools/list` requests share
one upstream request. Each MCP command caller keeps its own local deadline.
A longer-lived follower can extend the shared upstream wait, including HTTP
response headers and JSON/SSE body reads, without resending the leader's request.
The shared wait also retains the backend's default or configured budget as a
minimum. A short caller's timeout or disconnect does not end another caller's
wait or retire the upstream. Tool calls are not coalesced.

Pending routes have a five-minute cleanup budget by default. Longer caller
budgets and shared discovery deadlines can retain routes beyond that interval;
this cleanup budget is not the caller's operation timeout. Expired shared
requests release their waiters. A later explicit request can start a new
initialization or discovery attempt; timed-out operations are never replayed
automatically.

Responses route only by the exact upstream request ID. An uncorrelated response
with an empty ID is not assigned to another client's oldest pending request.
HTTP POST failures can still return an error for their known requesting client.

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
- `server_config.rs` — selected JSONC config and resolved definitions
- `mcp_cli.rs` / `tool_arguments.rs` / `tool_output.rs` / `tool_discovery.rs` — MCP command parsing, output, and discovery
- `config_commands.rs` / `daemon_commands.rs` — JSON config operations, auth, vault, and daemon commands
- `mcp_client.rs` / `mcp_client_wire.rs` — MCP requests through pooled local connections
- `mcp_bridge.rs` / `mcp_bridge_*.rs` — keep-alive tool bridge over stdio or HTTP
- `tool_filter.rs` — shared tool filtering for discovery, calls, and bridge exposure
- `oauth.rs` / `oauth_*.rs` — explicit authorization, credential reuse, storage, and refresh
- `upstream.rs` — upstream ownership and confirmed shutdown contract
- `upstream_stdio.rs` / `upstream_http.rs` — stdio and remote transport backends
- `jsonrpc.rs` — JSON-RPC id translation (per-client ids rewritten to pool-unique ids)
- `socket_proxy.rs` / `socket_proxy_*.rs` — lifecycle, client handling, routing, and discovery caching
- `pool.rs` — registry of pooled servers + socket discovery for daemon reattach
- `control.rs` / `daemon.rs` — control protocol + long-lived daemon
- `daemon_client.rs` — daemon control requests and pool startup
- `proxy.rs` — per-agent stdio bridge to a pool socket (self-starts the upstream)
- `cli.rs` / `cli_output.rs` / `main.rs` — subcommand dispatch and output
