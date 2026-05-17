#!/usr/bin/env bash
# Smoke check: load MDN's JavaScript landing page and assert it
# renders past the inline theme-setup script and the third-party
# consent script. MDN's main content is server-rendered, so the
# h1, sidebar nav, and intro paragraphs should all appear in the
# layout tree regardless of whether the modules load fully.
#
#   tools/mdn_smoke.sh
#
# Exits non-zero on failure. Requires a release build and outbound
# HTTPS to developer.mozilla.org.

set -euo pipefail
cd "$(dirname "$0")/.."

if [[ ! -x target/release/copper ]]; then
    cargo build --release --bin copper >/dev/null
fi

dump=$(./target/release/copper layout --width 1400 \
    https://developer.mozilla.org/en-US/docs/Web/JavaScript 2>/dev/null \
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

# 1. Body painted with meaningful height — Grid layout used by MDN's
#    page-layout container; height includes article content.
require "body grid laid out tall" '1400 [1-9][0-9]{3,}\] Grid +body\.page-layout'

# 2. The h1 "JavaScript" — assertion of the page's primary heading.
#    The localStorage / documentElement.dataset.theme fix is what
#    keeps the inline script from aborting and letting the rest of
#    the page proceed.
require "h1 'JavaScript' rendered" 'Line  +"JavaScript"$'

# 3. At least one section link from the docs hub.
require "doc hub links present"  'Line .* "The" "JavaScript" "(guide|reference)"'
# 4. The "frameworks" section header — page rendered far enough down
#    to include this anchor, proving the full article paint isn't
#    truncated by the early-fail of an inline script.
require "frameworks section"     'Line .* "JavaScript" "frameworks"'

if (( fail )); then
    echo "mdn_smoke: FAILED"
    exit 1
fi
echo "mdn_smoke: OK"
