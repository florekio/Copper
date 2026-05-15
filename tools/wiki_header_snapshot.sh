#!/usr/bin/env bash
# Snapshot check: render Wikipedia's Cat article at width 1400 and
# assert that the top header layout matches the rough shape of the
# real site (see wiki-real.png). Run manually:
#
#     tools/wiki_header_snapshot.sh
#
# Exits non-zero on any assertion failure. Requires a release build
# of copper and outbound HTTPS to en.wikipedia.org.

set -euo pipefail
cd "$(dirname "$0")/.."

if [[ ! -x target/release/copper ]]; then
    cargo build --release --bin copper >/dev/null
fi

# Replace the multibyte × with a space so plain awk can split.
dump=$(./target/release/copper layout --width 1400 \
    https://en.wikipedia.org/wiki/Cat 2>/dev/null \
    | sed 's/×/ /g')

# Helper: find first line matching $1 and emit "x y w h".
# Each dump line looks like:  "[ x, y w  h] Type label ..."  after the × sub.
extract_frame() {
    echo "$dump" | grep -E "$1" | head -1 \
        | sed -E 's/^[[:space:]]*\[[[:space:]]*//; s/\].*$//; s/,/ /'
}

fail=0
check_lt() {
    local label="$1" value="$2" limit="$3"
    echo "$label: $value (limit < $limit)"
    if [[ -z "$value" ]] || (( $(echo "$value >= $limit" | bc -l) )); then
        echo "  FAIL"
        fail=1
    fi
}
check_ge() {
    local label="$1" value="$2" floor="$3"
    echo "$label: $value (floor >= $floor)"
    if [[ -z "$value" ]] || (( $(echo "$value < $floor" | bc -l) )); then
        echo "  FAIL"
        fail=1
    fi
}

# Each frame is "x y w h" after extract_frame.
header=$(extract_frame "Flex *header\.vector-header ")
header_h=$(awk '{print $4}' <<<"$header")
check_lt "vector-header height" "$header_h" 70

logo=$(extract_frame "Flex *a\.mw-logo ")
logo_w=$(awk '{print $3}' <<<"$logo")
check_ge "a.mw-logo width" "$logo_w" 140

search=$(extract_frame "Flex *form#searchform")
search_h=$(awk '{print $4}' <<<"$search")
check_lt "form#searchform height" "$search_h" 60

# 4. Dropdown contents must remain opacity:0 or visibility:hidden — without
#    one of those, the (otherwise-laid-out) menu items would paint over
#    the page body. The layout dump tags either as "op=0.00" / "vis=hid".
if ! echo "$dump" | grep -E "vector-dropdown-content.*(op=0\.00|vis=hid)" >/dev/null; then
    echo "FAIL: .vector-dropdown-content lost its opacity:0 / visibility:hidden"
    fail=1
else
    echo "vector-dropdown-content remains hidden: OK"
fi

if (( fail )); then
    echo "wiki_header_snapshot: FAILED"
    exit 1
fi
echo "wiki_header_snapshot: OK"
