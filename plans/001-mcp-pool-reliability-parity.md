# Plan 001: Bring mcp-pool reliability/performance closer to mcpproxy-go while preserving per-MCP visibility

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 14bb70f..HEAD -- src/socket_proxy.rs src/upstream.rs src/pool.rs src/daemon.rs src/cli.rs src/types.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.
>
> **Dirty-baseline check (also run first)**:
> `git status --short`
> `git diff -- src/socket_proxy.rs`
> `cargo check --message-format=short`
> Expected at plan time: `src/socket_proxy.rs` is modified and `cargo check`
> fails with `E0382 borrow of partially moved value: value` at the response
> fallback in `route_response`. Step 1 must preserve the existing dirty behavior
> except for restoring compilation.

## Status

- **Priority**: P0
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: none
- **Category**: bug, perf, tech-debt
- **Planned at**: commit `14bb70f`, 2026-06-20

## Why this matters

`mcp-pool` is meant to let many agent sessions share one upstream MCP process. The current implementation is close, but still behaves more like a byte multiplexer than a session-aware proxy. Live logs from `~/Projects/PersonalAssistant` showed repeated upstream `initialize`/`tools/list`, Agency token acquisition/429 churn, and `Session not found` responses from upstream bridges with `id:""` that mcp-pool drops as orphaned responses, causing Copilot tool calls to appear stalled.

The reference implementation, `~/Projects/references/mcpproxy-go`, is more stable because it separates upstream lifecycle/discovery from downstream client sessions. This plan keeps mcp-pool's required per-MCP-visible model (`mcp-pool proxy <name>` per configured server) while adopting the same reliability patterns: local downstream lifecycle, cached/last-good discovery, coalesced `tools/list`, explicit session recovery, capability-aware server callback routing, and structured observability.

## Current state

Relevant files:

- `src/socket_proxy.rs` — per-server local socket proxy, request id rewriting, response routing, current handshake cache attempt. **Currently dirty and not compiling**.
- `src/upstream.rs` — stdio/HTTP/SSE upstream process/client code.
- `src/pool.rs` — registry of `SocketProxy` instances and start/stop/restart.
- `src/daemon.rs` — daemon boot, control socket, warm-all startup.
- `src/cli.rs` — CLI dispatch and daemon auto-spawn.
- `src/types.rs` — status response types.

Current critical excerpt and failure:

```text
src/socket_proxy.rs:780-826 (dirty working tree at plan time)
route_response(...) parses upstream responses. In the response branch it matches
`value` by value, moving the object, then later calls
`restore_empty_id_error_to_oldest_pending(&value, request_map)`.

Current command:
cargo check --message-format=short

Current error:
src\socket_proxy.rs:826:74: error[E0382]: borrow of partially moved value: `value`
```

Existing cache attempt:

```text
src/socket_proxy.rs:34-45
HandshakeCache stores successful `initialize` and `tools/list` result payloads.

src/socket_proxy.rs:540-563
handle_client answers cache hits directly with a re-id'd JSON-RPC success response.

src/socket_proxy.rs:600-615
dirty change attempts to swallow `notifications/initialized` after cached initialize.
```

Existing routing behavior:

```text
src/socket_proxy.rs:780-843
route_response restores responses only when upstream id matches an entry in request_map.
If upstream returns id:"" for an error, it currently becomes an orphan and is dropped.
```

Live evidence from `~/Projects/PersonalAssistant`:

```text
mcp-pool log:
pool_request_forwarded client_id=Microsoft-Calendar-client-2 bytes=404
pool_response_orphaned id="" reason=no_pending_request
upstream_stderr Proxy error: {"jsonrpc":"2.0","error":{"code":-32001,"message":"Session not found"},"id":""}

Agency Calendar log:
caller message: {"id":5,"jsonrpc":"2.0","method":"tools/call",...,"name":"ListCalendarView"}
Forwarding request to upstream MCP server: ... "id":5
Upstream server returned status code: 404
Upstream response text: {"error":{"code":-32001,"message":"Session not found"},"id":"","jsonrpc":"2.0"}
```

Reference patterns from `mcpproxy-go`:

