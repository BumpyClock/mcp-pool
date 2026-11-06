#!/bin/sh
set -eu
home=$MCP_CLI_HOME
case "$1 $2" in
    'pool serve')
        if test -f "$home/exit-early"; then exit 0; fi
        while ! test -f "$home/stopped"; do sleep 0.02; done
        ;;
    'daemon stop')
        if test -f "$home/fail"; then echo 'retirement unverified' >&2; exit 9; fi
        if test -f "$home/hang"; then exec sleep 60; fi
        if ! test -f "$home/stay-alive"; then touch "$home/stopped"; fi
        echo '{"running":false}'
        ;;
    'daemon status')
        if test -f "$home/startup-not-ready"; then
            rm "$home/startup-not-ready"
            echo 'listener setup still in progress' >&2
            exit 1
        fi
        if test -f "$home/exit-early"; then exec sleep 60; fi
        if test -f "$home/malformed"; then echo '{"running":"yes"}'; exit 0; fi
        if test -f "$home/status-hang"; then exec sleep 60; fi
        if test -f "$home/stopped"; then echo '{"running":false}'; else echo '{"running":true}'; fi
        ;;
    *) exit 2 ;;
esac
