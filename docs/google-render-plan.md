# Real Google search rendering: the long road

## Why a separate plan

`js-engine-plan.md` covered "make inline `<script>` tags do
useful work" — Phases 1–6 wired up event listeners, fetch,
setTimeout, location, and user-input dispatch. Form submit
now lands on the real Google `/search` URL with full query
parameters. The Wikipedia / GitHub / HN / news-site class of
page renders cleanly.

What it does **not** cover is rendering Google's modern
search results page. That's a different beast: Google ships
its UI as a Closure-compiler bundle that depends on roughly
the whole web platform. Every other site that uses Closure
or a similar bundler (Maps, Mail, Docs, Drive, YouTube web)
hits the same wall. This plan is the honest scope for
getting there.

Treat the timeline below as a research budget, not a
deadline. Each phase has high variance because the bundle
is highly inter-dependent — one missing API on the critical
path can block the next ten.

## Where `/search` differs from `/` (homepage)

After several rounds of Phase 19 work, the homepage
`google.com` renders real content (top bar, logo SVG, footer
links). The search-results page `google.com/search?q=…` still
renders empty. The difference is structural:

| Aspect | `/` (homepage) | `/search?q=…` |
|---|---|---|
| Response size | ~150 KB | ~90 KB |
| `<script src=…>` tags | 1 (the xjs bundle) | 0 |
| `<form>` / `<input>` in HTML | Yes | No |
| Visible `<body>` content | Logo + footer in static HTML | None — body is empty |
| Inline scripts | ~5 small + xjs handler | 5, the big one is a 62 KB self-decoding bundle |
| Render strategy | Static HTML, JS for enhancement | 100 % JS-injected from a self-eval'd bundle |

The 62 KB inline script #2 on `/search` is a **self-decoding
bundle**. It defines a state-machine dispatcher

```js
z = function(c, L, Z, T, F, H, S, W, C, n) {
    for (C=64; C!=60;)
        if (C==64) C = c;
        else if (C==65) { /* TrustedTypes branch */ }
        else if (C==37) return n;
        else { /* numbered-state transitions */ }
};
```

…walks through a numbered-state interpreter loop, assembles
the bundle's real source by joining an array of fragments,
then passes the result through `(0, eval)(...)`.

The eval'd code references unbound minified names (`J`, `K`,
…) that LOOK like the dispatcher's responsibility but are
actually function parameters in the eval'd source. The
ReferenceError was a *symptom*: a Zinc lexer bug
mis-tokenised `.replace(/=/g, "")` (the regex-start `/=`
got eaten as `SlashAssign`), cascading into syntax errors
that broke later function declarations, which surfaced as
"undefined J" at call sites.

**Fixed (commit `b4f6fa5` in the zinc repo):** the lexer
now checks regex-start context BEFORE consuming `/=`. With
that fix, the bundle progresses further — the new failure
is `Cannot read properties of undefined (reading 'call')`
on a different code path, still inside the eval'd bundle
but now several thousand lines deeper.

### Investigation status (latest session)

**Pinned and worked around:** the `sctm is not defined`
error was a Zinc closure-tracking bug. Patched script #3's
`function V(a)` to log `typeof sctm` and confirmed it's
`undefined` (binding genuinely lost). Minimal closure
repros work fine — the bug needs the full 26 KB script's
specific declaration sequence. Workaround in the prelude:
declare `var sctm = false; var sclm = false;` as globals so
the unbound read resolves to the same value the script
intended. /search error count dropped from 2 to 1.

**Diagnosed with line info, still in eval'd code:** the
remaining error now reports `(at line 0, pc 13)` thanks to
Zinc's new source-location annotations on TypeError messages
(commit `688bd4e` in the Zinc repo). Line 0 means the error
is in a dynamically-eval'd chunk where source-line mapping
isn't populated; pc 13 means very near the start of that
chunk's bytecode.

