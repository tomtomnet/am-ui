#!/usr/bin/env bash
# Health check of the lossless-route wrapper: process, HTTP /status on 127.0.0.1, storefronts,
# signed-in state (file presence only; no token contents). Exit 0 when the service answers.
set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
exec python3 -I -- "$here/wrapperctl.py" status "$@"
