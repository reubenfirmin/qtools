#!/bin/sh
# Install a root-owned copy; whitelist only that copy with no arguments.
set -eu

if [ "$#" -ne 0 ]; then
    printf '%s\n' 'qtools-drop-caches takes no arguments.' >&2
    exit 2
fi

if [ "$(/usr/bin/id -u)" -ne 0 ]; then
    printf '%s\n' 'qtools-drop-caches must run as root.' >&2
    exit 1
fi

/usr/bin/sync
printf '3\n' > /proc/sys/vm/drop_caches