Manual instrumentation: inserted `console.log('M1')`,
`console.log('M2 VL')` markers into the eval'd source.
Output:
```
M1
M2 VL
```
M1 = inside the IIFE start. M2 = after `var VL=function`
definition started. M3 (after `var x=this||self`) never
fires. So the failure is somewhere in the long
`var VL = function(){…}, fu = function(){…}, …` chain —
151 function declarations bound at module init.

The bundle wraps everything in one IIFE that runs all 151
function definitions PLUS a long init sequence (`((CN=VL(…
` etc.). The init sequence calls `VL(…)` immediately. VL's
body references `Oc[L]` — but `Oc` is defined later in the
same chain. So either:
1. The chain's expression-order evaluation hits VL's call
   before Oc is initialised — and Oc resolves to
   `undefined` — and `Oc[L]` is `undefined[L]` →
   ReferenceError-style throw.
2. Or some assignment in the chain produces an unexpected
   value.

The next investigation needs a different toolset than
manual command-line bisection of a 60+ KB minified
bundle — either:
- Add per-instruction tracing in Zinc (dump every OpCall
  with its receiver) and run; the trace pinpoints the call.
- Or step the bundle with a JS debugger and capture the
  state at the moment of error.

Both are session-sized in the right environment but
unwieldy from cold-start CLI grep + bisect.

Wrapped
the eval'd bundle's body in try/catch and caught the
message: `constructor,hasOwnProperty,isPrototypeOf,
propertyIsEnumerable,toLocaleString,toString,valueOf is
not a function`. The receiver is an array of
Object.prototype's property names. Somewhere the bundle
gets such an array where it expected a function. The
bundle doesn't call `Object.getOwnPropertyNames` literally
— some other path produces the list. Likely candidates: a
`for…in` loop over `Object.prototype` (Zinc may iterate
non-enumerable names differently), or `Object.entries` /
`Object.keys` returning unexpected results in some edge
case. Pinning requires either binary-patching the bundle
at each call site or extending Zinc to capture the stack at
TypeError construction.

### Investigation status

The two remaining errors on `/search` were each investigated:

**`'call' of undefined` in script #2:** Grepped every
`.call(` site in the eval'd source — 20+ distinct receivers
(`x6`, `h`, `fu`, `forEach`, `ZO`, `WA`, `splice`,
`preventDefault`, …) each appearing multiple times. Identifying
which receiver is `undefined` at runtime requires per-line
instrumentation (wrapping `Function.prototype.call` to log
when called on undefined, or splitting the eval'd source at
each call site and bisecting). Out of scope for one session.

**`sctm is not defined` in script #3:** Standalone, the
script throws `Error("a")` (Math-identity check at line 1)
because `window` isn't set up. With our prelude (which sets
`window.Math = Math`), the Math check passes and execution
proceeds to a callsite that reads `sctm` from closure scope.

Minimal repros of the closure pattern (`(function(){ var
sctm = false; function V(a) { return sctm; }; V() })()`)
work fine in Zinc standalone. The full 26 KB script triggers
the bug; the repro doesn't. Something about size, function
hoisting order, or some token combination loses the
binding. No `eval` / `new Function` / `with` is present that
would explain dynamic scope. A/B bisecting the script while
preserving syntactic validity is the next investigative step.

### Where the remaining `/search` errors land

After the lexer fix, two errors remain on `/search`:

