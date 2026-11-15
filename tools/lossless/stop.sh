#!/usr/bin/env bash
# Stop the wrapper that start.sh launched (identified by its pidfile and start time); it never
# touches a wrapper started by anything else, even one answering on the same port.
set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
exec python3 -I -- "$here/wrapperctl.py" stop "$@"
