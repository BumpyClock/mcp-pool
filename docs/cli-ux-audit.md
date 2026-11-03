# CLI UX audit

## Contract

Compare supported mcp-pool commands with mcporter using identical local MCP tool
schemas, calls, resources, and synthetic OAuth credentials. Check readable human
output, useful help, reliable errors, and clean machine output. Native pool
commands have no mcporter equivalent. Byte-for-byte parity and mcporter's
generator, SDK, record, and replay features are not goals.

## Reproduce

Requires Python 3.11+, mcporter on PATH, and a built mcp-pool executable.
The runner targets macOS/Linux. It does not qualify Windows behavior.

```sh
cargo build --release --locked
python3 -m unittest discover -s scripts -p 'test_cli_ux*.py' -v
python3 scripts/cli_ux_audit.py \
  --mcp-pool target/release/mcp-pool \
  --output .test-artifacts/cli-ux-new
```

Choose a new output directory for every run. `results.json` records arguments,
exit code, stdout, stderr, and failed assertions for every scenario. Individual
text transcripts support side-by-side review. The exit status gates mcp-pool;
mcporter reference differences are reported separately. They require assessment
against each product's contract rather than automatic adoption.

Results under `.test-artifacts/` are ignored local artifacts, not repository
files. Record exact versions, findings, measurements, and run outcomes in the PR.
A fresh clone can generate new evidence, not recreate historical results.

## Isolation and cleanup

The runner uses private short-lived homes, explicit config/root selection,
isolated daemon directories, loopback servers, and synthetic tokens. It strips
inherited MCP/XDG/Node settings. OAuth tests reuse seeded tokens without consent.
All mutations and daemon operations target owned fixtures.

One process owner registers each direct child before setup continues and shares
bounded terminate/wait/kill/reap cleanup across HTTP, RPC, and foreground
processes. Partial setup rolls back acquired children. Unverified daemon or child
retirement retains the isolated state path instead of unlinking a potentially
live socket. Cleanup errors remain visible to the caller.

## Coverage

- Root and command help, including config/daemon/vault subcommands and aliases.
- List/describe/list-tools, server/tool selectors, brief/schema/all-parameter,
  source/status/JSON modes, hidden optional names, and auth-required reporting.
- Typed calls, JSON stdin, function-call syntax, inferred selectors, ad-hoc stdio
  and HTTP, all output formats, plain text, and tool failures.
- Resource list/read/JSON and the resources alias.
- Vault set/file/stdin/clear, cached auth/login, logout, and doctor health.
- Config list/get/add/remove, explicit import, dry run, and idempotent import.
- Daemon start/status/stop/restart, foreground operation, and read-only migration.
- Native pool list/serve, start/all/stop/restart/status, add/remove, both proxy
  forms, shutdown, and stdio/HTTP tool bridges with correlated RPC responses.
- Invalid arguments, unknown servers/tools, malformed JSON, quiet failure,
  redirected ANSI noise, leaked synthetic credentials, and incorrect typed JSON.

Run the Rust suite separately. It covers protocol, concurrency, converter
formats, image/log artifacts, terminal color/progress, and lifecycle cases beyond
the audit fixture. Presentation tests check string/object lifecycle equivalence,
unchanged JSON, resolved argument disclosure, and escaped control characters.

Browser consent, actual account writes, confirmed legacy retirement, and real
external tool calls are deliberately excluded from the runner.

## Performance checks

Use the existing release benchmark runner without changing its measurement logic:

```sh
python3 benchmarks/run.py --mcp-pool "$PWD/target/release/mcp-pool" \
  --cases cold,warm,memory --transports stdio,http --protocols legacy \
  --cold-rounds 5 --warm-rounds 12 --clients 1,8 --repeats 5 \
  --output benchmarks/results/cli-ux-new
```

Benchmark output directories are also ignored local artifacts. Every call token
and the single-upstream invariant are verified. Failed operations fail the run;
they are not omitted from results or automatically replayed.

Record counts, ranges, binary versions, and measurement limits with each result.
Cold means a fresh daemon namespace, not cleared disk caches. Memory totals
include owned fixtures and double-count shared pages. Keep other local builds
and audits out of measurement windows. These local observations do not establish
throughput or saturation capacity.

## Known inherited limitation

Unix cold starts can fail with
`local socket endpoint does not have private mode 0600`.
The existing bind path creates a socket inside a private directory, then
chmods it. A same-user client can inspect it between those operations. Retain
failed results rather than silently retrying. Fixing the IPC publication policy
is separate from this presentation audit.
