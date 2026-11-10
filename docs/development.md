# Development and validation

[README](../README.md) | [Architecture](architecture.md) | [Installation](installation.md)

## Build and test

```sh
cargo test
cargo clippy --all-targets
bacon --job run
bacon --job clippy
```

On Windows, `.\dev.ps1` starts the `bacon --job run` watcher. On macOS/Linux, use
`./dev.sh`. With `ast-grep` installed,
`python3 scripts/rust_codebase_map.py --format dot` regenerates the syntax
dependency graph; `--format skeleton` lists function signatures. The
[CLI UX audit](cli-ux-audit.md) is a historical comparison with mcporter using
isolated fixtures and transcripts.

## Opt-in push checks

The checked-in toolchain pins Rust 1.95.0 with rustfmt and Clippy. Prepare the
toolchain, locked dependencies, and Python 3.11+ before opting in:

```sh
rustup toolchain install 1.95.0 --profile minimal --component rustfmt --component clippy
cargo fetch --locked
python3 scripts/test_pre_push.py -v
./scripts/install-hooks.sh
```

The tracked executable hook is not installed automatically. The installer is
idempotent, supports linked worktrees, refuses unrelated active hooks or
`core.hooksPath`, and changes repository-local config only. Default hooks behind
symlinked directories are preserved. Narrow `.gitattributes` rules keep shell
entrypoint and installer on LF under `core.autocrlf`; Git-status errors fail closed.

### Push requirements

Push validation requires a clean checkout including untracked files. Every
nondeleted pushed ref must resolve to checked-out `HEAD`. Multiple refs, new
branches, missing base objects, empty input/diffs, renames, and deletions broaden
checks conservatively; deletion-only updates run no project commands. No fetching,
stashing, or source rewriting occurs.

### Checks and platform limits

The hook uses the installed pinned toolchain, `cargo fmt --all -- --check`, and
`cargo check --offline --locked --bin mcp-cli`. Missing tools, components, or
dependencies fail with setup instructions; it downloads nothing. Source,
dependency, shared config, and uncertain changes run the `local_security` and
`transport::tests` filters serially on Unix. The broader CI gate also runs
documentation, config-output, and `cli_presentation` tests. Run
`bash scripts/test_unix_ipc.sh` for that gate. Socket fixtures use a fresh private
short `/tmp` MCP home, cleaned on failure, never user state. Markdown-only changes
retain format/check but omit the IPC runtime gate. Failures propagate.

Unix tests are not Windows acceptance. Non-Unix hosts report that Unix IPC remains
required in CI; Windows named-pipe/process behavior and the full suite require
native qualification. The hook does not invoke MCP commands, refresh tokens,
launch providers, import credentials, or restart daemons. Fixture tests use stub
commands in disposable Git repositories and do not push. Run full project tests
separately.

The existing `unix-ipc` workflow uses Boat on pull requests and manual dispatch. It
reads the compiler pin from `rust-toolchain.toml`, prepares Python, and runs hook
fixtures plus `bash scripts/test_unix_ipc.sh`. The same gate runs locally without
parsing workflow YAML. Owner-only public execution follows the separately configured
actor policy; outsiders and bots are not implicitly authorized.

## Benchmarks

Unix Python scripts in `benchmarks/` compare isolated fixtures, not live configured
servers. They measure CLI and retained-bridge workloads, verify tool results, and
record process memory separately from latency. Use their help to select binaries,
protocols, transports, and concurrency. Synthetic throughput is not hosted capacity;
summed RSS is not unique physical memory.

```sh
cargo build --release --locked
python3 benchmarks/run.py --products mcp-cli --mcp-cli target/release/mcp-cli --self-test
python3 benchmarks/run.py --products mcp-cli --mcp-cli target/release/mcp-cli \
  --cases bridge,hol --bridge-modes 1,8
```

Short-lived CLI commands default to one Tokio worker. Daemon, bridge, proxy, and
native `pool` commands retain Tokio defaults. `TOKIO_WORKER_THREADS` overrides this
choice.
