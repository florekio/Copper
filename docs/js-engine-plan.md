# JS engine milestone: real Google search end-to-end

## Why

Google (and most modern sites) have killed every non-JS path:
`gbv=1`, mobile XHTML, Lynx-compatible endpoints — all confirmed
to return either a `<noscript><meta refresh>` retry loop or a
"please upgrade your browser" notice. The only way to render
Google's modern search results is to actually run the inline JS
shell, let it intercept the form submit, let it XHR for results,
and paint the response. The current DDG redirect (`crates/bui/
src/main.rs:maybe_rewrite_google_search`) is a pragmatic
stand-in; this milestone replaces it.

The work is also load-bearing for any site that gates content on
client-side JS — which is most of the modern web. Treat Google as
the canonical hard target; smaller sites should fall out of the
same plumbing.

Update (July 2026) — heavy-SPA bring-up (DuckDuckGo SERP):

Real JS-heavy result pages (the DDG SERP as the canonical target)
now get much further. The blockers were engine-correctness and
missing-DOM-surface bugs, not throughput — each fix is load-bearing
for any Closure/webpack/jQuery site:

- **RegExp**: global/sticky `exec` now advances `lastIndex` (was an
  infinite loop on `while ((m = re.exec(s)))` — the original SERP
  fuel-exhaustion blocker); `replace`/`replaceAll` fire a function
  replacement per match; `matchAll` implemented; Rust-built arrays
  (split/match/matchAll + the embedder `alloc_array`) now carry
  `Array.prototype` so `for…of` over them works.
- **Objects/functions**: `fn[key] = v` (computed writes onto function
  values) now store — jQuery *is* a function and `jQuery.extend`
  copies its statics this way, so this unblocked jQuery init.
- **for-in**: fixed a compiler bug where a nested `for-in` clobbered
  the outer loop's iterator (hoisted-loop-var slot aliasing) — this
  was DDG's localization (Jed/Gettext) blocker.
- **DOM**: `document.childNodes` + `document.nodeType`, and element
  `getElementsByTagName`/`getElementsByClassName` (Sizzle/jQuery
  feature-detection needs them).

Result: the SERP clears the fuel wall, jQuery loads and defines `$`,
and the locale/gettext layer initializes. Remaining failures are in
DDG's own framework layer (a templating `registerHelper`, an
`instanceof` on an undefined RHS, `DDG.deep.*`). Diagnostics added:
`ZINC_FUEL_TRACE=1` (locate runaway loops), `COPPER_DUMP_SCRIPTS=1`
(dump each executed script), `COPPER_MAX_STEPS` (override the fuel
budget). Note: copper's error "line N" is a source **byte offset**,
not a file line, on minified bundles.

Milestone status (May 2026):

Phases 1–5 shipped. Phase 6 shipped on the engine side and on
the host side for form submit (Enter in a focused input + click
on a submit-style control). A `submit` event now fires through
`EventListenerMap::dispatch_js` before the chrome's default
navigate; `event.preventDefault()` flips a Rust-side flag so
the host suppresses its native form submit, and a handler that
sets `location.href = …` drains as the navigation target on
the same input frame. The smoke test still asserts the
structural homepage form rather than `<h3>` results — wiring
`click` / `keydown` events the same way is the next iteration
once we have a real Google session to drive. Concretely:

- `bui-js` runs inline `<script>` tags through Zinc with
  read+write DOM bindings.
- `EventListenerMap` accepts both Rust closures and JS callables
  (`Listener::Js(Value)`); `dispatch_js` re-enters the VM via
  `host_call`.
- `__addEventListener` registers JS callbacks on (NodeId, type,
  capture) tuples; tested end-to-end in
  `events::tests::js_listener_fires_via_dispatch_js` and
  `tests::element_level_add_event_listener_fires_on_target`.
- `Event.preventDefault()` / `stopPropagation()` are host fns
  bound to a per-`JsContext` atomic; `dispatch_js` snapshots and
  folds the atomic into `Event.flags` so a JS handler can
  cancel the host's default action
  (`events::tests::js_listener_can_prevent_default`).