1. **Script #2 (62 KB self-decoder)**: `Cannot read
   properties of undefined (reading 'call')` —
   the eval'd bundle runs much further but eventually
   tries `something.call(...)` on undefined. That's
   typical of a missing-API path: real browsers would
   resolve `something` to a built-in we don't expose.
   Bisecting WHERE inside the bundle requires either
   per-line instrumentation or a try/catch wrap.

2. **Script #3 (26 KB)**: `ReferenceError: sctm is not
   defined`. The script declares `var sctm = false` at
   the top of its IIFE and reads it inside three nested
   functions later. Standalone (no prelude) the script
   throws `Error: a` (a different, explicit throw)
   instead — so something our prelude installs breaks
   the closure scope for `sctm`. Most likely a `var`
   redeclaration / hoisting interaction with the
   prelude's many globals. Investigation: A/B which
   prelude block triggers the regression.

Neither of these is a structural wall the way the regex
bug was. Both are tractable trace-and-fix problems. The
"page renders empty" state on `/search` continues until
both clear (the bundle needs to actually paint DOM, which
the inline scripts only do at the END of their flow).

Tried (didn't work): pre-declaring `var J, K, M, …`
globally as `undefined` in the prelude. The dispatcher
writes through to those names via a pattern that conflicts
with the already-declared `var` — crashed `/search`
entirely and added new errors on `/`.

To make `/search` render specifically:

1. **Reverse-engineer** the state-machine dispatcher enough
   to know what API surface it expects in the global scope
   for the walk to complete. The dispatcher uses `x =
   this || self` and reads `x.trustedTypes` plus a few other
   globals; stubbing those right *might* unlock the rest.
2. **Vendor closure-library** so Google's bundle never has
   to fall back to the self-decode path — Phase 19 proper.

Both are real, neither is session-sized. The chase-
individual-error loop that worked for the homepage doesn't
help here because the actual error happens in
dynamically-generated code that doesn't exist on disk for us
to read or patch.

## Today's reality (May 2026)

Inline scripts run. DOM mutations work. Submit events fire
through registered JS listeners. None of that is enough.
A fresh `https://www.google.com/` load on this branch logs
~7 distinct JS errors before its bootstrap gives up:

```
TypeError: Cannot read properties of undefined (reading 'load')
Error: b
ReferenceError: _s is not defined
TypeError: Cannot read properties of undefined (reading 'parse')
Error: a
TypeError: Cannot read properties of undefined (reading 'length')
TypeError: Cannot read properties of undefined (reading 'length')
```

Every one of those traces back to the same shape. Google's
inline scripts are Closure-compiler output:

```js
this.gbar_=this.gbar_||{};(function(_){var window=this;
    var fe=function(a){this.J=_.x(a)};
    _.B(fe,_.S);
    var ge=function(){_.y.call(this);this.j=[];this.i=[]};
    _.B(ge,_.y);
    // …thousands of lines of `_.X` calls…
    _.G("gapi.load",(0,_.E)(this.o,this));
}).call(this);
```

The IIFE expects a `_` argument — Google's **Closure-library
runtime** — populated by a bootstrap loader that lives in a
separate `<script src=…>` we don't fetch and execute. Without
`_`, every `_.B`/`_.S`/`_.G`/`_.y` access throws on undefined,
the script aborts mid-evaluation, and the page leaves the
DOM in a partial state.

`gbv=1` (the legacy basic-HTML endpoint) is dead — it serves
the same `<noscript><meta refresh>` "enable JS" stub as the
modern URL. `m.google.com` redirects to about.google. There
is no non-JS Google path left. The only paths to a real
results page are:

1. Run Google's Closure-library bundle ourselves.
2. Route Google search through a different upstream (DDG,
   Brave, Startpage) — a separate, smaller piece of work
   that lives in `bui/main.rs` not in `bui-js`.

This plan covers option 1.

## Goal

A user types a query, presses Enter, lands on
`https://www.google.com/search?q=…`, and sees the real
modern Google results page render — `<h3>` headings,
clickable result links, the "People also ask" widget, the
sidebar — all painted by Copper. Optional: autocomplete
suggestions on input.

Done = `tools/google_smoke.sh` asserts at least one `<h3>`
result heading appears after a form submit, and the user
can manually click a result and follow.

## Non-goals

- Maps, Mail, Docs, Drive, YouTube. They use the same
  Closure foundation but each adds its own surface
  (Canvas, WebGL, Service Workers). One target at a time.
- Lighthouse-quality performance.
- Cross-tab state synchronization (broadcast channels,
  shared workers).
- Accessibility tree fidelity beyond what naturally falls
  out of the DOM.

## Effort summary

