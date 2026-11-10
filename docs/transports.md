# Upstream transports and negotiation

[README](../README.md) | [Configuration](configuration.md) |
[Authentication](authentication.md) | [Shared sessions](shared-sessions.md)

## Protocol negotiation

Server entries accept `protocolVersion` (`protocol_version` is an alias):

| Value | Behavior |
| --- | --- |
| `auto` (default) | Probe `server/discover` for `2026-07-28`; fall back to legacy initialization only after a definitive unsupported-method/version response or an initial HTTP protocol mismatch. |
| `legacy` | Skip the modern probe and send `initialize`. Supports `2025-11-25`, `2025-06-18`, `2025-03-26`, and `2024-11-05`. |
| `2026-07-28` | Require modern discovery without legacy fallback. |

Modern requests carry protocol-version metadata and HTTP method/name headers,
without legacy `initialize` or `notifications/initialized`. Legacy SSE uses
legacy negotiation. Incompatible configured preferences have distinct pool
identities. JSON discovery reports negotiated `protocolVersion`, `era`, and
server instructions.

Negotiation uses the same upstream, not a temporary probe. Authentication failures,
malformed replies, disconnects, and unanswered probes do not trigger restart or
replay. Set `protocolVersion` to `legacy` for older stdio servers that exit or
ignore unknown methods. An operation with unknown outcome is never automatically
retried.

For example, 1Password desktop `1password-mcp` can exit with
`ExpectedInitializeRequest` when probed with `server/discover`, surfaced as EOF.
Configure:

```json
{"mcpServers":{"1password":{"command":"1password-mcp","protocolVersion":"legacy"}}}
```

Then `mcp-cli list 1password` verifies discovery. This does not authorize 1Password
Environments.

## HTTP and SSE

`--transport http` uses Streamable HTTP POST. Initialization captures session ID
and negotiated protocol version for later requests. JSON and incremental SSE
responses are supported.

`--transport sse` uses a legacy GET event stream, discovers its same-origin
message endpoint, and sends POSTs there; it is not a POST-only approximation.

### HTTP limits and deadlines

| Limit or deadline | Value |
| --- | --- |
| Concurrent requests | 32 |
| Outgoing request | 1 MiB |
| Incoming JSON response or SSE frame | 16 MiB |
| Default request deadline | 60 seconds |
| Default stream read deadline | 30 seconds |

Large catalogs can exceed 1 MiB; size errors include the response limit in bytes.
Configured `timeoutMs` changes deadline budgets. MCP commands carry caller operation
budgets through the pool to HTTP rather than inheriting a fixed 60-second ceiling.
Shared initialization and discovery use the deadline rules in
[shared sessions](shared-sessions.md).

Empty 202/204 responses are accepted for notifications. Transport failures return
JSON-RPC errors with the requesting client's original ID. After stateless modern
negotiation, discovery refresh shares bounded HTTP admission with unrelated calls;
initial negotiation and legacy initialization remain ordering barriers.

### Session shutdown and recovery

Deliberate shutdown sends a session DELETE when a session ID exists. DELETE has a
two-second deadline; unsupported DELETE or timeout is logged without blocking
confirmed local retirement. Remote cleanup is not guaranteed.

An expired session returns an error without replay; restart the pool and reconnect
before new requests. Optional Streamable HTTP GET and resumable event replay are
not implemented. Configured servers can use headers and scoped OAuth credentials;
consent requires explicit `auth`.