- `internal/server/mcp.go:190-238` — downstream client `initialize` is handled locally; client capabilities are recorded in session state.
- `internal/runtime/lifecycle.go:312-422` — tool discovery updates an index and keeps last-good snapshots instead of shrinking/discarding tools on transient discovery failure.
- `internal/upstream/managed/client.go:497-637` — `ListTools` is coalesced: one leader performs upstream `tools/list`; concurrent followers wait for the same result.
- `internal/upstream/managed/client.go:964-975` — health check uses lightweight `ping`, not `tools/list`.
- `internal/runtime/lifecycle.go:91-105` and `453-492` — discovery/reconnect work is deduplicated and retried with backoff.

Repo conventions:

- Rust edition 2024.
- No `mod.rs` / `lib.rs`; modules are declared from `src/main.rs`.
- No panicking APIs: no `unwrap()`, `expect()`, or panicking indexing in production code.
- Prefer explicit `match`/`if let`, use `?` for errors, and log ignored fallible operations consistently.
- Build/test gates: `cargo build`, `cargo clippy --all-targets`, `cargo test`.
- Existing tests live in module-local `#[cfg(test)]` blocks; `socket_proxy.rs` already has routing tests to extend.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Build | `cargo build --message-format=short` | exit 0, no errors |
| Lint | `cargo clippy --all-targets --message-format=short` | exit 0, no warnings/errors |
| Tests | `cargo test --message-format=short` | exit 0, all tests pass |
| Windows install | `cargo build --release --message-format=short`; copy `target\release\mcp-pool.exe` to `%USERPROFILE%\.local\bin\mcp-pool.exe` | installed binary reports `mcp-pool 0.1.0` |
| Real smoke | In tmux under `~/Projects/PersonalAssistant`: `mcp-pool serve --debug`, then a fresh `copilot` session | second Copilot session shows cache hits for `initialize`/`tools/list` and no repeated upstream discovery storm |

## Scope

**In scope**:

- `src/socket_proxy.rs`
- `src/mcp_session.rs` (create if using the recommended split for pure protocol/session helpers)
- `src/main.rs` (only if `src/mcp_session.rs` is created; add `mod mcp_session;`)
- `src/upstream.rs`
- `src/pool.rs`
- `src/daemon.rs`
- `src/cli.rs`
- `src/types.rs`
- tests in those same files

**Out of scope**:

- Changing the public configuration model: do **not** collapse all MCPs behind one facade. Each MCP must remain separately visible/proxied (`mcp-pool proxy <name>`).
- Editing `~/Projects/PersonalAssistant` MCP config except for manual testing.
- Modifying Agency or mcpproxy-go.
- Adding new dependencies unless absolutely necessary. Use std/Tokio/serde_json already present.
- Persisting cached tool data to disk. This plan is in-memory per daemon.

## Git workflow

- Work on the current branch unless the operator tells you otherwise.
- Keep commits logical. Suggested messages:
  - `fix: route malformed upstream errors instead of hanging clients`
  - `refactor: split downstream MCP lifecycle from upstream discovery`
  - `perf: coalesce tool discovery and cache last-good tools`
- Do not push or open a PR unless instructed.

## Parallel execution model

Do **not** let multiple agents freely edit `src/socket_proxy.rs`. It is the hot file and all reliability behavior converges there. Use this order:

1. **Serial preflight/integrator** (one agent): execute Step 1 and Step 2's type/contract extraction. This restores compilation and creates stable helper types/functions.
2. **Parallel work after contracts exist**:
   - **Agent A — response routing**: owns `route_response`, empty-id error fallback, response-routing tests. May edit `src/socket_proxy.rs` only in the route_response/helper/test sections.
   - **Agent B — downstream lifecycle + capabilities**: owns `handle_client` request classification, cached initialize lifecycle, `ClientCapabilities` parsing, and related tests. May edit `src/socket_proxy.rs` only in handle_client/helper/test sections and `src/mcp_session.rs` if created.
   - **Agent C — tools/list cache/coalescing**: owns cache state structs/helpers and tests. Prefer `src/mcp_session.rs` for pure cache state; wire into `src/socket_proxy.rs` only through the integrator if conflicts arise.
   - **Agent D — recovery + observability**: owns recovery signal loop/logging in `SocketProxy` and `Pool`/daemon wiring if needed. Must not alter handle_client or route_response semantics except through helper calls agreed in Step 2.
