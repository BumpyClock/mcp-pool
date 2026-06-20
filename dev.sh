#!/usr/bin/env sh
set -eu

cd "$(dirname "$0")"
export RUST_BACKTRACE=full

bacon --job run