| Phase | Scope | Rough effort | Blocker for |
|---|---|---|---|
| 7 | Zinc-side: real Promise + microtask queue | 2 weeks | every async path |
| 8 | Zinc-side: Symbol + Proxy + Reflect | 3 weeks | Closure-library boot |
| 9 | bui-js: async fetch returning real Promise<Response> | 1 week | every XHR-replacement path |
| 10 | bui-js: real XMLHttpRequest with addEventListener | 1 week | Google's pre-fetch loader |
| 11 | bui-js: timer queue (setTimeout/Interval, requestAnimationFrame, requestIdleCallback) | 1 week | Google's microtask shims |
| 12 | bui-js: MutationObserver | 2 weeks | Closure render plumbing |
| 13 | bui-js: IntersectionObserver + ResizeObserver | 2 weeks | lazy-image / sticky-nav |
| 14 | bui-js: DOM completeness pass | 3 weeks | form.elements, dataset, computedStyle, getBoundingClientRect, Range/Selection, Node tree |
| 15 | bui-js: Event interface hierarchy (MouseEvent, KeyboardEvent, InputEvent, SubmitEvent, FocusEvent, CustomEvent) | 1 week | every input handler |
| 16 | bui-js: storage (localStorage, sessionStorage, JS-readable cookies) | 1 week | persistence; not always blocking |
| 17 | bui-js: history.pushState + popstate + URL/URLSearchParams | 1 week | SPA navigation |
| 18 | host: orchestrator integration — async event loop, microtask drain, re-layout between dispatches | 3 weeks | every async DOM mutation lands here |
| 19 | Closure-library shim — load the upstream library or shim the subset Google's bundle inlines | 4–8 weeks | THE blocker for Google |
| 20 | Google specifics — XHR `/search?async=…` plumbing, gbar UI, autocomplete | 2 weeks | the actual goal |

**Floor: ~6 months. Realistic: 9–12 months at full-time pace.**
Add 30 % for "one phase surfaces a Zinc bug that blocks the
next two" — this has happened on every previous milestone.

## Phases in dependency order

### Phase 7 — Real `Promise` + microtask queue (Zinc-side)

The single biggest unlock. Closure-library and every modern
bundle assume native Promise with:

- `Promise.resolve(v)`, `Promise.reject(e)`, `Promise.all`,
  `Promise.race`, `Promise.allSettled`, `Promise.any`
- `.then` returns a new Promise; chains of arbitrary depth
- `.catch`, `.finally`
- `async` / `await` syntax (Zinc has it half-wired — confirm
  full spec compliance)
- Microtask scheduling: a `.then` callback runs after the
  current synchronous task completes, before the next
  macrotask

**Scope (Zinc, `~/Desktop/browser`):**
- `src/runtime/promise.rs` with `PromiseState::{Pending,
  Fulfilled, Rejected}`, fulfillment + rejection reaction
  queues, the abstract operations `PromiseResolveThenable`,
  `EnqueueJob`, etc. The ECMA-262 spec for §27.2 is the
  source of truth.
- A `Vm::microtask_queue: VecDeque<Microtask>` that the
  embedder drains via `Vm::drain_microtasks()`.
- `async function` desugaring already exists; verify it
  composes with the real Promise.

**Bui side:** `JsContext::dispatch` must drain microtasks
after each event dispatch. The host's frame loop must drain
microtasks before paint.

**Verification:** a tight regression test
(`bui-js/tests/promise_spec.rs`) ports the relevant Test262
sub-suite — `built-ins/Promise/**`. Pass rate >95 %.

### Phase 8 — `Symbol`, `Proxy`, `Reflect`

Closure-library uses `Symbol.iterator` for collections,
`Reflect` for property descriptors, and a `Proxy` in a
couple of internal places. Without these, half the
`for…of` and `Object.assign` patterns in modern code throw.

**Scope (Zinc):**
- `Symbol` as a real primitive type, registry, well-known
  symbols (`Symbol.iterator`, `Symbol.asyncIterator`,
  `Symbol.toPrimitive`, `Symbol.toStringTag`,
  `Symbol.hasInstance`).
