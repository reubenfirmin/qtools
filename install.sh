#!/bin/bash
set -e

# Running as root (e.g. `sudo ./install.sh`) installs system-wide instead of just for one user.
# Note sudo resets $HOME to root's home by default, so without this check a `sudo` run would
# silently install into /root/.local/bin: not on any regular user's PATH.
if [ "$(id -u)" -eq 0 ]; then
	BIN_DIR="/usr/local/bin"
else
	BIN_DIR="$HOME/.local/bin"
fi

if [ ! -f ./dq ]; then
	echo "./dq not found. Run ./build.sh first." >&2
	exit 1
fi

mkdir -p "$BIN_DIR"
cp ./dq "$BIN_DIR/dq"
echo "Installed dq to $BIN_DIR/dq"

if [ -f ./pq ]; then
	cp ./pq "$BIN_DIR/pq"
	echo "Installed pq to $BIN_DIR/pq"
fi

case ":$PATH:" in
	*":$BIN_DIR:"*) ;;
	*) echo "Note: $BIN_DIR is not on your PATH. Add it, e.g. export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac
