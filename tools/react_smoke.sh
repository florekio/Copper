#!/usr/bin/env bash
# React render smoke: serves the vendored React 18 production UMD
# fixture locally and asserts copper's layout contains the tree
# ReactDOM committed. Catches regressions in the JS engine's closure /
# scheduler / DOM-binding machinery that test262 and the page smokes
# can't see (minified-bundle shapes: shared captures, Function.bind
# identity, extracted builtins).
set -u
cd "$(dirname "$0")/.."
PORT=8941
python3 -m http.server $PORT --directory tools/fixtures/react >/dev/null 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT
sleep 0.5

OUT=$(cargo run -q --bin copper -- layout "http://127.0.0.1:$PORT/index.html" 2>/dev/null)
fail=0
check() {
  if echo "$OUT" | grep -q "$1"; then
    echo "OK  : $2"
  else
    echo "FAIL: $2 (no match for $1)"
    fail=1
  fi
}
check 'REACT-SMOKE-HEADING' 'h1 rendered by ReactDOM'
check 'item-alpha' 'first list item'
check 'item-gamma' 'third list item'

OUT=$(cargo run -q --bin copper -- layout "http://127.0.0.1:$PORT/preact.html" 2>/dev/null)
check 'PREACT-SMOKE-HEADING' 'h1 rendered by Preact'
check 'p-item-one' 'preact list item'

# Interactivity: synthetic click -> onClick -> useState -> re-render.
# The fixture polls and prints INTERACTIVE-OK once COUNT:1 commits.
IOUT=$(cargo run -q --bin copper -- layout "http://127.0.0.1:$PORT/counter.html" 2>&1 >/dev/null)
if echo "$IOUT" | grep -q 'INTERACTIVE-OK'; then
  echo "OK  : click -> setState -> re-render"
else
  echo "FAIL: click -> setState -> re-render"
  fail=1
fi
if [ $fail -eq 0 ]; then echo "react_smoke: OK"; else echo "react_smoke: FAILED"; exit 1; fi