- `Reflect` namespace with the full set: `get`, `set`,
  `has`, `deleteProperty`, `ownKeys`, `getPrototypeOf`,
  `setPrototypeOf`, `defineProperty`, `getOwnPropertyDescriptor`,
  `isExtensible`, `preventExtensions`, `construct`, `apply`.
- `Proxy` with all 13 traps. The interpreter loop has to
  consult traps on every property operation — pervasive
  change.

**Verification:** Test262 `built-ins/Symbol/**`,
`built-ins/Reflect/**`, `built-ins/Proxy/**`. ≥90 %.

### Phase 9 — Async `fetch(url, opts)` returning `Promise<Response>`

The current synchronous shim is enough for "fetch then
parse" patterns but breaks anything with real async ordering
(`Promise.all([fetch(a), fetch(b)])`, abort signals, response
streaming).

**Scope (bui-js):**
- `fetch` host fn returns a `Promise<Response>` (built via
  Phase 7 API).
- A worker thread pool driving real network requests via
  `shared_runtime().spawn(shared_client().get(...))` —
  resolution posts a microtask back onto the engine's
  queue.
- `Response` with `.text()`, `.json()`, `.arrayBuffer()`,
  `.blob()`, `.headers.get/has/forEach`, `.ok`, `.status`,
  `.statusText`, `.url`, `.redirected`.
- `Request` constructor accepting URL or Request object.
- `AbortController` / `AbortSignal` (Google uses
  `fetch(url, {signal})` for cancellation).
- `Headers` constructor.

**Risk:** thread-safety of the Engine. Today `Engine` isn't
`Send` because its JIT cache holds raw pointers. Resolving
a fetch on a worker means handing a callback to a
non-`Send` value. Solution: post a *message* into a queue
on the engine's owning thread, drained on the main loop's
tick.

**Verification:** a roundtrip test that issues
`Promise.all([fetch('/a'), fetch('/b')])` and asserts both
complete with the right order.

### Phase 10 — Real `XMLHttpRequest`

Google's loader still uses XHR (not fetch) for some paths,
especially the `/gen_204` telemetry ping and the search
results XHR (`/search?async=…&pq=…`). The `addEventListener('load')`
+ `responseText` shape is required.

**Scope (bui-js):**
- `XMLHttpRequest` constructor returning an object with
  `open`, `send`, `setRequestHeader`, `abort`,
  `readyState`, `status`, `statusText`, `responseText`,
  `responseType`, `response`, `responseURL`, `getResponseHeader`,
  `getAllResponseHeaders`, `upload`, `withCredentials`,
  `timeout`.
- Event firing: `readystatechange`, `progress`, `load`,
  `error`, `abort`, `timeout`, `loadend`. All routed
  through the same `EventListenerMap` shape we already
  have for DOM events.

**Verification:** a synthetic test that issues an XHR
to a local HTTP server (the same one Phase 9 sets up),
verifies progress + load events fire in order.

### Phase 11 — Timer queue

Today `setTimeout(fn, 0)` fires synchronously and any
non-zero delay is dropped. Google relies on real timer
semantics — `setTimeout(fn, 100)` to schedule a retry,
`requestAnimationFrame(fn)` for paint hooks,
`requestIdleCallback(fn)` for low-priority deferred work.

**Scope (bui-js + host):**
- Per-`JsContext` `Vec<(when: Instant, id: u32, callback:
  Value, repeat: Option<Duration>)>` timer queue.
- `setTimeout(fn, ms)`, `setInterval(fn, ms)`, `clearTimeout`,
  `clearInterval` host fns.
- `requestAnimationFrame(fn)` ties into the paint loop —
  fires before each frame's render.
- `requestIdleCallback(fn, opts)` runs after paint when
  the main thread has slack.
- The host's main loop walks the queue every frame and
  fires due callbacks before painting.

