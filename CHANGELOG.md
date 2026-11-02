# Changelog

## Unreleased

### Added

- Opt-in bridge `serve --concurrency N` allows up to 64 concurrent operations
  per server over local connections to the same pooled upstream. The default
  remains serialized. Operation deadlines include waiting for a connection.
- Reproducible, isolated performance fixtures for CLI and retained bridge
  workloads, including protocol, concurrency, and resident-memory comparisons.
- Opt-in offline pre-push checks for formatting, binary checking, and selected
  Unix IPC tests. The repository pins Rust 1.95.0 with rustfmt and Clippy.
  Hook installation is explicit and repository-local.
- Opt-in `import mcporter --include-auth` copies cached HTTP/SSE OAuth tokens,
  client registrations, and discovery metadata into the destination credential
  scope. Supports vault and directory caches, redacted previews, conflict checks,
  private writes, and pool retirement without login, refresh, or source changes.
  Copied remote definitions no longer share mcporter token-cache directories.
- Discoverable root help and configured-server CLI shorthand for discovery,
  dotted calls, server-first calls, and call expressions. Explicit native
  commands keep precedence; extra tool tokens are never silently ignored.
- Safe tool-name aliases and non-executing typo suggestions, shared with
  tool-specific discovery. Missing `help` and `list_tools` tools can fall back
  to discovery without bypassing excluded real tools.
- Upstream `2025-11-25` initialization and modern `2026-07-28` negotiation,
  configurable through `protocolVersion`. Modern HTTP method/name headers,
  shared discovery, and protocol metadata use the existing pooled upstream.
- Source-aware version output and opt-in Windows/Unix installers that verify
  the staged host executable and report command-path mismatches.
- Explicit Cursor, Windsurf, OpenCode, and VS Code imports, plus
  `config add --copy-from SOURCE:NAME` with source selection, normalized
  environment/path references, overrides, dry runs, and atomic replacement.
- Authenticated discovery, tool calls, resources, config and credential
  commands, and a tool bridge over the native shared MCP pool runtime.
  Consent starts only
  through explicit `auth` or `config login`; ordinary operations can reuse
  credentials or refresh tokens.
- Current-user local IPC checks, private Unix sockets and application
  directories, and owner-only Windows named pipes that reject remote clients.
- Readable `list SERVER` references with wrapped descriptions, parameter docs,
  typed signatures, enum-aware JSON argument examples, and transport summaries.
  Required parameters stay visible; `--all-parameters` expands hidden optional
  fields, and `--brief` stays compact. `--no-color`, `NO_COLOR`, and redirection
  suppress colors.
- Interactive stderr progress for discovery, calls, resources, and native
  control, cleared before results or errors. Machine modes, redirected streams,
  CI, and `TERM=dumb` suppress progress; `MCP_POOL_NO_PROGRESS=1` and
  `MCPORTER_NO_SPINNER=1` disable it explicitly.
- Explicit `import` / `config import` for Claude Code, Claude Desktop, Codex,
  project configs, mcporter JSONC, and former native TOML configs. Imports
  preserve environment references, rebase paths, support previews and selected
  server names, and require `--replace` for conflicting definitions.

### Changed

- Short-lived CLI commands default to one Tokio worker rather than one per
  host CPU. Long-running commands and explicit `TOKIO_WORKER_THREADS` settings
  retain their previous behavior.
- Stdio output encodes each JSON message once. CLI, client, and HTTP bridge
  handoffs move owned JSON values instead of cloning consumed payloads.
- Tool-name deduplication borrows names and preserves upstream order.
- HTTP endpoint and OAuth credential-setting references now share configured-env
  precedence, keeping imported credential identities consistent with later runtime use.
- Default daemon state and logs now use `~/.mcp/` on every platform, alongside
  the existing home config and credentials. Unix server sockets use `.mcp/run/`,
  avoiding the default macOS Application Support socket-length failure without
  reducing identity hashes. Windows keeps its home-scoped named pipes.
  `MCP_POOL_HOME` overrides are unchanged. Stop the old daemon with the old
  executable before upgrading; former runtime state is not migrated.
- Internal config and command operations no longer rebuild JSON text or CLI
  arguments. Shared-request transitions and daemon ownership selection have
  explicit owners. The bridge uses the pooled MCP client directly.
- All commands now use `~/.mcp/config.json` merged with
  `PROJECT/.mcp/config.json`, with project entries taking precedence.
  `MCP_POOL_CONFIG` replaces `MCPORTER_CONFIG`; explicit `--config` selects one
  file. External configs are read only during explicit imports.
- Credentials use the same home and project scopes. Project authentication
  overrides home authentication even for inherited server definitions.
  Expired project credentials never fall back to another account. Legacy OAuth
  tokens are not imported automatically; authorize the new scope explicitly.
- Native lifecycle and proxy commands use source- and credential-scoped pool
  identities. Each caller resolves its configuration; the daemon no longer
  auto-starts configs from its own launch directory. Restart an older daemon
  after upgrading.

### Fixed

- Daemon startup attempts to connect immediately before applying bounded retry
  delays, removing the unconditional initial 100 ms wait.
- Unix IPC test fixtures use short private temporary paths, avoiding macOS
  socket-length failures caused by long checkout paths.
- Identical explicit imports and `config add --copy-from` preserve existing
  config bytes and modification times, including JSONC comments, BOMs, and
  whitespace. Validation and required credential retirement/import still run;
  config and credential commits remain separate.
- Tool arguments preserve exact schema property names when another property's
  kebab-case alias has the same spelling.
- Single-server discovery now matches mcporter's default exit status. Failures
  remain visible in the report; `--exit-code` and `--quiet` return a failure
  status for health checks. Unknown tool selectors still fail, and quiet
  health checks suppress both stdout and stderr.
- macOS release builds and retirement of naturally exited upstream processes.
  Socket-name length budgets now account for canonical paths such as `/private/tmp`.
- Inferred server-first calls preserve JSON payloads and selector aliases
  before the tool name. Routing and explicit calls share one typed option model.
- Atomic config writes reject malformed legacy `imports` fields before
  replacement, using the same shape validation as config reads.
- Windows installation can replace an existing executable safely. Staged files
  retain an `.exe` suffix, and replacement passes a true null backup path to
  .NET instead of an empty string.
- Windows command resolution prefers `PATHEXT` launchers over sibling
  extensionless Unix scripts, so commands such as `npx` work without changing
  configs to `npx.cmd`.
- Private-directory Unix binding no longer changes the process-wide umask.
  Sticky-shared parents still use a temporary restrictive umask during bind.
  Cancelled Windows accepts retain their pending instance; abandoned clients
  do not prevent later connections.
- Configured `env` values take precedence when resolving HTTP headers,
  `bearerToken`, and `bearerTokenEnv`. Exact-name tool filters apply consistently
  to discovery, calls, and bridge exposure; raw proxy access bypasses them.
- Ad-hoc `--persist PATH` rejects existing names without replacing saved
  definitions. Human transport footers distinguish SSE from HTTP.

### Scope

The daemon and upstream state remain shared within a configuration and credential
scope; this is not per-client state isolation. Request deadlines and the prohibition
on replaying unknown outcomes remain unchanged. `MCP_POOL_HOME` redirects home
credentials but not project credentials or explicit `tokenCacheDir` stores.
This is not complete mcporter parity; code generation, recording/replay, and an SDK
remain out of scope.
The bridge exposes tools, not resources, prompts, or elicitation.
