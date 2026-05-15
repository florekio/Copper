#!/usr/bin/env bash
# Smoke check: load google.com, assert the search form is present
# and the binary survives Google's heavy inline JS.
#
#   tools/google_smoke.sh
#
# Exits non-zero on failure. Requires a release build and outbound
# HTTPS to www.google.com.

set -euo pipefail
cd "$(dirname "$0")/.."

if [[ ! -x target/release/copper ]]; then
    cargo build --release --bin copper >/dev/null
fi

dump=$(./target/release/copper layout --width 1400 \
    https://www.google.com/ 2>/dev/null \
    | sed 's/×/ /g')

fail=0
require() {
    local label="$1" pattern="$2"
    if echo "$dump" | grep -qE "$pattern"; then
        echo "OK  : $label"
    else
        echo "FAIL: $label (no match for /$pattern/)"
        fail=1
    fi
}

# 1. Body painted — binary didn't die mid-fetch.
require "body element rendered" "Block.*body \(fs="
# 2. The form is in the layout tree (action="/search" routes through
#    chrome's native form-submit handler; gbv=1 rewrite produces the
#    legacy results page).
require "search form present"   "Block.*form "
# 3. At least one <ctrl> survives — the textarea (modern Google) or
#    input (legacy gbv=1) that takes the query.
require "search input present"  "Line .*<ctrl>"

if (( fail )); then
    echo "google_smoke: FAILED"
    exit 1
fi
echo "google_smoke: OK"