**Verification:** test that schedules 100 setTimeouts of
varying delays and asserts they fire in correct order
with correct gaps (±1 frame).

### Phase 12 — `MutationObserver`

Closure-library's render pipeline observes DOM mutations
to schedule re-renders. Without MutationObserver, every
`appendChild` triggered by JS leaves Closure thinking the
DOM is in an older state than it is.

**Scope (bui-js):**
- `MutationObserver` constructor.
- `observe(target, options)` records (NodeId, options) in
  a per-context list.
- Every mutation in `dom_bindings` checks the observer
  list and, if matched, queues a `MutationRecord` onto a
  per-observer batch.
- After each microtask drain, the observers' batches are
  delivered as `Array<MutationRecord>` to their callbacks.
- `disconnect`, `takeRecords`.

**Verification:** test that observes a subtree, mutates
it from JS, and asserts the callback fires with the
right records.

### Phase 13 — `IntersectionObserver` + `ResizeObserver`

Closure uses IntersectionObserver for lazy image loading
and to hide / show sticky nav. ResizeObserver for layout-
sensitive widgets.

**Scope (bui-js + host):**
- Both observer types with the standard surface
  (`observe`, `unobserve`, `disconnect`, callback shape).
- After every layout pass, walk the observer registries
  and queue callbacks for newly-intersecting / newly-
  resized targets.

**Verification:** synthetic tests that scroll a tall
document past sentinel divs and assert callbacks fire.

### Phase 14 — DOM completeness pass

The long tail. Google's bundle uses every corner of the
DOM.

**Critical:**
- `form.elements` (`HTMLFormControlsCollection`) — Google's
  submit interceptor reads `form.elements.q.value`
- `Element.dataset` (proxy-like access to `data-*` attrs)
- `Element.style` get/set (`CSSStyleDeclaration`)
- `window.getComputedStyle(el).getPropertyValue(...)`
- `Element.getBoundingClientRect`, `Element.offsetParent`,
  `offsetTop`, `offsetLeft`, `offsetWidth`, `offsetHeight`,
  `clientWidth`, `clientHeight`, `scrollWidth`, `scrollHeight`,
  `scrollTop`, `scrollLeft`
- `Element.scroll(...)`, `scrollTo`, `scrollBy`, `scrollIntoView`
- Full `Node` tree: `childNodes`, `firstChild`, `lastChild`,
  `nextSibling`, `previousSibling`, `nodeType`, `nodeName`,
  `nodeValue`
- `DocumentFragment` + `<template>` content
- `Range` and `Selection` (we have partial selection in
  paint, need the JS-side API)
- `MouseEvent`, `KeyboardEvent`, `InputEvent`, `SubmitEvent`,
  `FocusEvent`, `WheelEvent` as real subclasses with the
  right inheritance chain (each phase 15 below)

**Non-critical but Google touches them:** `Element.attributes`
as a NamedNodeMap, `Element.closest`, `Element.matches` (we
have), `Element.insertAdjacentHTML`, `Element.outerHTML`,
`Element.innerHTML` setter.

### Phase 15 — Event interface hierarchy

Synthetic events today carry `type`/`target`/`bubbles`/
`cancelable`/`defaultPrevented`/`preventDefault`/`stopPropagation`.
Real handlers need a lot more: `MouseEvent.clientX/Y`,
`KeyboardEvent.key`/`code`/`shiftKey`/`metaKey`,
`InputEvent.data`/`inputType`, `SubmitEvent.submitter`.

**Scope (bui-js):** subclasses of `Event` with the right
prototype chain. `new MouseEvent(type, init)` and friends.
Dispatch uses the right subclass based on event kind.

### Phase 16 — Storage

`localStorage`, `sessionStorage` for site preferences
(theme, language, search settings); `document.cookie`
getter/setter routing JS reads/writes to the bui-net
cookie jar.

**Scope (bui-js):** Storage interface, per-origin
partitioning, persisted to `~/.cache/copper/storage/`
or similar.

### Phase 17 — `history.pushState` + popstate + URL/URLSearchParams

