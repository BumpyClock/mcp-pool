$ErrorActionPreference = "Stop"

$env:RUST_BACKTRACE = "full"

Set-Location $PSScriptRoot

bacon --job run
