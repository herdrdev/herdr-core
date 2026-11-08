#!/bin/sh
# Stands in for the C compiler and archiver when only type-checking another
# target (clippy, check): C in dependencies (ring) needs that target's
# headers, but nothing links. Writes an empty output where one is asked for.
#   CC_x86_64_pc_windows_msvc=scripts/cc-stub.sh AR_x86_64_pc_windows_msvc=scripts/cc-stub.sh
out=
while [ $# -gt 0 ]; do
    case $1 in
        -o) out=$2; shift ;;
        -Fo* | /Fo*) out=$(printf '%s' "$1" | cut -c4-) ;;
        -out:* | /OUT:* | /out:*) out=$(printf '%s' "$1" | cut -d: -f2-) ;;
        -E) exit 0 ;; # compiler detection: nothing to report
    esac
    shift
done
if [ -n "$out" ]; then : >"$out"; fi
exit 0