Google's SPA navigation: `history.pushState({}, '',
'/search?q=…')` swaps the URL bar without a real
navigation, then the bundle XHRs results and renders.

**Scope (bui-js + host):**
- `history.pushState(state, title, url)` updates
  `window.location.*` getters AND the chrome's URL
  pill, without triggering a real fetch.
- `history.replaceState`, `history.state`, `history.go`,
  `history.back`, `history.forward` (last three need a
  real navigation — wire through `TabState::navigate_to`
  / `go_back` / `go_forward` with state stash).
- `popstate` event fires on `history.back`/`forward`.
- `URL` / `URLSearchParams` constructors with full surface.

### Phase 18 — Host orchestrator: async event loop

Up to this point each phase is an independent surface.
This phase ties them together: the main render loop has
to drive JS forward continuously, not just at navigation
time.

**Scope (bui/src/main.rs + bui-js):**
- A per-tab `tick(&mut self)` method called every frame
  before paint, in this order:
  1. Drain expired timers (Phase 11)
  2. Drain pending fetch / XHR resolutions (Phase 9 / 10)
  3. Drain microtask queue (Phase 7)
  4. Run dirty observers — Mutation, Intersection, Resize
     (Phase 12 / 13) → may queue more microtasks → goto 3
  5. If `dirty` was tripped: re-style + re-layout (already
     wired)
  6. Fire `requestAnimationFrame` callbacks (Phase 11)
  7. Paint
- An async cancellation token per tab so navigations
  abort in-flight fetches and timers.

**Verification:** a test page that sets up a 10ms
setTimeout that mutates the DOM; the next paint shows
the mutation, not the stale state.

### Phase 19 — Closure-library bundle

The unknown. Google's inline scripts assume the namespace
exposed by `goog.global._` — the Closure-library runtime
that ships as `base.js` + the rest of the library tree.

**Two paths, both real work:**

**A. Run upstream closure-library:** clone
`https://github.com/google/closure-library`, bundle the
subset Google uses, prepend it to every Google page before
the inline scripts. Pros: real Google code, no
divergence. Cons: another 1MB of JS to actually run; uses
APIs we may still not have (`goog.events`,
`goog.async.run`, `goog.userAgent.product`, `goog.json`,
`goog.dom.classlist`, `goog.crypt.base64`, …). Each one
of those will hit a gap we have to patch.

**B. Shim the subset Google's bundle inlines:** Google's
production bundle is post-Closure-compiler, so the
namespace symbols (`_.B`, `_.S`, `_.G`, …) are minified
and reordered every release. Reverse-engineering the shim
is whack-a-mole that breaks every time Google rebuilds.

Recommend **A**. Even if 30 % of closure-library doesn't
run cleanly, it gets us further than B.

**Scope:**
- A `bui-closure` crate that bundles a vendored copy of
  the relevant closure-library modules.
- Inject the bundle as a hidden first `<script>` on any
  page whose host matches Google.
- Iterate on engine + bui-js gaps until closure-library
  initializes cleanly.