- `JsContext` (in `crates/bui-js/src/lib.rs`) owns the
  `Engine` + `BindingContext` for the lifetime of a page and
  exposes `dispatch(event)` so user-input frames re-enter the
  same VM the inline scripts set up
  (`tests::js_context_dispatches_submit_after_script_pass`).
- `fetch(url)` is synchronous, backed by an embedder-supplied
  `Fetcher` closure that bridges to `shared_client()` and
  records into the dev-dock XHR tab.
- `setTimeout(fn, 0)` fires synchronously after the script
  returns; non-zero delays are dropped (acceptable for Google's
  microtask-shim use of `setTimeout(…, 0)`).
- `__navigate` side channel handles `location.href` /
  `assign` / `replace` and feeds back into the host's
  `navigate_to`.
- Zinc patches landed: `close_upvalues_above` bounds-checks
  stale slots (closes to `undefined` instead of panicking);
  `Vm::alloc_host_object` mirrors `Engine::alloc_host_object`
  so `document.createElement` can mint host handles from inside
  a host-fn callback. `panic::catch_unwind` is gone.

## Goal

A user types a query into Google's `<textarea name="q">` on
`google.com`, presses Enter, and lands on a search-results page
that renders Google's actual results (the XHR-driven, modern,
JS-built results list). Every link is clickable; the URL bar
shows a real Google URL.

Done = the `tools/google_smoke.sh` script asserts (1) the
homepage submits without error, (2) a `<h3>` element appears in
the results layout within 10 seconds of the form submit.

## Non-goals

- Google Maps / Images / News / autocomplete suggestions
  dropdown. Just text search.
- Implementing Service Workers, WebGL, WebSocket, WASM, or any
  of the long-tail browser APIs Google uses for its richer
  features. Stubs return undefined and we accept that some
  features will degrade.
- Removing all of Google's logged exceptions. We aim for the
  "right enough" subset.

## Phases

### Phase 1 — Zinc upvalue-closing patch

**Why first:** every other phase depends on Zinc not panicking
when scripts get complex. The `catch_unwind` swallow is a
band-aid; once we start firing event callbacks the panic
frequency goes up.

**Scope (Zinc side, `~/Desktop/browser` repo):**
- Fix `close_upvalues_from` in `src/vm/vm.rs:663`: the loop
  indexes `self.stack[stack_idx]` where `stack_idx` can exceed
  `self.stack.len()` after a stack-shrink. Bound-check and skip
  closed upvalues whose original slot is past the current frame.
- Add a regression test in Zinc that compiles a synthetic-but-
  representative ~200-statement script with nested closures and
  immediate stack shrinks. The Google homepage shell minified
  is the worst case in the wild.
- Cut a tagged release on the Zinc repo (`v0.6.0` or whatever)
  so `bui-js` can pin to it.

**Verification:** strip `panic::catch_unwind` from
`bui_js::execute_inline_scripts_with_dom` and re-run
`./target/release/copper layout --width 1400
https://www.google.com/`. No panics in stderr.

### Phase 2 — Zinc::Vm::alloc_host_object

**Why second:** unblocks `document.createElement` /
`createTextNode`, which Google's submit-intercept code uses to
build the results-list DOM. Without this every dynamic
DOM-build operation errors out.

**Scope (Zinc side):**
- Mirror `Engine::alloc_host_object` onto `Vm`: same shape, but
  callable from inside a `register_host_fn` closure (which only
  has `&mut Vm`).
- Bui side: drop the `engine_alloc_host_object_via_vm` stub at
  `crates/bui-js/src/dom_bindings.rs:720`, replace with the real
  call. Existing test `host_objects_round_trip_through_js` and
  the `#[ignore]`'d createElement-followed-by-appendChild test
  (line 1125) are the acceptance.

**Verification:** un-`#[ignore]` the createElement test. It
should pass.

### Phase 3 — addEventListener that actually fires

