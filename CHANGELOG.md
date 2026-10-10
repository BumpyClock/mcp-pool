# Changelog

## Unreleased

### Added

- Authenticated discovery, tool calls, resources, config and credential
  commands, and a tool bridge over the native shared MCP pool runtime.
  Existing mcporter JSONC configs are read directly. Consent starts only
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

### Fixed

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

The daemon, upstream state, and credential stores remain shared; this is not
per-client or OAuth isolation. Request deadlines and the prohibition on replaying
unknown outcomes remain unchanged. `MCP_POOL_HOME` does not isolate credentials.
This is not complete mcporter parity: editor/config imports and conversion are
deferred; code generation, recording/replay, and an SDK remain out of scope.
The bridge exposes tools, not resources, prompts, or elicitation.
