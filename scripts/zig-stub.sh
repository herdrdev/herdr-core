#!/bin/sh
# Stands in for zig when only type-checking another target (clippy, check):
# leaves an empty libghostty-vt where the build script looks. Nothing links.
#   ZIG=scripts/zig-stub.sh cargo clippy --target x86_64-pc-windows-msvc
# `zig ar` (editing that archive) has nothing to do.
[ "$1" = ar ] && exit 0
while [ $# -gt 0 ]; do
    [ "$1" = --prefix ] && prefix=$2
    shift
done
mkdir -p "$prefix/lib"
: >"$prefix/lib/ghostty-vt-static.lib"
: >"$prefix/lib/libghostty-vt.a"
