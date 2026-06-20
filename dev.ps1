# Run dev PowerShell script with Rust backtrace and bacon.

$ErrorActionPreference = "Stop"

$env:RUST_BACKTRACE = "full"

Set-Location $PSScriptRoot

bacon --job run
