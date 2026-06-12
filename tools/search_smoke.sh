#!/usr/bin/env bash
# Smoke check: the address-bar search backend renders real results.
#
# Typing a non-URL query in the omnibox routes to DuckDuckGo's HTML
# results endpoint (see address_entry_to_url) — Google's /search
# serves every client an anti-bot "enablejs" shell with no results,
# so search goes through an engine that server-renders them. This
# asserts a known query's results actually lay out.
#
#   tools/search_smoke.sh
#
# Requires a release build and outbound HTTPS.
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ ! -x target/release/copper ]]; then
    cargo build --release --bin copper >/dev/null
fi

# The exact URL address_entry_to_url("rust programming") resolves to.
dump=$(./target/release/copper layout --width 1400 \
    'https://html.duckduckgo.com/html/?q=rust+programming' 2>/dev/null)

fail=0
require() {
    if echo "$dump" | grep -qE "$2"; then echo "OK  : $1"; else echo "FAIL: $1 (no /$2/)"; fail=1; fi
}
require "search results rendered"   'rust-lang\.org'
require "result snippet text"       'Line .*"Rust"'
if [ $fail -eq 0 ]; then echo "search_smoke: OK"; else echo "search_smoke: FAILED"; exit 1; fi
