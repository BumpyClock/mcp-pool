# Discover and call tools

[README](../README.md) | [Configuration](configuration.md) |
[Shared sessions](shared-sessions.md) | [Authentication](authentication.md)

Replace `docs`, `search`, and `docs://guide` in these examples with names and URIs
from your server. Discovery connects to servers and may start stdio processes.
`config list` and `pool list` inspect configured servers without discovery.

```sh
mcp-cli list
mcp-cli list docs --schema
mcp-cli describe docs
mcp-cli list-tools docs
mcp-cli list docs.search --json
mcp-cli call docs.search query="MCP" --output json
mcp-cli call --server docs --tool search --args '{"query":"MCP"}'
mcp-cli resources docs
mcp-cli resource docs "docs://guide"
```

## Short forms and help

Configured servers also support `mcp-cli docs`, `mcp-cli docs list --schema`,
`mcp-cli docs.search query="MCP"`, `mcp-cli docs search query="MCP"`, and
`mcp-cli 'docs.search(query: "MCP")'`. The first two discover; the others call.
A server-first tool token is never silently ignored.

Explicit commands and the `pool` namespace take precedence over same-named
servers; use explicit `list NAME` or `call --server NAME --tool TOOL` for those
servers. Root `--help`, `help call`, `call --help`, and nested command help
describe options without loading config.

## Discovery options and output

`list`, `describe`, and `list-tools` accept `--schema`, `--brief`/`--signatures`,
`--all-parameters`, `--json`, `--status`, `--sources`, and
`--timeout MILLISECONDS`. `--brief` cannot combine with `--schema`, `--json`,
or `--status`.

Server failures are reported but exit successfully unless `--exit-code` is set.
`--quiet` suppresses stdout and stderr and enables failure exit status. Invalid
arguments, unknown servers or tool selectors, and invalid configuration still fail.

Human output has a version and server-count header, server name, tool count or
failure, elapsed time, healthy descriptions, and a summary of healthy,
auth-required, offline, HTTP-error, and other-error counts. Auth failures suggest
a named login; HTTP 403 also points to account permissions. JSON retains the
underlying diagnostic in `issue.rawMessage`. Auth failures do not trigger
protocol fallback or replay; the legacy hint is reserved for failed discovery
probes with no JSON-RPC error response.

`list SERVER` displays descriptions, parameter documentation, typed signatures,
examples, and transport summary. The default shows up to five parameters plus
additional required ones and names hidden optional fields. `--all-parameters`
shows them, `--brief` shows required-only signatures, and `--schema` includes
complete input schemas. Examples preserve enum types through local references
including array items; cyclic or unresolved references use generic placeholders,
not validated values. `list SERVER.TOOL` selects one tool; a second positional
argument is not accepted.

`--no-color`, `NO_COLOR`, redirected output, and `TERM=dumb` disable color.
Windows interactive colors require virtual-terminal support; the command enables
it without clearing other console flags, reports failures and falls back to plain
text. JSON and quiet listings do not change console modes.

## Call selectors and arguments

`call` accepts `SERVER.TOOL`, `SERVER TOOL`, or `--server NAME --tool NAME`.
Dotted selectors split at the first dot: `docs.search.v2` selects `docs` and
`search.v2`; tool-specific discovery works likewise. Inferred commands instead
select the longest configured server prefix. Use explicit server/tool flags for
dotted server names.

| Form | Behavior |
| --- | --- |
| `key=value`, `key:value` | Named argument. |
| `--args`/`--params` | JSON arguments; `--args -` reads a JSON object on stdin. |
| Positional arguments | Follow schema property declaration order. |
| `key=@FILE` | Read a regular UTF-8 file as literal text, up to 16 MiB. |
| `key=@@text` | Send literal `@text`. |

Tool-option flags must match schema properties or kebab-case aliases; exact names
win, ambiguous aliases and unknown options fail before dispatch. Local schema
references use inline-equivalent argument types. `--raw-strings`/`--no-coerce`
preserves strings rather than coercing JSON numbers, booleans, or collections.