**Verification:** loading `https://www.google.com/`
produces ≤2 unique JS errors in the dev-dock Console.
gbar (Google's top bar) becomes visible.

### Phase 20 — Google-specific render

Once Closure boots, Google's homepage finishes its
bootstrap and the form submit triggers the modern
results XHR (`/search?async=…`). The XHR returns JSON
that Google's render pipeline parses + injects into the
DOM. We need every API in that pipeline to work.

**Scope:**
- Verify the result-XHR endpoint accepts our cookies +
  UA. If not, identify the missing CSRF / token / signed
  param and source it from the homepage state.
- Trace any remaining gaps in DOM / Event / Observer
  surface that fire during result render. Patch them.
- Optional: autocomplete suggestions — issues a
  `/complete/search?…` JSONP request on every keystroke.
  Probably its own sub-phase.

**Verification:** `tools/google_smoke.sh` asserts a
`<h3>` heading appears in the layout dump after a
synthetic form submit on
`https://www.google.com/?q=rust+browser`. The dev-dock
XHR tab shows the `/search?async=…` request.

## Critical files (will exist when this is done)

| File | What changes |
|---|---|
| `~/Desktop/browser/src/runtime/promise.rs` (new) | Phase 7 |
| `~/Desktop/browser/src/runtime/symbol.rs` (new) | Phase 8 |
| `~/Desktop/browser/src/runtime/proxy.rs` (new) | Phase 8 |
| `~/Desktop/browser/src/runtime/reflect.rs` (new) | Phase 8 |
| `crates/bui-js/src/fetch.rs` (new) | Phase 9 |
| `crates/bui-js/src/xhr.rs` (new) | Phase 10 |
| `crates/bui-js/src/timers.rs` (new) | Phase 11 |
| `crates/bui-js/src/observers.rs` (new) | Phase 12 / 13 |
| `crates/bui-js/src/dom_bindings.rs` | every phase 14 / 15 addition |
| `crates/bui-js/src/storage.rs` (new) | Phase 16 |
| `crates/bui-js/src/history.rs` (new) | Phase 17 |
| `crates/bui/src/main.rs` (`TabState::tick`) | Phase 18 |
| `crates/bui-closure/` (new crate) | Phase 19 |
| `tools/google_smoke.sh` | tightened in Phase 20 |

## Verification per phase

Every phase ships at least one test:
- Phase 7–10: Test262 sub-suite or hand-rolled equivalent
- Phase 11–17: a synthetic integration test per API
- Phase 18: a multi-async ordering test
- Phase 19: closure-library initializes cleanly against
  a fixture page
- Phase 20: the smoke test asserts an `<h3>` heading

## Risks / what could shift the timeline

- **Zinc complexity at Phase 7 / 8.** Proxy in particular
  is invasive — every property access in the interpreter
  has to consult traps. Could surface latent VM bugs
  that block follow-on phases.
- **closure-library version drift.** Google ships a
  minified bundle that doesn't exactly match upstream
  closure-library — they fork. We may need to vendor a
  specific commit and accept divergence.
- **Captcha / bot detection.** Heavy automated traffic
  from a hand-rolled UA will eventually get reCAPTCHA'd.
  The smoke test should run manually, not in CI.
- **API surface explosion.** Google touches at least 200
  Web APIs. The "DOM completeness pass" (Phase 14)
  estimate of 3 weeks assumes we can cut scope to what
  Google's homepage + results page actually uses. If
  Google adds a new API dependency mid-rebuild we lose
  a week per occurrence.
- **One-off Zinc panic resurfaces.** Phase 1 of the
  earlier milestone fixed the upvalue-closing bug; we
  have low confidence there aren't similar latent bugs
  in the JIT that surface only on bundles this large.

## What this plan does NOT do

- Maps, Mail, Docs, Drive, YouTube. Each is its own
  milestone of similar size.
- Service Workers, Web Workers, WebSockets, WebRTC,
  WebGL, WASM, Canvas 2D, MediaSource Extensions, IndexedDB.
- A real CSP enforcement layer (we ignore nonces today).
- Performance tuning. Goal is "renders correctly", not
  "renders fast".

## Commit policy

Each phase ships as its own commit (or small commit
series). **No `Co-Authored-By: Claude …` trailers on any
commit in this milestone** — same policy as the rest of
the project. Author stays `Florian Stein <info@florek.io>`.

## Cheaper alternative

If a multi-month rebuild isn't worth it for "just Google
search", the pragmatic shortcut is to rewrite
`google.<tld>/search?q=…` requests at the
`maybe_rewrite_google_search` layer to a different
upstream that ships HTML results today — DuckDuckGo's
`/html` endpoint, Brave Search, or Startpage. That ships
in a session, not a year, and the user gets real search
results in the URL bar — they just aren't Google's.

The two options are not mutually exclusive: a fallback
unblocks the user today, the long road delivers Google
specifically later.