3. **Integration owner** (one agent): merges all subagent changes, resolves `socket_proxy.rs`, runs all gates and real tmux verification.

If a subagent needs to touch a section outside its ownership, it must stop and report. If this coordination is unavailable, execute Steps 1–7 serially.

## Steps

### Step 1: Restore a compiling baseline

Fix the current `src/socket_proxy.rs` borrow/move error before any design work. In `route_response`, avoid moving `value` before fallback logic. Acceptable shapes:

- borrow the object and clone only when building the restored response, or
- compute fallback before consuming `value`, or
- clone `value` into a local response template before moving.

Before editing, run:

```powershell
git status --short
git diff -- src/socket_proxy.rs
cargo check --message-format=short
```

Expected: dirty `src/socket_proxy.rs` and `E0382` at the `restore_empty_id_error_to_oldest_pending(&value, request_map)` call. Do not overwrite unrelated dirty changes. Do not change behavior in this step except to compile.

**Verify**: `cargo build --message-format=short` → exit 0.

### Step 2: Extract protocol/session contracts used by later work

Create stable data shapes before parallel implementation. Prefer a new pure-helper module:

- `src/mcp_session.rs` — JSON-RPC/MCP session helper types and pure functions.
- `src/main.rs` — add `mod mcp_session;` if the module is created.

