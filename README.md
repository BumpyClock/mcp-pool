# mcp-cli

`mcp-cli` is a standalone, platform-independent CLI and MCP server for pooling
MCP (Model Context Protocol) servers. It offers discovery, call, resource, and
auth commands inspired by mcporter. Each pooled server runs once as an upstream.
Multiple agent clients share it over a local socket, with JSON-RPC requests
multiplexed by `id`.

## Why

Tools like `claude-code` spawn their configured MCP servers for each session.
When several agent sessions run in parallel, each can launch its own copy of the
same server, such as an `npx` server. `mcp-cli` lets those sessions share one
upstream.

## Install / build

```sh
cargo build --release
# binary at target/release/mcp-cli
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
replacing an existing file. Hashing or staged-validation failure retains the
previous executable and its permissions. A locked Windows executable fails without
stopping its daemon. The scripts do not change `PATH`; they warn if another executable would
still be selected. `mcp-cli --version` includes the Git revision and a source
fingerprint, so two different dirty builds no longer look identical.
`MCP_CLI_BIN_DIR` selects the default install directory; `MCP_POOL_BIN_DIR`
remains a fallback. An explicit `--bin-dir` or `-BinDir` takes precedence.

## Development

```sh
cargo test
cargo clippy --all-targets
bacon --job run
bacon --job clippy
```

On Windows, `.\dev.ps1` starts the `bacon --job run` watcher. On macOS/Linux,
use `./dev.sh`.

The [CLI UX audit](docs/cli-ux-audit.md) compares every supported command family
with mcporter using isolated local fixtures and retains command transcripts.
With `ast-grep` installed, `python3 scripts/rust_codebase_map.py --format dot`
regenerates the syntax dependency graph; `--format skeleton` lists function
signatures.

### Reproducible opt-in push checks

The checked-in toolchain pins Rust 1.95.0 with rustfmt and Clippy. Before
opting in, prepare that toolchain, the locked dependencies, and Python 3.11+:

```sh
rustup toolchain install 1.95.0 --profile minimal --component rustfmt --component clippy
cargo fetch --locked
python3 scripts/test_pre_push.py -v
./scripts/install-hooks.sh
```

The tracked executable hook is never installed automatically. The installer is idempotent, supports linked worktrees and refuses unrelated active hooks or `core.hooksPath`. It changes repository-local configuration only. Default hooks behind symlinked directories are preserved. Narrow `.gitattributes` rules keep the shell entrypoint and installer on LF under `core.autocrlf`; Git-status errors fail closed.

Push validation requires a clean checkout, including untracked files. Every nondeleted pushed ref must resolve to checked-out `HEAD`; other refs fail rather than validating unrelated code. Multiple refs, new branches, missing base objects, empty input/diffs, renames and deletions broaden checks conservatively. A deletion-only update runs no project commands. No fetching, stashing or source rewriting occurs.

The hook uses the already-installed pinned Rust toolchain, full project `cargo fmt --all -- --check`, and `cargo check --offline --locked --bin mcp-cli`. Missing tools, components or dependencies fail with setup instructions; the hook never installs or downloads them. Source, dependency, shared configuration and uncertain changes run the existing Unix security/transport filter serially on Unix. Socket fixtures use a fresh private, short `/tmp` MCP home, cleaned even on failure, never the user's MCP state. Markdown-only changes retain format/check but omit the IPC runtime gate. All failures propagate.

Unix tests are not Windows acceptance. A non-Unix hook host reports that Unix IPC remains required in CI; Windows named-pipe/process behavior and the full suite still need native qualification. The hook does not invoke MCP commands, refresh tokens, launch providers, import credentials or restart daemons. Its fixture tests use stub commands in disposable Git repositories and make no outbound push. Run full project tests separately.

The existing `unix-ipc` workflow uses Boat. It runs on pull requests and on manual dispatch. It reads the compiler pin from `rust-toolchain.toml`, prepares Python, and runs hook fixtures plus `bash scripts/test_unix_ipc.sh`. The same executable gate can run locally without parsing workflow YAML. Owner-only public execution follows the separately configured actor policy; outsiders and bots are not implicitly authorized.

## Configuration and imports

All commands use the same JSONC format at `~/.mcp/config.json` and
`PROJECT/.mcp/config.json`. Project entries override same-named home entries;
other home entries remain available. Project lookup uses the nearest `.mcp`
directory or Git root, falling back to the current directory. `--root DIR`
selects a project explicitly. `--config PATH` or `MCP_CLI_CONFIG` selects just
one file instead of merging. `MCPORTER_CONFIG` is no longer read.

Import existing definitions once, then use only the copied configuration:

```sh
mcp-cli import claude --dry-run
mcp-cli import claude
mcp-cli import codex --scope project
mcp-cli import project --scope project
mcp-cli import mcporter --from PATH
mcp-cli import mcporter --include-auth --dry-run
mcp-cli import mcporter --include-auth
mcp-cli import cursor --scope project
mcp-cli import vscode --from PATH --dry-run
mcp-cli config import claude docs --scope project
mcp-cli import mcp-pool --from OLD_CONFIG_TOML
```

`import` and `config import` are equivalent. Imports default to the home config;
use `--scope project` for `PROJECT/.mcp/config.json`, or `--config PATH` for an
explicit destination. `--from PATH` selects a source file, and optional server
names select a subset. If the imported definitions leave an existing destination
unchanged, its bytes and modification time are preserved, including JSONC
comments, a BOM, and whitespace. Different definitions require `--replace`.
Conflicts leave the whole destination unchanged. `--dry-run` makes no files or
connections. Import output reports names and paths, not credential values.

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
explicitly instead of dropping authentication. OAuth tokens are not imported by
default; authorize each scope explicitly or use the mcporter option below.
Legacy `imports` arrays are not followed.
Literal `bearerToken` credentials and masked credential placeholders are rejected
atomically; use `bearerTokenEnv` or an unresolved environment reference instead.
After upgrading, stop an old daemon before starting pools with the new config.

`import mcporter --include-auth` also copies cached HTTP/SSE OAuth tokens and their
bound client registration. It reads `~/.mcporter/credentials.json`, or
`$XDG_DATA_HOME/mcporter/credentials.json` when XDG_DATA_HOME is absolute. Configured
environment overrides take precedence. Source home follows effective `HOME`, then
`USERPROFILE`, independently of the destination scope. Explicit `tokenCacheDir` stores and legacy
`~/.mcporter/NAME` caches are supported. `--from` selects definitions, not a
credential file. Other import sources and stdio helper credentials are unsupported.

Credentials go into `credentials.json` beside the destination config, not the
config itself. With `--include-auth`, copied remote definitions drop `tokenCacheDir`
to prevent future writes to mcporter caches. Sources stay untouched. Import makes
no network requests and starts no consent flow. Expiration times stay unchanged;
expired tokens still need refresh or explicit authorization on first use.

Text and JSON reports show per-server auth status (`import`, `unchanged`, `missing`,
or `unsupported`), never credential values. Missing auth does not stop definition
imports. Different destination credentials, client registrations, and unfinished
authorization state require `--replace`; identical copies are idempotent.
Destinations that alias source credential files, including symlinks, are rejected. `--dry-run` reads and validates without writing or retiring pools.
Credential writes obey `MCP_CLI_CREDENTIALS_READ_ONLY=1` and retire affected pool
views first. Preflight validation and conflict errors leave destination files unchanged.
Config and credential files are committed separately. A later credential I/O failure
can leave the config and earlier credentials imported; fix the store and rerun.
An unchanged config still permits credential imports. Required credential checks,
pool retirement, and credential writes run even when the config is not rewritten.

**Cutover warning.** Copied refresh tokens may rotate. Stop using imported servers
through mcporter after cutover, or one runtime can invalidate the other runtime's
credentials. `--replace` can also overwrite newer destination tokens with an older
source copy. If the source is stale, authorize through mcp-cli instead.

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
mcp-cli list                         # connect and discover configured servers
mcp-cli list docs --schema           # show tool signatures and input schemas
mcp-cli describe docs                # alias for list
mcp-cli list-tools docs              # alias for list
mcp-cli list docs.search --json       # one tool's metadata
mcp-cli call docs.search query="MCP" --output json
mcp-cli call --server docs --tool search --args '{"query":"MCP"}'
mcp-cli resources docs               # list resources
mcp-cli resource docs "docs://guide"  # read a resource
```