**Why third:** Google's homepage installs a `submit` interceptor
via `document.documentElement.addEventListener("submit", …)`.
Without firing it, the form's native submit goes through and
the user lands at the dead `/search` endpoint. With firing,
Google's JS can call `preventDefault()` and XHR for results.

**Scope:**
- Extend `EventListenerMap` in `crates/bui-js/src/events.rs` so
  listener payloads can be Zinc `Value`s (Native callbacks
  today only). Hold the Engine + a `Vec<Value>` of registered
  callbacks per (NodeId, event_type) tuple.
- `__addEventListener(target, type, listener)` host fn that
  records the JS callable into the map. Replace the no-op
  stub in `dom_bindings.rs`.
- When a user input arrives (chrome click, key) and the page
  has a listener registered for that event type on the hit
  node (or any ancestor for bubble phase), dispatch:
  build an `Event` object with `target`, `preventDefault`,
  `stopPropagation`, then `engine.call(callback, [event])`.
- Honour the `preventDefault()` flag — if set, swallow the
  default chrome behavior (form submit, link follow, etc.).

**Risks:** the existing `Event` type carries only generic
`HashMap<String, String>` data. Google's submit handler reads
`event.target.elements.q.value`. We'll need to surface a real
`elements` collection or short-circuit to the form's input
buffer. Probably one or two iterations of "Google still throws
on field X" before it's stable.

**Verification:** the dev-dock Console shows Google's submit
handler being entered (add a temporary `console.log` to a test
page). With the listener firing, `preventDefault()` should
suppress our native navigate.

### Phase 4 — fetch / XMLHttpRequest backed by bui-net

**Why fourth:** Google's submit handler calls
`fetch('/complete/search?…')` or `new XMLHttpRequest()` to load
results without a full navigation. Without fetch, the handler
hangs or falls through to its `location.href = google.gbvu`
fallback (which is what triggers the consent flow we hit
originally).

**Scope:**
- Promise minimum: Zinc has microtasks (`drain_microtasks`) but
  no public Promise API. Either ship a minimal Promise impl in
  the prelude (`then`/`catch`/`finally` chained closures) or
  pin Zinc to a release that ships a real one. The former is
  faster; the latter is correct long-term.
- `fetch(url, opts)` host fn: spawns
  `shared_runtime().spawn(shared_client().get(url))`, returns a
  Promise-shaped object whose `then` runs once the future
  resolves. Body is JSON-parsed on demand via `.json()`.
- `XMLHttpRequest` constructor returns a real wrapper that
  drives the same plumbing. Honour `addEventListener('load',
  …)` and `responseText`.
- Plumb captured fetches into `tab.net_log` so the dev-dock XHR
  tab shows them.

**Risks:** async re-entry into JS. We need the engine to be
quiescent (no script frame on the stack) when a fetch resolves
and we call the user's callback. Easiest: drain a per-tab
"resolved fetch" queue between paint frames, same shape as
the existing navigation side channel.

**Verification:** a tiny inline `<script>fetch('/api/x').then(r
=> console.log(r.status))</script>` page captures the status in
the dev-dock Console.

### Phase 5 — setTimeout / requestAnimationFrame / microtask queue

**Why fifth:** Google's code uses `setTimeout(…, 0)` as a
microtask shim and `requestAnimationFrame` for paint
scheduling. Without these the submit handler waits forever.

**Scope:**
- Per-tab `Vec<(when: Instant, callback: Value)>` timer queue.
- `setTimeout(fn, ms)` host fn pushes onto it, returns a fake
  id. `clearTimeout(id)` removes by id.
- After each paint frame, the host walks the queue and fires
  any callbacks whose `when` has passed.
- `requestAnimationFrame` fires once per paint frame
  unconditionally (close enough).

**Verification:** a `setTimeout(() => console.log('tick'), 50)`
test sees the log line within ~one frame.

### Phase 6 — Drop the DDG redirect; ship real Google (form submit live)

