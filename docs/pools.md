# Pool lifecycle and local security

[README](../README.md) | [Configuration](configuration.md) |
[Shared sessions](shared-sessions.md) | [Bridge](bridge.md)

Native lifecycle commands use the same JSON config. Each caller resolves it; the
daemon does not use its launch directory to resolve definitions and does not warm
servers at launch.

## Add and use a proxy

Configure an example stdio server:

```sh
mcp-cli add echo -- npx -y @modelcontextprotocol/server-everything stdio
mcp-cli pool list
mcp-cli list echo
```

Replace the example command with your own. Alternatively, configure a remote
endpoint with `mcp-cli add remote --url https://example.com/mcp --transport http`;
replace the example URL. `pool list` lists configured servers without discovery;
top-level `list` connects and discovers tools. Top-level `serve` runs the MCP
bridge; `pool serve` runs the daemon.

Configure the agent to run `mcp-cli proxy echo` over stdio, as shown in the
[quick start](../README.md#quick-start). `proxy NAME` and `pool proxy NAME` resolve
merged config, scoped auth, `--config`, and `--root`; they do not fall back to
legacy TOML or mcporter config. Proxy attaches to an existing upstream or starts it
and the daemon. It exits when the pool connection closes, even if agent stdin stays
open. Closing stdin stops new requests but permits pending responses to drain for up
to 800 ms.

## Optional lifecycle controls

The following commands pre-warm, inspect, restart, or stop the example pool.
`start` without a name starts all servers in the caller's merged config.

```sh
mcp-cli start echo
mcp-cli status
mcp-cli restart echo
mcp-cli stop echo
```

When the definition is no longer needed, remove it separately with
`mcp-cli remove echo`. Removing it means the proxy cannot use that definition.

## Foreground daemon

To run the daemon explicitly instead of relying on automatic launch, run this in
a separate terminal. It remains in the foreground:

```sh
mcp-cli pool serve
```

## Daemon control

```sh
mcp-cli daemon start
mcp-cli daemon status --json
mcp-cli daemon restart
mcp-cli daemon stop
```

These commands use the native broker. `daemon status` does not launch a daemon;
native `status` can. Stop retires owned pools and disconnects clients; restart
waits for retirement.

`daemon start` and `daemon restart` accept `--foreground`, `--log`,
`--log-file PATH`, and `--log-servers NAME,...`. Launch options apply only on
launch, so restart to change them. `--log` defaults to `logs/daemon.log` in the
state directory; the sink rotates its retained tail at 5 MiB. Server filters match
exact logical names, not hashes or executable names. General daemon records remain;
unidentified server records are suppressed with a filter. `MCP_CLI_DEBUG=1`
enables diagnostic state-directory logging.

## Legacy retirement

`daemon migrate --json` inventories observed legacy owners as `pid` and `verified`
records; config conversion is a separate [import](imports.md). Legacy retirement
requires `--stop-legacy --confirmed-drained`, drained clients, and authorization.
It requires a same-user local handshake and verified process identities/tree
ownership. The CLI asks the verified runtime to stop rather than signaling
arbitrary PIDs, records a journal, and confirms tree retirement. Unverified owners
are not stopped; unverified retirement retains the journal and blocks cutover.

## Local endpoint security

Unix sockets use mode `0600` and verify same-user peers. Application socket
directories use `0700`. Binding inside a private directory does not change
process-wide umask; sticky-shared parents still require a temporary restrictive
umask during bind.

Windows named pipes use an owner-only DACL, reject remote clients, and verify the
server's user. Cancelled accepts retain their pending instance; a client
disconnecting before accept does not prevent later clients connecting. Raw proxy
attachment retries unavailable or busy endpoints; permanent permission, path,
data, or unsupported-operation errors return immediately.

## Readiness and retirement

`start` confirms transport setup, not MCP initialization. Status shows `starting`,
`running` after setup, and `stopping` during retirement. Readiness distinguishes
bound local `socket`, upstream `transport`, and received `mcp` initialization
result. JSON includes these flags and startup or retirement errors. Discovery
`list` reports tool discovery or an error, not lifecycle status; a running pool
can fail discovery or lack initialization.

`stop`, `restart`, and `shutdown` wait for owned retirement. Failure leaves the
pool `failed` and blocks replacement. Windows children use a Job Object; Unix
children use an owned process group. Children deliberately leaving that group are
outside the boundary. Do not configure servers that daemonize or detach workers.

Shutdown signals all owned generations before waiting for individual retirements,
including replacements from already-admitted restarts, and waits for every result.
If shutdown fails, failed pools remain blocked and visible; the daemon resumes
accepting unrelated lifecycle commands. Restart creates fresh routing and discovery
state and disconnects proxy clients, which must reconnect.

## Pool identity and external endpoints

Pool identities include configured name, config source, credential scope, and
resolved definition. Status uses logical names and JSON includes internal
`pool_name`. The same command under different names or config files can create
separate pools.

Unix daemon startup can discover live sockets owned by other processes as external
endpoints, without acquiring child ownership; Windows named pipes cannot be
enumerated. Daemon shutdown retires owned upstreams.

## Windows process launch

Executable resolution honors configured `PATH` and `PATHEXT`. Native programs
launch directly; batch launchers use Rust batch argument encoding and reject
carriage returns or newlines in arguments. Diagnostic stderr is decoded lossily,
so non-UTF-8 output does not stop a server.