Human `list` output starts with a version and server-count header. Each row shows
the server name, tool count or failure status, and elapsed time. Healthy rows also
show configured descriptions. Authentication failures include a named OAuth login command; HTTP 403 also
points to account permissions. A final summary counts healthy servers, auth
requirements, offline servers, HTTP errors, and other errors. Terminal output
uses bold names, muted details, yellow auth warnings, and red failures.
`--no-color`, `NO_COLOR`, redirected output, and `TERM=dumb` disable color.
`--quiet` emits no output. JSON and detailed server/tool documentation retain their
formats; JSON keeps the underlying diagnostic in `issue.rawMessage`.
Authentication failures do not trigger protocol fallback or request replay. The
legacy-protocol hint is reserved for failed discovery probes that receive no
JSON-RPC error response.

Root `--help` exposes both MCP commands and native pool controls. `help call`
and `call --help` describe the same command. Nested commands such as
`config get --help`, `daemon start --help`, and `vault set --help` show their
own flags and examples without loading configuration. Configured servers also
support short forms:

```sh
mcp-cli docs
mcp-cli docs list --schema
mcp-cli docs.search query="MCP"
mcp-cli docs search query="MCP"
mcp-cli 'docs.search(query: "MCP")'
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
codes. On Windows, listing colors require virtual-terminal support. The command
enables it without clearing other console mode flags; if this fails, it reports
the error and prints plain text. JSON and quiet listings do not change console
modes. Examples preserve enum value types through local schema references,
including array items. Cyclic or unresolved references use generic placeholders,
not validated argument values. The default shows up to five parameters plus any
additional required ones. Each tool names its hidden optional fields; use
`--all-parameters` to show them. `--brief` prints required-only signatures, and
`--schema` adds the complete input schemas. `list SERVER.TOOL` selects one tool;
a second positional argument is not accepted.

Interactive discovery, calls, resources, and native pool-control commands show
a waiting indicator on stderr with the current phase and elapsed time.
Discovery also shows the number of completed servers.
The indicator clears before results or errors are printed. JSON, raw, plain,
quiet, redirected output, CI, and `TERM=dumb` remain free of progress output.
Set `MCP_CLI_NO_PROGRESS=1` to disable the indicator; the existing
`MCPORTER_NO_SPINNER=1` setting is also honored. Proxy and bridge streams are
never decorated with progress output.
For MCP commands, `--plain` suppresses progress without changing the selected
result format. Native status and `pool list` use it for stable line-based rows.

`call` accepts `SERVER.TOOL`, `SERVER TOOL`, or `--server NAME --tool NAME`.
Dotted tool selectors split at the first dot: `docs.search.v2` selects server
`docs` and tool `search.v2`. This also applies to tool-specific discovery.
The inferred command form instead chooses the longest configured server prefix.
Use `--server NAME --tool NAME` to make dotted server names unambiguous.
Arguments can use `key=value`, `key:value`, JSON through `--args`/`--params`, or
`--args -` for a JSON object on stdin. Positional arguments use the tool schema's
property declaration order. Named `key=@FILE` values read a regular UTF-8 file
as literal text, up to 16 MiB; `key=@@text` sends literal `@text`.
Tool-option flags must match schema properties or their kebab-case spelling;
exact schema property names take precedence over aliases. Ambiguous kebab-case
aliases and unknown options fail before dispatch. Local schema references use the
same argument types as equivalent inline properties. Discovery and bridge catalogs
omit missing or empty tool names and retain the first entry for each exact name.
Tool names resolve exactly first, then through a unique case-insensitive alias
that ignores punctuation. For example, `list-pages` can select upstream
`list_pages`. Ambiguous aliases fail; near-miss spellings only suggest a name and
never execute the suggested tool. Tool-specific discovery uses the same resolver.
`call SERVER.list_tools` and `call SERVER.help` fall back to discovery only when
there is no corresponding real tool. Real tools, including excluded tools, are
never bypassed by these fallbacks. Tool arguments are not accepted by a discovery
fallback. It renders the already-discovered catalog without reconnecting or
traversing pages again. Its duration and deadlines cover that initial call-side
discovery, not a second list operation.
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
HTTP endpoints, headers, `bearerToken`, and `bearerTokenEnv` resolve environment
values from the server's configured `env` overrides before the caller's environment.
OAuth client metadata and credential-cache paths use the same precedence.
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

For example, the 1Password desktop `1password-mcp` server can exit with
`ExpectedInitializeRequest` when probed with `server/discover`, surfaced by the
client as EOF. Set `protocolVersion` explicitly for that server:

```json
{"mcpServers":{"1password":{"command":"1password-mcp","protocolVersion":"legacy"}}}
```

Then run `mcp-cli list 1password` to verify tool discovery. This changes only
negotiation; it does not authorize access to 1Password Environments.

Server entries can use `allowedTools` or `blockedTools` arrays, with
`allowed_tools` and `blocked_tools` as aliases, to filter discovery, calls,
and bridge exposure. All three paths use the same filter. Names match upstream tool
names exactly, not patterns or bridge-prefixed names. An empty `allowedTools`
array hides all tools. Specifying both fields, or a field that is not an array
of strings, is an error. These filters do not constrain a raw `proxy` connection
or direct access to the upstream; they are not a global access-control boundary.

### Manage config explicitly

```sh
mcp-cli config list --json           # local definitions, without discovery
mcp-cli config get docs --json
mcp-cli config doctor --json         # config and credential status, not a live probe
mcp-cli config add echo --command npx --arg -y --arg @modelcontextprotocol/server-everything --arg stdio --dry-run
mcp-cli config add remote https://example.com/mcp --scope project
mcp-cli config add docs-copy --copy-from cursor:docs --scope project --dry-run
mcp-cli config add docs-copy --copy-from vscode:docs --from PATH --protocol-version legacy
mcp-cli --config .\.mcp\config.json config remove remote
mcp-cli config login docs            # explicit auth
mcp-cli config logout docs           # clear stored OAuth credentials
mcp-cli config help
```

`config list` and `config get` show transport details, command arguments, source,
and configured auth/protocol/lifecycle settings. Environment and header values
are redacted; command arguments are resolved and unredacted, including secrets
expanded from environment references. For servers that support it, pass secrets
through the child environment rather than command arguments. Human config
reports escape control characters for display; JSON preserves the values.
Review output before sharing it.

`config doctor` prints readable per-server credential health without connecting
or refreshing tokens. Servers without explicit OAuth configuration and without
usable cached credentials are labeled accordingly. Missing or invalid tokens
are described as having no usable credentials. For scripts, use
`config doctor --json`; its structured report is unchanged. The default report
is human-readable, no longer JSON objects printed one per line.
Doctor emits named authorization commands for simple ASCII aliases. For other
names it points to `auth --help` rather than embedding untrusted configuration
text in shell commands. Credential inspections run in read-only batches of at
most four. Reports remain in server-name order. A failed batch drains its started
inspections before returning the first name-ordered error, without partial output
or starting another batch.

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
from the source name. `--from PATH` selects the source file. An identical copy
preserves the existing destination's bytes and modification time; a different
existing destination requires `--replace`. Copying does not read or transfer
OAuth vault/cache credentials.
Config mutations preserve unrelated JSON fields. Except for unchanged imports
and copies, writes reformat the file and remove JSONC comments. Use a separate
file when the original must remain unchanged.
Replacement and removal retire matching prior pool views before committing.
If retirement fails, the config remains unchanged.

### Config, credentials, and isolation

Defaults use `~/.mcp/` on every platform (`%USERPROFILE%\.mcp\` on Windows).
Home config and credentials remain `config.json` and `credentials.json` in that
directory. Logs use `~/.mcp/logs/`. Unix server sockets use `~/.mcp/run/`, and
the control socket is `~/.mcp/mcp-cli-control.sock`. Windows endpoints remain
home-scoped named pipes, not filesystem sockets. Unix socket directories are
private (`0700`); symlinked application socket directories are rejected.

Before upgrading, stop the old daemon with the old executable
(`mcp-pool daemon stop`) before starting `mcp-cli`. Repeat this for each original
`MCP_POOL_HOME` override with that same value. Socket names changed, so the
new CLI cannot discover or retire old-brand daemons. Starting both can create
duplicate upstreams that still share `.mcp` config and credentials. Old runtime
state is not migrated. Config and credentials stay in `.mcp`; no data is moved
or deleted. Long Unix home paths can still require a shorter `MCP_CLI_HOME`.
The legacy `import mcp-pool` selector, `_mcp_pool_timeout_ms` local wire field,
`__mcp_pool_commit` credential metadata, and `mcp-pool-legacy-v1` retirement
journals retain their existing names for compatibility.

`MCP_CLI_HOME` redirects the home config and home credential layer to
`MCP_CLI_HOME/config/`, and keeps daemon state, sockets, and logs under
`MCP_CLI_HOME/state/`. The override does not relocate project config or project
credentials. Use `MCP_CLI_CONFIG` or `--config PATH` for a fully explicit
configuration and adjacent credential scope.

Set `MCP_CLI_CREDENTIALS_READ_ONLY=1` in the calling process to
allow use of valid cached credentials without refresh or credential writes.
`MCP_CLI_HOME`, `MCP_CLI_CONFIG`, `MCP_CLI_DEBUG`, `MCP_CLI_NO_PROGRESS`, and
`MCP_CLI_RAW_UPSTREAM_STDERR` are primary names; corresponding `MCP_POOL_*`
names remain fallback aliases. A set new value wins, even when empty. For
read-only policy, either `MCP_CLI_CREDENTIALS_READ_ONLY=1` or
`MCP_POOL_CREDENTIALS_READ_ONLY=1` enforces restriction; setting the other to
`0` cannot weaken it.
Expired credentials cannot refresh in this mode; auth and vault mutations fail.
The read-only policy travels with the server request, including to an existing
daemon that was started without the flag.
This setting does not prevent explicit `config add` or `config remove` writes.

For example, this PowerShell sequence selects the home config explicitly while
keeping daemon state in the repository and credentials read-only:

```powershell
$env:MCP_CLI_HOME = Join-Path $PWD "target\mcp-validation"
$env:MCP_CLI_CONFIG = Join-Path $HOME ".mcp\config.json"
$env:MCP_CLI_CREDENTIALS_READ_ONLY = "1"
mcp-cli list --json --exit-code
mcp-cli daemon stop
```

Authentication is explicit. `list`, `call`, resources, and `proxy` do not open
a consent flow. They can use existing credentials; normal operation can refresh
an expired token when a refresh token is available. Missing credentials require
`mcp-cli auth SERVER` rather than automatic authorization. Provider availability,
account permissions, and consent are not guaranteed by CLI compatibility.
Discovery, calls, and resources accept `--no-oauth` to use valid cached OAuth
tokens without refresh. This does not remove headers explicitly supplied in config.

```sh
mcp-cli auth docs
mcp-cli auth docs --no-browser        # do not launch a browser automatically
mcp-cli vault set docs --tokens-file tokens.json
mcp-cli vault clear docs
```

Credentials belong to the config file that defines the selected server.
Servers from `~/.mcp/config.json` use `~/.mcp/credentials.json` from every project.
Servers from `PROJECT/.mcp/config.json` use `PROJECT/.mcp/credentials.json`.
A same-named project definition replaces the home definition and uses only its
project credentials. Missing, expired, or cleared project credentials do not
fall back to a home account. Removing the project definition restores the home
definition and its home credentials.
Login, refresh, credential inspection, `vault set`, and logout/clear all use the
defining config's store. `--config PATH` uses credentials beside that config;
`--root` selects project definitions without relocating inherited home credentials.
Home-defined servers share pooled connections across projects. Project definitions
and explicit configs retain separate pooled identities and credentials.
Existing credentials saved in a project by older versions are not moved automatically.
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
with dynamic registration uses an advertised client authentication method,
preferring `none`, then `client_secret_basic`, then `client_secret_post`.
Absent an advertised method list, registration uses `client_secret_basic`.
`oauthTokenEndpointAuthMethod` overrides this selection. Client secrets issued
by registration are retained for code exchange and refresh.
A provider without dynamic client registration needs a legitimate configured
`oauthClientId` or a supported HTTPS client metadata document. Unsupported
signed client-authentication methods fail explicitly. This does not provide
credentials for a provider or bypass its consent and registration requirements.

### Daemon control

```sh
mcp-cli daemon start
mcp-cli daemon status --json
mcp-cli daemon restart
mcp-cli daemon stop
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
mcp-cli serve --stdio --servers docs
mcp-cli serve --http 3333 --servers docs
mcp-cli serve --http 3333 --servers docs --concurrency 8
```

Stdio is the default. HTTP accepts `--http PORT` and an optional `--host HOST`;
the default host is `127.0.0.1`. Keep the bridge on loopback unless its access is
controlled separately. The HTTP bridge has no downstream authentication.
Binding a non-loopback `--host` exposes its tools without authentication.
`--stdio` and `--http` are mutually exclusive, and
`--host` requires HTTP mode.

`--concurrency N` permits 1 to 64 concurrent operations per selected server.
The default is 1, preserving serialized operations. Higher values let the bridge
dispatch independent calls while another call waits. The upstream may still
serialize them. Connections remain local
clients of the same pool; increasing concurrency does not start more upstreams.
Discovery shares this limit. Notification listeners have their own limits.
Concurrent calls can finish out of order and still share upstream state.
Keep the default for servers that require ordered operations.

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

### Reproduce performance measurements

The Unix Python scripts in `benchmarks/` compare isolated MCP fixtures rather than
live configured servers. They measure CLI and retained-bridge workloads, verify
tool results, and record process memory separately from latency. Use their help
output to select binaries, protocols, transports, and concurrency.
Do not treat synthetic throughput as a hosted service's capacity or summed
process RSS as unique physical memory.

```sh
python3 benchmarks/run.py --products mcp-cli --mcp-cli target/release/mcp-cli --self-test
python3 benchmarks/run.py --products mcp-cli --mcp-cli target/release/mcp-cli \
  --cases bridge,hol --bridge-modes 1,8
