#!/bin/sh
set -eu
root=$(git rev-parse --show-toplevel)
cd "$root"
[ -x .githooks/pre-push ] || {
    echo 'install-hooks: tracked .githooks/pre-push must be executable' >&2
    exit 1
}
current=$(git config --get core.hooksPath || true)
if [ "$current" = .githooks ]; then
    echo 'Tracked hooks already selected.'
    exit 0
fi
if [ -n "$current" ]; then
    echo 'install-hooks: existing core.hooksPath is unrelated; reconcile it manually first' >&2
    exit 1
fi
hooks=$(git rev-parse --git-path hooks)
if [ -d "$hooks" ]; then
    for hook in "$hooks"/*; do
        [ -e "$hook" ] || continue
        case "$hook" in *.sample) continue ;; esac
        echo 'install-hooks: existing active hooks found; reconcile them manually first' >&2
        exit 1
    done
fi
git config --local core.hooksPath .githooks
echo 'Selected tracked hooks for this repository. No tools installed.'
