#!/bin/sh
# User-operated shortcut only. Agents must never invoke this script.
# No arguments are accepted.
set -eu

if [ "$#" -ne 0 ]; then
    printf '%s\n' 'reset-dq-caches takes no arguments.' >&2
    exit 2
fi

exec /usr/bin/env -i PATH=/usr/bin:/bin LANG=C \
    /usr/bin/sudo -n -- /usr/local/sbin/qtools-drop-caches