```

Short-lived CLI commands use one Tokio worker by default to avoid starting a
worker for every host CPU on each invocation. Daemon, bridge, proxy, and native
`pool` commands retain Tokio's default worker count. An explicit
`TOKIO_WORKER_THREADS` setting overrides this choice.

### Command scope

The command surface includes `list`/`describe`/`list-tools`, `auth`, `call`,
`resource`/`resources`, `vault set|clear`,
`config list|get|add|remove|import|login|logout|doctor|help`, `import`, `daemon`, and `serve`.
mcporter is the command and config inspiration, not a second runtime or a promise
of complete parity.
Imports copy definitions; they do not enable live editor-config dependencies.
`emit-ts`, `generate-cli`, `inspect-cli`, `record`, `replay`, and an SDK are
outside this implementation's scope.
The persistent native daemon owns pools, so commands and agent clients reuse
the same daemon and multiplexer rather than an ephemeral per-command runtime or
second broker. Concurrent routing, verified retirement, and the prohibition on
replaying unknown outcomes remain part of the pool contract.

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
mcp-cli add echo -- npx -y @modelcontextprotocol/server-everything stdio

# Define a remote HTTP/SSE MCP
mcp-cli add remote --url https://example.com/mcp --transport http

# Lifecycle (drives a long-lived daemon over a control socket)
mcp-cli start echo        # pre-warm the pooled upstream (optional)
mcp-cli status            # show all pools
mcp-cli restart echo
mcp-cli stop echo
mcp-cli pool list         # configured servers; no discovery
mcp-cli remove echo

# Run the daemon explicitly (otherwise auto-launched on first command)
mcp-cli pool serve

# Bridge an agent's stdio to a pool socket. Put this in the agent's MCP config:
#   command = "mcp-cli", args = ["proxy", "echo"]
# proxy is self-starting: if the upstream is already running it just attaches
# (shared), otherwise it auto-starts the upstream (and the daemon) first. No
# separate `start` step is required — `start` is only for pre-warming.
mcp-cli proxy echo
```