**Shipped:**
- `maybe_rewrite_google_search` is a no-op pass-through;
  `seed_google_consent` still seeds the CONSENT/SOCS cookies so
  the consent gate never appears.
- After the inline-script pass, the orchestrator fires one
  synthetic `load` event on the document root. JS-side
  `addEventListener('load', …)` handlers run inside the same
  VM and any navigation they request via `location.href = …`
  lands in the same `pending_nav` slot the host already drains.
- `TabState` owns a `bui_js::JsContext` for the lifetime of
  the page. Submit paths (Enter in a focused input + click on
  a submit-style control) dispatch a `submit` event through
  the persistent engine before any default action: handlers
  observe `event.target` as a real wrapped element, can call
  `event.preventDefault()` to suppress the chrome's native
  form submit, and can do `location.href = …` to redirect via
  the existing pending-nav drain.
- `tools/google_smoke.sh` asserts the homepage form is
  structurally reachable.

**Deferred follow-up:**
- Route `click` and `keydown` user input through the same
  `JsContext::dispatch` so non-submit handlers (autocomplete
  suggestions, anchor interceptors, modal close-on-Escape)
  light up. The plumbing is built; the remaining work is
  picking event targets from the hit-test result and the
  focused input.
- Once enough Google JS runs end-to-end to land on a real
  results page, tighten `tools/google_smoke.sh` to assert a
  `<h3>` result heading.
- Add a section to `docs/architecture.md` / `docs/dev-dock.md`
  describing the new JS surface.

## Critical files

| File | What changes |
|---|---|
| `~/Desktop/browser/src/vm/vm.rs:663` | Bounds check in `close_upvalues_from` |
| `~/Desktop/browser/src/vm/vm.rs` (new) | `pub fn alloc_host_object(&mut self, tag, payload)` |
| `crates/bui-js/src/lib.rs` | Drop `catch_unwind`, wire async fetch callback drain |
| `crates/bui-js/src/dom_bindings.rs` | Real `__addEventListener`, real `fetch`/`XHR`/`setTimeout` host fns; drop the prelude no-ops |
| `crates/bui-js/src/events.rs` | Extend `EventListenerMap` to hold JS `Value` callbacks |
| `crates/bui/src/main.rs` | Drain timer + fetch-resolve queues between paint frames; remove `maybe_rewrite_google_search` |
| `tools/google_smoke.sh` | Tighten to assert real `<h3>` results |

## Verification (end-to-end)

```bash
cargo test --workspace
cargo build --release --bin copper

# Manual:
./target/release/copper render https://www.google.com/
# - homepage renders cleanly, no JS exceptions in dev-dock Console
# - type a query in the textarea, press Enter
# - the JS submit handler runs, intercepts, fetches /search via XHR
# - results page paints with a list of <h3> result headings
# - clicking a result follows to the destination

tools/google_smoke.sh
tools/wiki_header_snapshot.sh
```

## Risks / what could shift the timeline

- **Zinc complexity**: the upvalue patch may surface deeper
  bugs (the VM has known rough edges around scope analysis).
  Budget half the phase for "fix one, hit another".
- **Google's CSP / nonces**: every inline script has a nonce.
  We don't enforce CSP today; if Google starts requiring it for
  the submit handler to run, we'd need a CSP-aware bypass on
  our side (we control the engine, so we can ignore nonces —
  but document this).
- **Captcha**: heavy automated traffic from a hand-rolled
  browser will get reCAPTCHA'd. The smoke test should run
  manually, not in CI, to avoid burning the IP.

## Out of scope (explicit)

- Streaming response handling (`ReadableStream`).
- Service Workers.
- WebSockets, WebRTC, WebGL.
- IndexedDB / FileSystem APIs.
- Module scripts (`<script type=module>`). Inline only.

These can become individual milestones if a target site needs
them.

## Commit policy

Each phase ships as its own commit (or small commit series).
**No `Co-Authored-By: Claude …` trailers on any commit in this
milestone** — same policy as the rest of the project.
