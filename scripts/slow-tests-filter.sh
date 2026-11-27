#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# The nextest filterset of the slow tests, derived from the source: every test
# carrying `#[ignore = "slow: ..."]` (or `cfg_attr(debug_assertions, ignore = "slow: ...")`)
# is in it, so no list is kept by hand. A reason starting `slow: gpu:` is a test
# that needs a GPU adapter (lavapipe on CI); any other `slow:` reason is CPU only.
#
#   scripts/slow-tests-filter.sh cpu     # the slow tests that need no GPU
#   scripts/slow-tests-filter.sh gpu     # the slow tests that render
#   scripts/slow-tests-filter.sh check   # exit 1 if a tag has no test under it
#
#   cargo nextest run --run-ignored only --workspace -E "$(scripts/slow-tests-filter.sh cpu)"
set -euo pipefail
cd "$(dirname "$0")/.."
kind="${1:?usage: slow-tests-filter.sh cpu|gpu|check}"

# kind<TAB>function name, for each slow tag and the first `fn` below it.
tags="$(git ls-files 'crates/**/*.rs' | xargs awk '
    /ignore = "slow:/ { k = ($0 ~ /ignore = "slow: gpu:/) ? "gpu" : "cpu"; file = FILENAME; line = FNR; pending = 1; next }
    pending && match($0, /fn [A-Za-z0-9_]+/) {
        print k "\t" substr($0, RSTART + 3, RLENGTH - 3); pending = 0; next }
    pending && FNR - line > 8 { print "orphan\t" file ":" line; pending = 0 }
')"

case "$kind" in
    check)
        if printf '%s\n' "$tags" | grep -q '^orphan'; then
            echo "a slow tag with no test under it:" >&2
            printf '%s\n' "$tags" | grep '^orphan' >&2
            exit 1
        fi
        printf '%s\n' "$tags" | cut -f2 | sort | uniq -d | sed 's/^/the name is tagged twice (the filter would match both): /' | { grep . >&2 && exit 1 || true; }
        ;;
    cpu|gpu)
        names="$(printf '%s\n' "$tags" | awk -F'\t' -v k="$kind" '$1 == k { print $2 }' | sort -u)"
        [ -n "$names" ] || { echo "no slow $kind test" >&2; exit 1; }
        printf '%s\n' "$names" | awk 'BEGIN { ORS = "" } { printf "%stest(/(^|::)%s$/)", (NR > 1 ? " | " : ""), $0 } END { print "\n" }'
        ;;
    *) echo "usage: slow-tests-filter.sh cpu|gpu|check" >&2; exit 2 ;;
esac
