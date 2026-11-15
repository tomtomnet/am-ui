#!/usr/bin/env bash
# Apple's native (Android) sign-in from a terminal: the same sign-in the am-ui window runs behind its
# Connect button, and the one am-ui uses for the whole app (it is also what unlocks lossless).
# It explains what will be asked and where state is stored, then hands over to the wrapper's own
# prompts (password hidden, then "2FA code:"). The service must be stopped first (stop.sh), or pass
# --stop-service to stop the one this helper started. `login.sh --explain-only` prints the notice
# without asking for anything.
set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
exec python3 -I -- "$here/wrapperctl.py" login "$@"
