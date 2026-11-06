#!/usr/bin/env bash
set -euo pipefail

MCP_CLI_HOME="$(mktemp -d /tmp/mp-ci.XXXXXX)"
export MCP_CLI_HOME
export MCP_CLI_CREDENTIALS_READ_ONLY=1
trap 'rm -rf "$MCP_CLI_HOME"' EXIT

cargo test --locked --bin mcp-cli -- local_security transport::tests tool_documentation::tests config_commands::output::tests --test-threads=1
cargo test --locked --test cli_presentation -- --test-threads=1