## Tool name resolution

Catalogs omit missing or empty names and retain the first entry per exact name.
Resolution tries exact names, then a unique case-insensitive alias ignoring
punctuation; `list-pages` can select `list_pages`. Ambiguous aliases fail; typo
suggestions never execute. Tool-specific discovery uses the same resolver.

`call SERVER.list_tools` and `call SERVER.help` fall back to discovery only when
no real corresponding tool exists, including excluded tools. Fallbacks reject tool
arguments and render the already-discovered catalog without reconnecting or
repagination; their duration and deadlines cover initial call-side discovery.

## Result formats and files

Calls and resources accept `--output auto|text|markdown|json|raw` and
`--timeout MILLISECONDS`. Call `--json` is an argument object, not output
selection; use `--output json`. Raw output is a JSON MCP result envelope, not
Node inspect text; other formats can extract structured content or text. An MCP
error result exits unsuccessfully. Byte-for-byte output parity with mcporter is
not promised.

`call --save-images DIR` writes returned base64 images to collision-free files
and reports paths on stderr. `--tail-log` prints the last 20 lines of an absolute
regular-file `logPath`, `logFile`, or `logfile`, reading at most 1 MiB. It does
not follow the log. Tail adds non-JSON text on stdout; omit it when parsing JSON.

## Timeout precedence and phase budgets

The first applicable value sets the timeout:

1. `--timeout`.
2. A positive `MCPORTER_CALL_TIMEOUT` or `MCPORTER_LIST_TIMEOUT`.
3. Configured `timeoutMs`.
4. The default: 60 seconds for calls, 30 seconds for discovery.

A list deadline covers each server's credential preparation, connection,
initialization, and all discovery pages. Calls and resources apply the duration
separately to connection/initialization, paginated discovery, and final request;
credential preparation precedes connection deadline. It is not one whole-command
deadline. Timeouts do not change pool identity. A timed-out or interrupted request
closes its local connection, not the upstream, and is not replayed.

## Progress and global flags

Interactive discovery, calls, resources, and native pool controls show a phase and
elapsed-time indicator on stderr; discovery also shows completed server count. It
clears before results or errors. JSON, raw, plain, quiet, redirected output, CI,
and `TERM=dumb` have no progress output. `MCP_CLI_NO_PROGRESS=1` or
`MCPORTER_NO_SPINNER=1` disables it. Proxy and bridge streams are never decorated.
MCP `--plain` suppresses progress without changing result format; native status
and `pool list` use it for stable rows.

Leading global flags include `--json`, `--debug`, `--config PATH`, `--root DIR`,
`--log-level LEVEL`, and `--oauth-timeout MILLISECONDS`. Leading `--json` selects
JSON output, unlike call `--json`. `trace` and `debug` enable diagnostics; `info`,
`warn`, `error`, and `silent` disable verbose diagnostics. See
[authentication](authentication.md#oauth-requirements) for OAuth timeout restrictions.

## Ad-hoc targets

Discovery, calls, and auth can select `--http-url URL` or `--stdio COMMAND`, with
`--stdio-arg ARG`, `--cwd DIR`, `--env KEY=value`, `--header KEY=value`,
`--name NAME`, and `--description TEXT` as applicable. `--stdio` tokenizes its
command before repeated arguments are appended. Ad-hoc HTTP requires HTTPS unless
`--allow-http` is explicit; URL userinfo credentials are rejected.

`--persist PATH` requires an ad-hoc target and writes through the existing config
writer without overwriting an existing name. Calls persist before execution; auth
persists after successful authorization.

## Command scope

The supported surface also includes `auth`, `vault set|clear`, `config`, `import`,
`daemon`, and `serve`. mcporter inspires the command/config interface but is not
a runtime dependency or a promise of full parity. `emit-ts`, `generate-cli`,
`inspect-cli`, `record`, `replay`, and an SDK are out of scope.
