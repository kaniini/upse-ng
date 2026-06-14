#!/bin/sh
# SPDX-License-Identifier: LGPL-2.1-or-later
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: $0 SHARED_LIBRARY CRATE_DIRECTORY" >&2
    exit 2
fi

library=$1
crate=$2
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

cbindgen --config cbindgen.toml --symfile "$temporary/declared" \
    "$crate" -o "$temporary/header"
sed -n 's/^[[:space:]]*\(upse_[A-Za-z0-9_]*\);$/\1/p' \
    "$temporary/declared" | sort >"$temporary/expected"
nm -D --defined-only "$library" | awk '$3 ~ /^upse_/ { print $3 }' \
    | sort >"$temporary/actual"
diff -u "$temporary/expected" "$temporary/actual"

unexpected=$(nm -D --defined-only "$library" \
    | awk '$2 != "A" && $3 !~ /^upse_/ { print $3 }')
if [ -n "$unexpected" ]; then
    echo "unexpected exported symbols:" >&2
    echo "$unexpected" >&2
    exit 1
fi