Recommended exact types:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheableMethod {
    Initialize,
    ToolsList,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientCapabilities {
    pub sampling: bool,
    pub roots: bool,
}

#[derive(Debug, Clone)]
pub struct PendingRequestInfo {
    pub client_id: String,
    pub original_id: serde_json::Value,
    pub method: Option<String>,
    pub inserted_at: std::time::Instant,
}

#[derive(Debug, Clone)]
pub struct PendingWaiter {
    pub client_id: String,
    pub original_id: serde_json::Value,
    pub inserted_at: std::time::Instant,
}

#[derive(Debug, Default)]
pub struct ToolsListCache {
    pub cached_result: Option<serde_json::Value>,
    pub last_good_result: Option<serde_json::Value>,
    pub waiters: Vec<PendingWaiter>,
    pub in_flight: bool,
}

#[derive(Debug, Default)]
pub struct HandshakeCache {
    pub initialize: Option<serde_json::Value>,
    pub tools_list: ToolsListCache,
}
```

Recommended pure helper functions:

- `cacheable_method(method: &str) -> Option<CacheableMethod>`
- `build_success_response(original_id: Value, result: Value) -> String`
- `build_error_response(original_id: Value, code: i64, message: &str) -> String`
- `parse_client_capabilities(initialize_request: &Value) -> ClientCapabilities`
- `is_session_not_found_error(value: &Value) -> bool`
- `is_empty_id_error(value: &Value) -> bool`

If you keep these types in `src/socket_proxy.rs` instead, use the exact same shapes/names so later steps can be divided safely.

Tests:

- `cacheable_method` recognizes only `initialize` and `tools/list`.
- `parse_client_capabilities` finds `sampling` and `roots` under `params.capabilities`.
- `is_session_not_found_error` recognizes error code `-32001` and message containing `Session not found`.

**Verify**: `cargo test --message-format=short` → all tests pass.

### Step 3: Route malformed empty-id upstream errors to a pending client

Implement narrow fallback behavior for upstream error responses like:

```json
{"jsonrpc":"2.0","id":"","error":{"code":-32001,"message":"Session not found"}}
```

Rules:

- Only fallback-route when:
  - response has no `method`
  - response has `error`
  - id is exactly empty string
  - there is at least one pending request
- Pick the oldest pending request for this `SocketProxy` (by `Instant`).
- Remove that pending entry.
- Restore the original downstream id using `jsonrpc::with_id`.
- Send to that pending client.
- Log a concise line: `pool_response_empty_id_error_routed client_id=<id> method=<method-or-?>`.
- Do **not** fallback-route successful empty-id responses.
- Do **not** broadcast malformed errors.

Add unit tests in `src/socket_proxy.rs`:

- Empty-id error with one pending request is routed to that client with original id restored.
- Empty-id success response is not routed and is dropped/orphaned.
- Empty-id error with no pending request remains orphaned.

**Verify**: `cargo test --message-format=short` → all tests pass, including new tests.

### Step 4: Make downstream initialize lifecycle local

Mimic mcpproxy-go's local downstream initialization behavior while keeping upstream initialized once.

Current cache-hit behavior returns cached `initialize` response. Complete the lifecycle split:

- Track per-client `locally_initialized` state in `handle_client`.
- Track per-client `ClientCapabilities` from the downstream `initialize` request. Store it in a shared `clients`/session map keyed by `client_id`; remove on disconnect.
- When `initialize` is served from cache, mark that client locally initialized.
- Swallow that client's subsequent `notifications/initialized` instead of forwarding upstream.
- Log `pool_cached_initialized_swallowed client_id=<id>`.
- For the first upstream-populating initialize (cache miss), keep existing upstream forwarding so upstream process establishes its own session.
- Do not globally cache downstream client capabilities for routing yet; add a small per-client struct only if needed by Step 6.

Add tests:

- A helper/unit-level test proving cached initialize response sets local lifecycle state and `notifications/initialized` is not forwarded. If testing `handle_client` directly is too heavy, extract a small pure helper that classifies `notifications/initialized` for a locally initialized client and test it.
- Capability parsing test for initialize params with `sampling` and `roots`.

**Verify**: `cargo test --message-format=short` → all tests pass.

### Step 5: Coalesce concurrent `tools/list` and keep last-good tool snapshots

Close the gap with `mcpproxy-go/internal/upstream/managed/client.go:497-637`.

Current cache stores successful `tools/list`, but concurrent clients that miss the cache can still stampede upstream. Use the `ToolsListCache` shape from Step 2:

- If `cached_result` exists: return `build_success_response(original_id, cached_result.clone())` immediately.
- If `in_flight == false`: set `in_flight = true`; current request is the leader and forwards upstream. Store the leader in `request_map` with method `tools/list`.
- If `in_flight == true`: push `PendingWaiter { client_id, original_id, inserted_at }` to `waiters`; do not forward upstream.
- When the leader response returns:
  - set `in_flight = false`
  - drain `waiters`
  - if success (`result`, no `error`):
    - set `cached_result = Some(result.clone())`
    - set `last_good_result = Some(result.clone())`
    - return success to leader and every waiter, each with their original id
  - if error:
    - do not update `cached_result`
    - keep `last_good_result`
    - return the same error body to leader and every waiter, re-id'd per original id
- If `cleanup_stale_requests` or upstream shutdown finds stale waiters: drain them with JSON-RPC error code `-32001` and message `tools/list discovery timed out`.

Cache invalidation:

- On `notifications/tools/list_changed`, invalidate only the current `tools/list` cache and clear any count/derived state.
- Keep the last-good snapshot separately if useful, so transient failed discovery does not erase known tools.

Tests:

- Two concurrent `tools/list` misses result in one upstream request and two downstream responses.
- Failed leader does not cache the error.
- A later successful response repopulates cache.
- `notifications/tools/list_changed` invalidates the cache.

**Verify**: `cargo test --message-format=short` → all tests pass.

### Step 6: Add session-not-found recovery

When Agency/upstream returns `"Session not found"`:

- Detect it from JSON-RPC error response text, not by raw string only where possible:
  - error code `-32001`
  - message contains `Session not found`
- Mark the upstream session for that `SocketProxy` as invalid/recovering.
- Clear `initialize` and `tools/list` caches for that `SocketProxy`.
- Trigger an upstream restart/reinitialize path through a dedicated recovery signal.

Exact design:

- Add a `RecoveryReason` enum (in `socket_proxy.rs` or `mcp_session.rs`):
  - `SessionNotFound`
- Add to `SocketProxy`:
  - `recovery_requested: Arc<AtomicBool>`
  - `recovery_tx: mpsc::Sender<RecoveryReason>`
  - `recovery_rx: Mutex<Option<mpsc::Receiver<RecoveryReason>>>`
- Initialize the channel in `SocketProxy::new`.
- In `SocketProxy::start`, after spawning upstream/router/accept loop, spawn one recovery loop by taking `recovery_rx`.
- Recovery loop:
  - ignores duplicate signals while `recovery_requested` is already true
  - logs `pool_recovery_start reason=session_not_found`
  - calls `stop()` then waits for the exit receiver if present, clears caches, resets shutdown, calls `start()`
  - logs `pool_recovery_done` or `pool_recovery_failed`
  - sets `recovery_requested` false after completion
- If recursive use of `start()` from the recovery task is awkward, extract an internal restart helper that owns the needed Arc state. Do not call async restart while holding any `parking_lot` lock.
- For the current downstream request, return the concrete error to the client (from Step 2). Do **not** retry non-idempotent `tools/call` automatically.
- For discovery requests (`initialize`, `tools/list`), retry once after recovery only if it is safe and bounded. If not, return explicit error and let next client request repopulate.

Tests:

- A session-not-found empty-id error clears caches and emits/reports a recovery signal.
- Non-session errors do not trigger restart.
- Tool-call session-not-found is returned to the client rather than silently retried.

**Verify**: `cargo test --message-format=short` → all tests pass.

### Step 7: Replace last-active server callback routing with capability-aware routing

Current code routes server-initiated requests to `last_active_client`, with fallback to any client. This is explicitly a heuristic and can route callbacks to a client that lacks the needed capability.

Implement minimal capability-aware routing with exact data:

- Parse downstream `initialize` request params for `capabilities`.
- Add `client_capabilities: Arc<Mutex<HashMap<String, ClientCapabilities>>>` to `SocketProxy`, or store capabilities alongside `clients` in a new client state struct.
- Track exactly:
  - `sampling`: true if `params.capabilities.sampling` exists and is an object
  - `roots`: true if `params.capabilities.roots` exists and is an object
- For server-initiated request methods:
  - method prefix `sampling/` → route only to a client with `sampling == true`
  - method prefix `roots/` → route only to a client with `roots == true`
  - unknown callback → keep last-active fallback but log `pool_server_request_fallback method=<method>`.
- If no capable client exists:
  - send an upstream JSON-RPC error response using the server's request id:
    `{"jsonrpc":"2.0","id":<server-id>,"error":{"code":-32001,"message":"no capable downstream client connected for <method>"}}`
  - do not broadcast.

Tests:

- sampling request routes to sampling-capable client, not last-active incapable client.
- roots request routes to roots-capable client.
- no capable client sends a single error response upstream.

**Verify**: `cargo test --message-format=short` → all tests pass.

### Step 8: Add structured lifecycle logs

Make diagnosis like this session possible without parsing giant upstream dumps:

Add concise logs at these boundaries:

- client request accepted: `pool_request_received client_id=<id> method=<method> has_id=<bool> bytes=<n>`
- upstream forward: `pool_request_forwarded client_id=<id> method=<method> pool_id=<id> bytes=<n>`
- cache hit/miss/store/invalidate: method, client_id, server name if available
- response routed: client_id, method if known, elapsed_ms, result vs error
- malformed upstream error fallback: client_id, original id, method, error code/message
- session recovery: server name, reason, outcome

Do not log full request/response payloads by default. Keep upstream stderr capture as-is, but consider prefixing with server name if available.

Tests are not required for logs unless helper functions are added.

**Verify**: manually run a small local mock and inspect logs. Then run `cargo test`.

### Step 9: Real-world verification in PersonalAssistant

Use the real config, but do not edit it:

1. Kill existing processes from PowerShell:
   ```powershell
   Get-Process mcp-pool -ErrorAction SilentlyContinue | ForEach-Object { Stop-Process -Id $_.Id -Force }
   Get-Process agency -ErrorAction SilentlyContinue | ForEach-Object { Stop-Process -Id $_.Id -Force }
   ```
2. Build and install:
   - `cargo build --release --message-format=short`
   - copy `target\release\mcp-pool.exe` to `%USERPROFILE%\.local\bin\mcp-pool.exe`
3. Start debug daemon in tmux from PowerShell:
   ```powershell
   tmux kill-session -t poold 2>$null
   tmux new-session -d -s poold
   tmux send-keys -t poold "cd `"$HOME\Projects\PersonalAssistant`"; mcp-pool serve --debug" Enter
   ```
4. Start Copilot session 1 in tmux:
   ```powershell
   tmux kill-session -t cop1 2>$null
   tmux new-session -d -s cop1
   tmux send-keys -t cop1 "cd `"$HOME\Projects\PersonalAssistant`"; copilot" Enter
   ```
   Wait until MCPs load.
5. Start Copilot session 2 in tmux:
   ```powershell
   tmux kill-session -t cop2 2>$null
   tmux new-session -d -s cop2
   tmux send-keys -t cop2 "cd `"$HOME\Projects\PersonalAssistant`"; copilot" Enter
   ```
6. Inspect `%LOCALAPPDATA%\mcp-pool\logs\mcp-pool.log`.

Expected:

- Session 1 may forward upstream `initialize` / `tools/list` and populate caches.
- Session 2 should log `pool_cache_hit method=initialize` and `pool_cache_hit method=tools/list` for Microsoft MCPs.
- Session 2 should not trigger repeated upstream auth/429 for discovery.
- If Teams/Calendar upstream returns `Session not found`, Copilot should receive an MCP error instead of hanging, and mcp-pool should log recovery initiation.
- `mcp-pool --plain status` should show running servers and current `CONNS` only.

## Test plan

- Extend `src/socket_proxy.rs` module tests. Use existing tests near `route_response_restores_ids_without_cross_wiring` as the structural pattern.
- Add pure helper tests wherever direct socket harnessing is too heavy.
- Required new coverage:
  - empty-id error fallback
  - local cached initialize lifecycle and swallowed `notifications/initialized`
  - tools/list coalescing
  - tools/list_changed invalidation
  - session-not-found recovery signal
  - capability-aware server request routing

## Done criteria

All must hold:

- [ ] `cargo build --message-format=short` exits 0.
- [ ] `cargo clippy --all-targets --message-format=short` exits 0 with no warnings.
- [ ] `cargo test --message-format=short` exits 0.
- [ ] New tests exist for every item in the Test plan.
- [ ] `mcp-pool.exe` installed to `%USERPROFILE%\.local\bin\mcp-pool.exe`.
- [ ] Real PersonalAssistant/Copilot tmux verification completed.
- [ ] Second Copilot session shows cache hits and no upstream discovery storm.
- [ ] Upstream malformed `id:""` errors are returned to clients or trigger recovery; no `pool_response_orphaned id=""` remains for active pending requests.
- [ ] `plans/README.md` status row updated.

## STOP conditions

Stop and report back if:

- The dirty `src/socket_proxy.rs` state does not match this plan's current-state excerpts.
- Step 1 cannot restore compilation quickly.
- Any fix requires changing the per-MCP-visible configuration model.
- Recovery requires modifying Agency or remote Microsoft MCP behavior.
- Server-initiated request routing needs capabilities not present in downstream `initialize` params.
- A verification command fails twice after a reasonable fix attempt.
- You discover cached initialize/tools-list violates Copilot's expected MCP semantics.

## Maintenance notes

- This plan intentionally moves mcp-pool from byte-level multiplexing toward a session-aware MCP proxy. Reviewers should scrutinize protocol semantics more than line count.
- `initialize` capabilities are downstream-session state; do not cache them globally.
- `tools/list` is upstream-server state; it is safe to cache per `SocketProxy`, but must invalidate on list-changed.
- `tools/call` is not cacheable.
- If future MCP protocol versions add new discovery methods, add them to cache/coalescing only after confirming they are upstream-state, not downstream-session-state.
- Keep logs concise; avoid dumping full request bodies because PersonalAssistant logs include user data.
