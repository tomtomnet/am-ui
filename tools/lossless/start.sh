#!/usr/bin/env bash
# Start the lossless-route FairPlay wrapper detached on 127.0.0.1:${LOSSLESS_PORT:-12351}.
# Logged out, it still answers /status (empty storefront list); sign in first (Connect in the app, or login.sh).
set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
exec python3 -I -- "$here/wrapperctl.py" start "$@"