Set `MCP_CLI_DEBUG=1` to enable diagnostic logging to the state dir.

`proxy NAME` and `pool proxy NAME` both resolve the merged `.mcp` configuration,
including scoped credentials. Both accept `--config` and `--root`.
No legacy TOML or mcporter configuration is read as a fallback.
A proxy exits when its pool connection closes, even while agent stdin stays open.
Closing stdin stops new requests but keeps the pool connection open for up to
800 ms so pending responses can drain.

## Local endpoint security

Unix sockets use mode `0600` and verify same-user peers. Application socket
directories use mode `0700`. Binding inside a private directory does not change
the process-wide umask; sticky-shared parents still require a temporary restrictive
umask during bind.

Windows named pipes use an owner-only DACL, reject remote clients, and verify the
server's user. Cancelled accepts retain their pending instance. A client that
disconnects before accept does not prevent later clients from connecting.
Raw proxy attachment retries unavailable or busy local endpoints. Permanent
permission, path, data, or unsupported-operation errors return immediately instead
of consuming the readiness retry budget.

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

Shutdown signals all current owned generations before waiting for individual
retirements, including replacements from already-admitted restarts. It still
waits for every retirement result. If daemon shutdown fails, failed pools remain
blocked and visible in status. The daemon resumes accepting lifecycle commands
for unrelated pools.

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

HTTP transport limits are 32 concurrent requests, 1 MiB per outgoing request,
and 16 MiB per incoming JSON response or SSE frame. Large tool catalogs can
exceed 1 MiB; size-limit errors include the response limit in bytes.
The default request deadline is 60 seconds and the default stream
read deadline is 30 seconds; configured `timeoutMs` changes these budgets.
MCP command requests carry their caller's operation budget through the local
pool to the HTTP backend, rather than inheriting a fixed 60-second ceiling.
Coalesced initialization and tool discovery use a shared transport deadline
as described below. Empty 202/204 responses are accepted for notifications.
Transport failures produce
JSON-RPC errors with the requesting client's original ID.
After stateless modern negotiation succeeds, a discovery refresh shares ordinary
bounded HTTP admission with unrelated calls. Initial negotiation and legacy
initialization remain ordering barriers.

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
Within a response, notification, or expiration batch, ready client queues receive
messages before delivery waits on full queues. Each client's message order and
the barrier before routing the next upstream message remain unchanged.

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

- `transport.rs` provides local transport over Unix domain sockets or Windows named pipes.
- `config.rs` defines server settings and runtime paths.
- `server_config.rs` and `server_config_entry.rs` normalize JSONC and own entry decoding.
- `server_config_settings.rs` interprets borrowed settings without duplicating mutable config.
- `mcp_cli.rs`, `tool_arguments.rs`, `tool_output.rs`, and `tool_discovery.rs` handle MCP commands, output, and discovery.
- `config_commands_plan.rs` shares conflict and retirement planning between copy and batch imports; `config_commands_input.rs` adapts CLI arguments.
- `daemon_commands.rs` owns auth, vault, and daemon command orchestration.
- `mcp_client_endpoint.rs` owns credential preparation and pool identity; `mcp_client.rs` and `mcp_client_wire.rs` own local MCP sessions.
- `mcp_bridge.rs` and `mcp_bridge_*.rs` implement the keep-alive tool bridge over stdio or HTTP.
- `tool_filter.rs` normalizes catalogs and filters discovery, calls, and bridge exposure.
- `tool_schema_view.rs` owns bounded schema references and option aliases; `tool_inputs.rs` owns ordered input provenance.
- `oauth.rs` and `oauth_*.rs` handle authorization, credential reuse, storage, and refresh.
- `upstream.rs` defines upstream ownership and confirmed shutdown.
- `upstream_stdio.rs` and `upstream_http.rs` implement stdio and remote transport backends.
- `jsonrpc.rs` translates per-client JSON-RPC IDs to pool-unique IDs.
- `socket_proxy.rs` owns generation lifecycle; `socket_proxy_cache.rs` owns route/cache transactions, with delivery outside its locks.
- `mcp_session.rs` / `session_cache.rs` own shared-request transitions and cache state.
- `pool.rs` and `pool_endpoint.rs` separate owned servers from discovered external endpoints.
- `control.rs` and `daemon.rs` implement the control protocol and long-lived daemon.
- `daemon_client.rs` owns control exchange and launch mechanics with distinct connection policies.
- `daemon_configuration_ownership.rs` selects pool views for native control and mutation retirement.
- `proxy.rs` bridges an agent's stdio to a pool socket and starts the upstream when needed.
- `cli_syntax.rs` derives global option arity from Clap; `cli.rs`, `cli_output.rs`, and `main.rs` own dispatch, output, and runtime lifetime.
