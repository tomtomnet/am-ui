#!/bin/bash
# Build tools/fake-input.c into target/tools/fake-input (needs wayland-scanner and the Wayland client
# library's development files, which GTK's pull in) and print where it is.
set -eu
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
out=$here/../../../target/tools
mkdir -p "$out"
if [ ! -x "$out/fake-input" ] || [ "$here/fake-input.c" -nt "$out/fake-input" ] || [ "$here/fake-input.xml" -nt "$out/fake-input" ]; then
    wayland-scanner client-header "$here/fake-input.xml" "$out/fake-input-client.h"
    wayland-scanner private-code "$here/fake-input.xml" "$out/fake-input-protocol.c"
    # shellcheck disable=SC2046
    cc -O1 -Wall -Wextra -I"$out" -o "$out/fake-input" "$here/fake-input.c" "$out/fake-input-protocol.c" $(pkg-config --cflags --libs wayland-client)
fi
cd "$out" && printf '%s/fake-input\n' "$(pwd -P)"
