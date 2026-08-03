#!/bin/sh
# SPDX-License-Identifier: LGPL-2.1-or-later
set -eu

mode=${1:-normal}
root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
fixture_dir="$root/build/tests/fixtures"
binary_dir="$root/build/tests/bin-$mode"
stage=$(mktemp -d /tmp/upse-ng-c-api.XXXXXX)
trap 'rm -rf "$stage"' EXIT HUP INT TERM

python3 "$root/tests/fixtures/generate.py" "$fixture_dir"
make -C "$root" DESTDIR="$stage" install
mkdir -p "$binary_dir"

export PKG_CONFIG_PATH="$stage/usr/local/lib/pkgconfig"
export PKG_CONFIG_SYSROOT_DIR="$stage"
export LD_LIBRARY_PATH="$stage/usr/local/lib"

cflags=$(pkg-config --cflags libupse-ng)
libs=$(pkg-config --libs libupse-ng)
sanitizers=
if [ "$mode" = sanitize ]; then
    sanitizers="-fsanitize=address,undefined -fno-omit-frame-pointer"
elif [ "$mode" != normal ]; then
    echo "unknown C API test mode: $mode" >&2
    exit 2
fi

# Deliberate word splitting applies pkg-config's compiler and linker tokens.
# shellcheck disable=SC2086
cc -std=c11 -Wall -Wextra -Werror -pedantic $sanitizers $cflags \
    "$root/tests/c-api/test.c" -o "$binary_dir/test-c" $libs -pthread
# shellcheck disable=SC2086
c++ -std=c++17 -Wall -Wextra -Werror -pedantic $sanitizers $cflags \
    "$root/tests/c-api/test.cpp" -o "$binary_dir/test-cxx" $libs -pthread

"$binary_dir/test-c" "$fixture_dir/synthetic.psf" \
    "$fixture_dir/synthetic.minipsf" "$fixture_dir/library.psflib" \
    "$fixture_dir/synthetic.psf2"
"$binary_dir/test-cxx" "$fixture_dir/synthetic.psf"

make -C "$root/examples" clean
make -C "$root/examples"
"$root/examples/upse123" --version
"$root/examples/upse123" --driver null --seek 0.005 \
    "$fixture_dir/synthetic.psf"
"$root/examples/upse123" --driver null --seek 00:00:00.005 \
    "$fixture_dir/synthetic.psf"
"$root/examples/upse123" --driver null "$fixture_dir/synthetic.psf2"
if "$root/examples/upse123" --driver null --seek 00:60:00 \
    "$fixture_dir/synthetic.psf"; then
    echo "upse123 accepted an invalid seek time" >&2
    exit 1
fi
