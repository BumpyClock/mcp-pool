# Serve a tool bridge

[README](../README.md) | [Configuration](configuration.md) | [Pools](pools.md) |
[Shared sessions](shared-sessions.md)

Top-level `serve` exposes selected keep-alive servers over stdio or HTTP. It is
different from `pool serve`, the daemon control socket.

```sh
mcp-cli serve --stdio --servers docs
mcp-cli serve --http 3333 --servers docs
mcp-cli serve --http 3333 --servers docs --concurrency 8
```

Replace `docs` with a configured server. Stdio is the default; `--stdio` and
`--http` are mutually exclusive, and `--host` requires HTTP. HTTP defaults to
`127.0.0.1`. The HTTP bridge has no downstream authentication. Binding a
non-loopback host exposes tools without authentication; keep it on loopback unless
access is controlled separately.

## Keep-alive eligibility

`--servers` selects comma-separated names, not keep-alive eligibility. Selected
entries must qualify. Without the flag all qualifying entries are selected; an
empty selection is an error. Eligibility is evaluated in this order:

1. `MCPORTER_KEEPALIVE` enables matching entries, even if a disable override also
   matches.
2. `MCPORTER_DISABLE_KEEPALIVE` disables matching entries. `MCPORTER_NO_KEEPALIVE`
   is used only if `MCPORTER_DISABLE_KEEPALIVE` is unset, even if it is set to an
   empty value.
3. An explicitly configured lifecycle decides eligibility, for example
   `"lifecycle": "keep-alive"`.
4. Otherwise, a command or argument containing the literal `\${CHROME_DEVTOOLS_URL}`
   or `$env:CHROME_DEVTOOLS_URL` disables the default. Remaining entries qualify by
   default if their name or recognized command identifies `chrome-devtools`,
   `mobile-mcp`, `playwright`, or `cloudbase`.

Override values are comma-separated names or `*`, matched case-insensitively
against the configured name or recognized canonical name. Recognition examines
command and argument fragments:

| Canonical name | Fragments |
| --- | --- |
| `chrome-devtools` | `chrome-devtools-mcp` |
| `mobile-mcp` | `@mobilenext/mobile-mcp`, `mobile-mcp` |
| `playwright` | `@playwright/mcp`, `playwright/mcp` |
| `cloudbase` | `@cloudbase/cloudbase-mcp`, `cloudbase-mcp` |

Review eligible entries before exposing a bridge.

## Concurrency

`--concurrency N` allows 1 to 64 operation connections per server, default 1.
Higher values allow independent calls while another waits, though the upstream may
serialize them. Discovery shares the limit; notification listeners have separate
limits. Connections use the same pooled upstream, not additional upstreams or
isolated state. Calls can finish out of order; retain the default for ordered
operations.

## HTTP protocol

The example combined endpoint is `http://127.0.0.1:3333/mcp`; `/mcp/docs` exposes
only that server with bare tool names such as `search`. Combined names include
the server, such as `docs__search`. Reserved separators are escaped; use names
returned by `tools/list` instead of constructing unusual names.

The bridge accepts Streamable HTTP POSTs with JSON responses, legacy `initialize`,
and modern `server/discover` using `2026-07-28`. Modern HTTP requires
`MCP-Protocol-Version`, matching `MCP-Method`, and `MCP-Name` when `params.name`
is present. The same version must appear in
`params._meta["io.modelcontextprotocol/protocolVersion"]`. Ordinary requests
require `Accept: application/json, text/event-stream` and return JSON.
Notifications receive an empty HTTP 202.

## Subscription streams

Modern POST `subscriptions/listen` accepts a `notifications` filter containing
`"toolsListChanged": true` and requires `Accept: text/event-stream`. It streams an
acknowledgement, subscription-ID-tagged `notifications/tools/list_changed`
messages, and a completion on graceful shutdown. Each subscribed server has a
dedicated local notification connection to the same upstream. Listeners can
reconnect without replaying requests or missed events; full queues can coalesce
invalidations.

## Protocol scope

This sessionless tool bridge is not full MCP or mcporter `serve` parity. It creates
no session IDs and rejects supplied IDs. GET and DELETE are unsupported. SSE is
limited to tool-list changes, not arbitrary notifications or ordinary tool-call
related messages. Stdio supports modern discovery but not subscription streams.

The bridge advertises tools, not resources, prompts, or elicitation; it acknowledges
downstream notifications without forwarding and does not relay upstream sampling
or roots requests. Use `resource` or `proxy` for the corresponding upstream surface.

## Limits and deadlines

| Limit | Value |
| --- | --- |
| HTTP bodies and stdio frames | 1 MiB |
| HTTP in-flight requests, including streams | 64; excess receives 503 |
| Dedicated notification connections across subscriptions | 64 |
| SSE message | 1 MiB |
| Per-server operation timeout | Configured `timeoutMs`, default 60 seconds; no 120-second cap |

The per-server timeout includes queue wait and reconnect setup. Established
subscriptions have no idle operation deadline.
