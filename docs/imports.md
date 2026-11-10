# Import existing definitions

[README](../README.md) | [Configuration](configuration.md) |
[Authentication](authentication.md) | [Upgrade](installation.md#upgrade-from-mcp-pool)

Imports copy definitions explicitly; normal commands do not read or follow source
configs. `import` and `config import` are equivalent.

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

Replace `PATH`, `OLD_CONFIG_TOML`, and `docs` with your source path and server name.
The default destination is home; `--scope project` selects
`PROJECT/.mcp/config.json`, and `--config PATH` selects an explicit destination.
`--from PATH` selects a source file; optional server names select a subset.

Unchanged imports preserve bytes and modification time, including JSONC comments,
BOM, and whitespace. Different definitions require `--replace`; conflicts leave
the whole destination unchanged. `--dry-run` makes no files or connections. Output
reports names and paths, not credential values.

## Source locations

| Source | Files read during explicit import |
| --- | --- |
| `claude` / `claude-code` | `~/.claude.json`, Claude project-local entries there, `PROJECT/.mcp.json`, and legacy `.claude/mcp.json`, `settings.json`, and `settings.local.json`. Local overrides project, then user. |
| `claude-desktop` | Platform `Claude/claude_desktop_config.json`. |
| `codex` | `~/.codex/config.toml` and `PROJECT/.codex/config.toml`; project wins. `CODEX_HOME` overrides the user directory. |
| `mcporter` | `~/.mcporter/mcporter.json` and `PROJECT/config/mcporter.json`; project wins. |
| `cursor` | `PROJECT/.cursor/mcp.json`, `~/.cursor/mcp.json`, and platform `Cursor/User/mcp.json`; project wins. |
| `windsurf` | `.codeium/windsurf`, `.codeium/windsurf-next`, `.windsurf`, and platform Codeium locations containing `mcp_config.json`. Use `--from` for another location. |
| `vscode` / `vs-code` | `PROJECT/.vscode/mcp.json` and platform `Code/User/mcp.json` or `Code - Insiders/User/mcp.json`; reads the `servers` container. |
| `opencode` | Project `opencode.jsonc` / `opencode.json`, platform OpenCode config, and supported legacy OpenAI locations. Honors `OPENCODE_CONFIG`, `OPENCODE_CONFIG_DIR`, and `OPENAI_WORKDIR`. |
| `project` | Claude project files, `.codex/config.toml`, and `config/mcporter.json`. Cross-tool name conflicts fail. |
| `mcp-pool` | Former native `mcp-pool/config.toml`, or `--from PATH`. |

## Conversion rules

Sources are not modified or executed. Relative working directories and executable
paths are rebased; environment references, headers, OAuth settings, and tool
filters remain, but ambient environment values are not saved. Codex disabled
servers are omitted with a warning. Unsupported auth helpers and transports fail
rather than dropping authentication. Legacy `imports` arrays are not followed.

Literal `bearerToken` credentials and masked credential placeholders are rejected
atomically; use `bearerTokenEnv` or an unresolved environment reference. OAuth
tokens are not imported by default.

## Import mcporter credentials

`import mcporter --include-auth` also copies cached HTTP/SSE OAuth tokens and their
bound client registration. It reads `~/.mcporter/credentials.json` or
`$XDG_DATA_HOME/mcporter/credentials.json` when `XDG_DATA_HOME` is absolute.
Configured environment overrides take precedence. Source home follows effective
`HOME`, then `USERPROFILE`, independently of destination scope. Explicit
`tokenCacheDir` stores and legacy `~/.mcporter/NAME` caches are supported.
`--from` selects definitions, not a credential file. Other sources and stdio helper
credentials are unsupported.

Credentials go beside the destination config in `credentials.json`. Copied remote
definitions drop `tokenCacheDir` to prevent future writes to mcporter caches.
Sources stay untouched; import makes no network request and starts no consent flow.
Expiration times stay unchanged; expired tokens need refresh or explicit
authorization on first use.

### Conflicts and commits

Text and JSON reports give per-server `import`, `unchanged`, `missing`, or
`unsupported` auth status without credential values. Missing auth does not stop
definition imports. Different destination credentials, client registrations, or
unfinished authorization state require `--replace`; identical copies are
idempotent. Destination paths aliasing source credential files, including symlinks,
are rejected.

`--dry-run` reads and validates without writing or retiring pools. Credential
writes obey read-only policy and first retire affected pools. Preflight and conflict
errors leave destination files unchanged.

Config and credentials commit separately. A later credential I/O failure can leave
config and earlier credentials imported; fix the store and rerun. An unchanged
config still permits credential imports, including required checks, retirement, and
writes.

**Cutover warning.** Copied refresh tokens may rotate. Stop using imported servers
through mcporter after cutover, or one runtime can invalidate the other's
credentials. `--replace` can overwrite newer destination tokens with an older source
copy. If the source is stale, authorize through mcp-cli instead.
