# Shared session contract

[README](../README.md) | [Pools](pools.md) | [Commands](commands.md) |
[Transports](transports.md)

## Shared state

A pool shares one MCP session, not just one executable. Clients have separate
JSON-RPC request IDs but share upstream state. A tool selecting a browser page,
changing a working directory, or modifying server settings can affect other
clients. Coordinate stateful select-then-act sequences; multiplexing does not make
them atomic.

## Caching and notifications

Successful `initialize` and unpaginated `tools/list` results are cached.
`notifications/tools/list_changed` invalidates discovery; upstream notifications
broadcast to connected clients. Within a response, notification, or expiration
batch, ready client queues receive messages before delivery waits on full queues.
Per-client order and the barrier before routing the next upstream message remain
unchanged.

## Shared requests and deadlines

Concurrent initialization and cacheable unpaginated `tools/list` share one upstream
request. Each MCP command retains its local deadline. A longer-lived follower can
extend the shared upstream wait, including HTTP headers and JSON/SSE body reads,
without resending the leader request. The shared wait retains the backend default
or configured budget as a minimum. A short caller's timeout or disconnect does not
end another caller's wait or retire the upstream. Tool calls are not coalesced.

Pending routes default to a five-minute cleanup budget. Longer caller budgets and
shared discovery deadlines can extend retention; cleanup is not the caller's
operation timeout. Expired shared requests release waiters. A later explicit request
can start another initialization or discovery attempt; timed-out operations are
never replayed automatically.

## Response and callback routing

Responses route only by exact upstream request ID. An uncorrelated empty-ID
response is not assigned to another client's oldest pending request. HTTP POST
failures can return an error to their known requesting client.

Sampling and roots requests go to a client advertising the required capability,
preferring the most recently active capable client. Other server requests use a
most-recently-active fallback. This heuristic does not prove callback causality or
provide per-client callback isolation.

## Unknown outcomes

A failed or timed-out tool call can have an unknown outcome. The pool does not
replay it. Check the affected system before repeating an operation that changes
state.
