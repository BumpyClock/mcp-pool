# Installation and upgrades

[README](../README.md) | [Development](development.md) | [Pools](pools.md)

## Build and install

Build the current checkout with Rust:

```sh
cargo build --release
# binary: target/release/mcp-cli (mcp-cli.exe on Windows)
```

Install the checkout explicitly:

```sh
sh ./install.sh
sh ./install.sh --bin-dir "$HOME/.local/bin"
```

```powershell
.\install.ps1
.\install.ps1 -BinDir "$HOME\local\.bin"
```

The installers default to `~/.local/bin`, build the host target from this repository
even when invoked elsewhere, and validate the staged executable before replacement.
Hashing or validation failure retains the previous executable and permissions. A
locked Windows executable fails without stopping its daemon. Scripts do not change
`PATH` and warn when another executable would be selected.

`MCP_CLI_BIN_DIR` sets the default directory; `MCP_POOL_BIN_DIR` is its fallback.
An explicit directory option takes precedence. `mcp-cli --version` includes the
Git revision and source fingerprint, including dirty source changes.

## Upgrade from mcp-pool

Before upgrading, drain clients and stop the old daemon with the old executable
using `mcp-pool daemon stop`. Repeat for each original `MCP_POOL_HOME` override with
that same value. Before reconnecting, change agent proxy commands and automation
from `mcp-pool` to `mcp-cli`; an old proxy command can start the old daemon again.
Socket names changed, so the new CLI cannot discover or retire old-brand daemons.
Running both can create duplicate upstreams that still share config and credentials.

Runtime state is not migrated; config and credentials remain in their configured
locations and are not moved or deleted. The default home location is `~/.mcp/`.
A `MCP_CLI_HOME` override (or `MCP_POOL_HOME` fallback) uses its `config/` directory.
Credentials for an explicit config are beside that config unless a credential cache
path is configured. See [imports](imports.md) to convert former native TOML
definitions and [mcporter legacy retirement](pools.md#legacy-retirement) for
mcporter daemon migration, not verification of `mcp-pool` shutdown.

The legacy `import mcp-pool` selector, `_mcp_pool_timeout_ms` local wire field,
`__mcp_pool_commit` credential metadata, and `mcp-pool-legacy-v1` retirement
journals retain their names for compatibility.
