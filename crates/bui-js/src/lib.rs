//! bui-js — bridge between the DOM (`bui-dom`) and the Zinc JS engine.
//!
//! Zinc 0.5 unblocked the embedder API: `Engine::register_host_fn`,
//! `register_host_class` + `alloc_host_object` + `host_payload`, and
//! persistent VM state across `eval` calls. This module rides directly on
//! that surface — every `Engine` we hand out lives for the lifetime of a
//! `Document`, so two `<script>` tags share globals, and DOM nodes ride
//! into JS as `ObjectKind::Host` values that carry a `payload: u64` index
//! into a side table the binding layer owns.
//!
//! Currently in this module:
//!   * `execute_inline_scripts(doc)` — walks the DOM for `<script>` and
//!     evaluates each in document order against a single `Engine`.
//!   * `events::EventListenerMap` — capture/target/bubble dispatch (Rust
//!     side; JS-callback adapter lands when bindings install).
//!
//! Next:
//!   * `BindingContext` — owns the side table (`Vec<NodeId>`) and installs
//!     `window` / `document` / `Element` host functions.
//!   * `dirty` flag tripped by mutating bindings; orchestrator re-runs
//!     style + layout when set.

pub mod closure_shim;
pub mod dom_bindings;
pub mod events;

pub use dom_bindings::{BindingContext, FetchResponse, Fetcher};
pub use events::Event;

use zinc::runtime::value::Value;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use bui_dom::{Document, NodeId, NodeKind};
use zinc::engine::Engine;

#[derive(Debug, Clone)]
pub struct ScriptOutcome {
    pub node: NodeId,
    pub source: String,
    pub result: String,
    pub output: Vec<String>,
}

/// Evaluate every inline `<script>` in document order through a single
/// long-lived Zinc engine. Scripts with a `src` attribute are skipped
/// (they would require fetching, which is the caller's job).
///
/// This variant runs scripts WITHOUT DOM bindings — `document` /
/// `window` are not installed. Useful for pure-JS smoke tests; for live
/// pages, prefer `execute_inline_scripts_with_dom`.
pub fn execute_inline_scripts(doc: &Document) -> Vec<ScriptOutcome> {
    let mut engine = Engine::new();
    let scripts = collect_scripts(doc);
    let mut out = Vec::with_capacity(scripts.len());
    for (node, source) in scripts {
        let body = match source {
            ScriptSource::Inline(s) => s,
            // No fetcher here — external scripts are skipped
            // for the test-only entry point. The real path goes
            // through `JsContext::install_and_run`.
            ScriptSource::External(_) => continue,
        };
        let (result, output) = engine.eval_with_output(&body);
        out.push(ScriptOutcome {
            node,
            source: body,
            result,
            output,
        });
    }
    out
}

/// Like `execute_inline_scripts`, but installs the full `window` /
/// `document` / `Element` bindings — read + write — before each
/// script runs. The document is shared via `Arc<Mutex<Document>>` so
/// mutations made by scripts (setAttribute, appendChild, createElement,
/// classList.add, …) are visible to the caller after this returns.
///
/// `current_url` is what `window.location.href` reports during the
/// script pass (read-only properties of `location` derive from it).
///
/// Returns `(outcomes, dirty, pending_nav)`:
/// - `dirty` is the shared flag the bindings tripped on every mutation —
///   the orchestrator uses it to decide whether to re-style + re-layout
///   before paint.
/// - `pending_nav` is `Some(url)` when a script asked us to navigate
///   via `window.location.href = …` (or `.assign` / `.replace`);
///   the embedder turns it into a real navigation.
pub fn execute_inline_scripts_with_dom(
    doc: Arc<Mutex<Document>>,
    current_url: String,
) -> (Vec<ScriptOutcome>, bool, Option<String>) {
    execute_inline_scripts_with_dom_and_fetcher(doc, current_url, None)
}

/// Like `execute_inline_scripts_with_dom`, but also supplies a
/// synchronous fetcher that backs the JS-side `fetch(url)`.
/// Without one, inline scripts that call `fetch` get a Response
/// shape whose `.ok` is `false` and body is empty.
///
/// Thin wrapper around [`JsContext::install_and_run`] for callers
/// who don't need to keep the engine alive past the script pass.
/// New code should prefer `JsContext` directly so user-input
/// dispatches can reuse the engine the page set up.
pub fn execute_inline_scripts_with_dom_and_fetcher(
    doc: Arc<Mutex<Document>>,
    current_url: String,
    fetcher: Option<Fetcher>,
) -> (Vec<ScriptOutcome>, bool, Option<String>) {
    let (mut ctx, outcomes) = JsContext::install_and_run(doc, current_url, fetcher);
    let dirty = ctx.take_dirty();
    let pending_nav = ctx.take_pending_navigation();
    (outcomes, dirty, pending_nav)
}

/// Persistent JS context for one page load. Owns the `Engine`, the
/// `BindingContext`, and the shared state (dirty flag, pending
/// navigation, listener map) used by both the initial script pass
/// and any post-load event dispatch the embedder fires.
///
/// One `JsContext` per `TabState` — torn down when the tab
/// navigates to a new URL. While it's alive, user-input handlers
/// can keep firing `submit` / `click` / `keydown` events into the
/// same listener map the inline scripts registered against, and
/// the engine's globals (closures, captured state) persist across
/// every dispatch.
pub struct JsContext {
    engine: Engine,
    bindings: BindingContext,
    /// Arc-clone of the document the bindings were installed
    /// against. The embedder hands the same Arc into this struct
    /// AND keeps a clone on `TabState`; both see the same DOM.
    doc: Arc<Mutex<Document>>,
    /// Length of `engine.vm().output` immediately after the
    /// last `take_console_lines()` drain. Subsequent calls
    /// return only the lines appended since — console.log
    /// messages and `Uncaught exception …` lines pushed by
    /// `dispatch_inner` both land here.
    output_cursor: usize,
}

impl JsContext {
    /// Install the binding surface, run every inline `<script>`
    /// in document order, and fire one synthetic `load` event
    /// once the script pass finishes registering listeners.
    /// Returns the live context for further user-input
    /// dispatches plus the per-script outcomes (so the dev-dock
    /// Console can render them).
    pub fn install_and_run(
        doc: Arc<Mutex<Document>>,
        current_url: String,
        fetcher: Option<Fetcher>,
    ) -> (Self, Vec<ScriptOutcome>) {
        let scripts = {
            let d = doc.lock().unwrap();
            collect_scripts(&d)
        };

        let mut engine = Engine::new();
        // Cap VM steps per `eval` / `host_call` so a runaway script
        // (Google's homepage hits requestAnimationFrame loops and
        // similar) can't hang the browser indefinitely. 50M is
        // generous enough that real pages finish their bootstrap
        // (Wikipedia, the dev-dock probe page) yet aborts a true
        // infinite loop in under a second. Without this the
        // browser thread blocks forever on bad JS.
        engine.set_max_steps(50_000_000);
        // Silence Zinc's default stdout / stderr writes for
        // `console.log / warn / error`. The embedder is the
        // single source of truth for routing those lines into
        // the dev-dock Console (via `take_console_lines`); the
        // VM still captures every output line into its internal
        // `output` buffer regardless.
        engine.set_silent_console(true);
        let bindings = BindingContext::install_with_fetcher(
            &mut engine,
            doc.clone(),
            current_url,
            fetcher.clone(),
        );

        let mut outcomes = Vec::with_capacity(scripts.len());
        for (script_idx, (node, source)) in scripts.into_iter().enumerate() {
            // Resolve external scripts via the embedder's
            // fetcher — without this, every modern site that
            // loads its main bundle via `<script src=…>` ran
            // its inline `<script>`s alone and missed the
            // actual rendering code (Google's xjs, React's
            // app.js, every CDN'd bundle).
            //
            // Size cap. Earlier debugging showed the SIGTRAP
            // we saw with 1 MB+ bundles was actually caused
            // by our own closure-IIFE shim — feeding the
            // xjs bundle through `closure_shim::maybe_inject`
            // let execution proceed deep enough into the
            // bundle to overflow Zinc's call stack. With the
            // shim now gated at 64 KB (small inline IIFEs
            // only), large external bundles run unshimmed
            // and hit a clean RuntimeError on the first `_.X`
            // access instead of SIGTRAPping. Capping at 2 MB
            // gives us headroom for Google's xjs (1 MB) +
            // gstatic's og.asy (264 KB) + most React/Vue/
            // Svelte CDN bundles.
            const EXTERNAL_SCRIPT_CAP: usize = 2 * 1024 * 1024;
            let source: String = match source {
                ScriptSource::Inline(s) => s,
                ScriptSource::External(url) => {
                    let Some(ref f) = fetcher else { continue };
                    let Some(resp) = f(&url) else { continue };
                    if !(200..300).contains(&resp.status) {
                        outcomes.push(ScriptOutcome {
                            node,
                            source: format!("<external {url}>"),
                            result: format!("HTTP {}: skipping", resp.status),
                            output: vec![format!(
                                "[js] external {} returned HTTP {}",
                                url, resp.status
                            )],
                        });
                        continue;
                    }
                    if resp.body.len() > EXTERNAL_SCRIPT_CAP {
                        outcomes.push(ScriptOutcome {
                            node,
                            source: format!("<external {url}>"),
                            result: format!(
                                "skipped: {} bytes > {} cap",
                                resp.body.len(),
                                EXTERNAL_SCRIPT_CAP
                            ),
                            output: vec![format!(
                                "[js] external {} skipped ({} bytes > {} cap)",
                                url,
                                resp.body.len(),
                                EXTERNAL_SCRIPT_CAP
                            )],
                        });
                        continue;
                    }
                    String::from_utf8_lossy(&resp.body).into_owned()
                }
            };
            // First-cut Phase 19 (docs/google-render-plan.md):
            // detect Closure-compiler IIFEs and inject a
            // namespace stub so the `_.X` cascade no longer
            // throws on every access. The bundle still won't
            // render Google's results — real Closure runtime is
            // a multi-week vendoring task — but defusing the
            // error cascade is real progress toward the
            // eventual proper fix.
            //
            // Cap the shim at 64 KB. The xjs bundle (1 MB+)
            // also matches the IIFE pattern but feeding it
            // through the shim lets execution go deeper into
            // the bundle, where Zinc's call stack overflows
            // and SIGTRAPs the browser. The shim's actual
            // value is on the small gbar/inline IIFEs that
            // are well under 64 KB; gating by size keeps
            // those working while letting large external
            // bundles run unshimmed (they hit a clean
            // RuntimeError on the first `_.X` access, the
            // browser stays alive).
            let rewritten = if source.len() <= 64 * 1024 {
                closure_shim::maybe_inject(&source)
            } else {
                source.clone()
            };
            let (result, mut output) = engine.eval_with_output(&rewritten);
            if is_script_error(&result) {
                // Capture a source preview so the dev-dock
                // Console + the [js] tracer can identify
                // which inline script failed — naming by
                // index alone is meaningless when a page
                // has 15 scripts. 80 chars is enough to
                // pick out the canonical first-line shape
                // ("(function(){this.gbar_=…", "var _g=…",
                // …) without flooding the log.
                let preview: String = source
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(80)
                    .collect();
                output.push(format!(
                    "Uncaught {result}  [script #{script_idx}: {preview}…]"
                ));
            }
            outcomes.push(ScriptOutcome {
                node,
                source,
                result,
                output,
            });
        }

        // Capture the post-eval output cursor so the load-event
        // dispatch below — and every subsequent dispatch — only
        // surfaces lines pushed during *that* dispatch, not the
        // initial script-pass output (which is already returned
        // via `outcomes`).
        let cursor = engine.vm().output.len();
        let mut ctx = JsContext {
            engine,
            bindings,
            doc,
            output_cursor: cursor,
        };
        // Synthetic `load` event so handlers registered via
        // `window.addEventListener('load', …)` get one fan-out
        // opportunity right after the script pass.
        let root = ctx.doc.lock().unwrap().root;
        let _ = ctx.dispatch(Event::new("load", root));
        // Deliver any fetch() results that already completed during
        // the script pass (and the immediate rejections of a
        // fetcher-less embedder) before handing the context back.
        ctx.deliver_fetch_completions();
        (ctx, outcomes)
    }

    /// Settle the promises of background `fetch()` calls whose
    /// network work finished. Runs on the engine thread; reaction
    /// callbacks fire via the microtask drain that follows.
    fn deliver_fetch_completions(&mut self) {
        let done = self.bindings.take_fetch_completions();
        if done.is_empty() {
            return;
        }
        let delivered = done.len();
        let vm = self.engine.vm();
        for (pid, result) in done {
            match result {
                Some(resp) => {
                    let ok = (200..300).contains(&resp.status);
                    let obj = vm.alloc_object();
                    vm.set_property(obj, "ok", Value::boolean(ok));
                    vm.set_property(obj, "status", Value::int(resp.status as i32));
                    let url_v = vm.value_from_str(&resp.url);
                    vm.set_property(obj, "url", url_v);
                    let body = String::from_utf8_lossy(&resp.body).into_owned();
                    let body_v = vm.value_from_str(&body);
                    vm.set_property(obj, "body", body_v);
                    vm.host_promise_resolve(pid, obj);
                }
                None => {
                    let reason = vm.value_from_str("fetch: network error");
                    vm.host_promise_reject(pid, reason);
                }
            }
        }
        self.bindings.fetch_delivered(delivered);
        let _ = self.engine.vm().drain_microtasks();
        self.deliver_mutations();
    }

    /// Background `fetch()` calls whose results JS hasn't observed
    /// yet. Sync embedder paths (CLI rendering) poll this to settle
    /// scripts before styling.
    pub fn pending_fetches(&self) -> usize {
        self.bindings.fetch_pending()
    }

    /// Evaluate a script in the page's engine and return the result's
    /// string form. Test / debug surface.
    pub fn eval(&mut self, src: &str) -> String {
        let (result, _) = self.engine.eval_with_output(src);
        result
    }

    /// Install the embedder wakeup used when a background fetch
    /// completes (typically: wake the UI event loop so the next tick
    /// settles the promise). No-op when already set.
    pub fn ensure_wake_hook(&self, hook: &std::sync::Arc<dyn Fn() + Send + Sync>) {
        self.bindings.ensure_fetch_wake(hook);
    }

    /// Dispatch a synthetic event through the registered JS
    /// listeners. Returns the post-dispatch event with
    /// `flags.default_prevented` / `flags.stop_propagation`
    /// folded in so the caller can suppress its default action.
    ///
    /// The doc lock is held just long enough to build the
    /// target's ancestor chain, then dropped before any
    /// listener runs — JS handlers routinely call back into
    /// `getAttribute` / `querySelector` host fns which relock
    /// the same doc, and holding it across the walk
    /// deadlocks the dispatch.
    pub fn dispatch(&mut self, event: Event) -> Event {
        let target = event.target;
        let dispatch_ctx = self
            .bindings
            .event_dispatch_ctx(self.bindings.handle_for_node(target));
        let path = {
            let dlocked = self.doc.lock().unwrap();
            crate::events::ancestor_path(&dlocked, target)
        };
        let listeners = self.bindings.listeners();
        let mut map = listeners.lock().unwrap();
        let out = map.dispatch_js_path(path, event, self.engine.vm(), &dispatch_ctx);
        // Drop the listener-map lock before draining microtasks —
        // a Promise.then callback that came due during dispatch
        // may register a new listener and would otherwise re-take
        // this same lock.
        drop(map);
        // Drain queued Promise reactions ([[ResolveJobs]] etc.).
        // Listeners that call `fetch(...).then(cb)` or
        // `await something` queue their continuation as a
        // microtask; without this drain the continuation never
        // runs — equivalent to the browser's "run a JS task,
        // then microtasks" abstraction.
        let _ = self.engine.vm().drain_microtasks();
        // Deliver any MutationObserver records the handler
        // produced (or that earlier mutations left pending).
        self.deliver_mutations();
        out
    }

    /// Drain queued Promise reactions. The embedder calls this
    /// from its frame tick so async work (fetch resolutions,
    /// async function continuations) doesn't accumulate across
    /// frames.
    pub fn drain_microtasks(&mut self) {
        let _ = self.engine.vm().drain_microtasks();
    }

    /// Deliver every `MutationObserver` callback whose
    /// `pending` queue has records since the last delivery.
    /// Each call hands the observer a JS array of
    /// MutationRecord-shaped objects (`{ type, target,
    /// attributeName, oldValue, addedNodes, removedNodes }`)
    /// and clears its queue.
    ///
    /// Loop: a callback may mutate the DOM, queueing more
    /// records on the *same* observer (or any other). We
    /// re-drain until no observer has anything pending or we
    /// hit an iteration cap (defends against pathological
    /// observe-yourself loops).
    pub fn deliver_mutations(&mut self) {
        let observers = self.bindings.observers();
        let elem_tag = self.bindings.elem_tag_raw();
        let _ = elem_tag; // referenced via ensure_handle_for
        const MAX_ITERATIONS: usize = 32;
        for _ in 0..MAX_ITERATIONS {
            // Snapshot which observers have pending records,
            // and drain each. Build the JS payload AFTER
            // dropping the observers lock so callbacks can
            // call observe/disconnect without deadlocking.
            let batches: Vec<(zinc::runtime::value::Value, Vec<crate::dom_bindings::MutationRecord>)> = {
                let mut obs = observers.lock().unwrap();
                let mut out = Vec::new();
                for entry in obs.values_mut() {
                    if entry.pending.is_empty() {
                        continue;
                    }
                    out.push((entry.callback, std::mem::take(&mut entry.pending)));
                }
                out
            };
            if batches.is_empty() {
                break;
            }
            for (callback, records) in batches {
                // Build the JS array of MutationRecord objects.
                let mut js_records: Vec<zinc::runtime::value::Value> =
                    Vec::with_capacity(records.len());
                for rec in &records {
                    let vm = self.engine.vm();
                    let obj = vm.alloc_object();
                    let kind_str = match rec.kind {
                        crate::dom_bindings::MutationKind::Attributes => "attributes",
                        crate::dom_bindings::MutationKind::ChildList => "childList",
                        crate::dom_bindings::MutationKind::CharacterData => "characterData",
                    };
                    let ks = vm.value_from_str(kind_str);
                    vm.set_property(obj, "type", ks);
                    let target_h = self.bindings.ensure_handle_for(rec.target, self.engine.vm());
                    self.engine.vm().set_property(obj, "target", target_h);
                    if !rec.attribute_name.is_empty() {
                        let vm = self.engine.vm();
                        let n = vm.value_from_str(&rec.attribute_name);
                        vm.set_property(obj, "attributeName", n);
                    } else {
                        self.engine.vm().set_property(
                            obj,
                            "attributeName",
                            zinc::runtime::value::Value::null(),
                        );
                    }
                    if !rec.old_value.is_empty() {
                        let vm = self.engine.vm();
                        let v = vm.value_from_str(&rec.old_value);
                        vm.set_property(obj, "oldValue", v);
                    } else {
                        self.engine.vm().set_property(
                            obj,
                            "oldValue",
                            zinc::runtime::value::Value::null(),
                        );
                    }
                    // addedNodes / removedNodes as arrays of
                    // wrapped element handles.
                    let mut added_vals = Vec::with_capacity(rec.added.len());
                    for nid in &rec.added {
                        let h = self.bindings.ensure_handle_for(*nid, self.engine.vm());
                        added_vals.push(h);
                    }
                    let added_arr = self.engine.vm().alloc_array(added_vals);
                    self.engine.vm().set_property(obj, "addedNodes", added_arr);
                    let mut removed_vals = Vec::with_capacity(rec.removed.len());
                    for nid in &rec.removed {
                        let h = self.bindings.ensure_handle_for(*nid, self.engine.vm());
                        removed_vals.push(h);
                    }
                    let removed_arr = self.engine.vm().alloc_array(removed_vals);
                    self.engine.vm().set_property(obj, "removedNodes", removed_arr);
                    js_records.push(obj);
                }
                let records_arr = self.engine.vm().alloc_array(js_records);
                // Call the observer with (records, observer).
                // We pass `null` for the second argument
                // because we don't yet have a Value-shaped
                // handle to the JS-side observer object — the
                // common access pattern is `function(records)`,
                // so this is rarely consulted.
                if self
                    .engine
                    .vm()
                    .host_call(callback, &[records_arr])
                    .is_err()
                {
                    self.engine
                        .vm()
                        .output
                        .push("Uncaught exception in MutationObserver callback".into());
                }
            }
            // After all callbacks ran, drain microtasks once
            // — the callback bodies may have queued Promise
            // continuations.
            let _ = self.engine.vm().drain_microtasks();
        }
    }

    /// Publish per-NodeId layout frames so JS reads of
    /// `getBoundingClientRect` / `offsetWidth` etc. return the
    /// real geometry. The embedder calls this once per layout
    /// pass with `(NodeId, (x, y, w, h))` tuples; previous
    /// entries are replaced atomically. Missing entries return
    /// zeros to JS — matching what real browsers report for
    /// pre-paint or detached elements.
    pub fn publish_layout_frames<I>(&self, frames: I)
    where
        I: IntoIterator<Item = (bui_dom::NodeId, (f32, f32, f32, f32))>,
    {
        let arc = self.bindings.layout_frames();
        let mut map = arc.lock().unwrap();
        map.clear();
        map.extend(frames);
    }

    /// Per-frame tick: fire every `setTimeout` / `setInterval` /
    /// `requestAnimationFrame` callback whose deadline elapsed
    /// at or before `now`, then drain queued microtasks. The
    /// embedder calls this each frame from its render loop —
    /// without it, scheduled work piles up forever and pages
    /// that rely on delayed callbacks (most of them) stall.
    ///
    /// Repeating intervals re-enqueue themselves at `now +
    /// repeat` after firing. A timer whose `clearTimeout` was
    /// called between scheduling and firing is already gone
    /// from the queue (we drain by removing matched entries),
    /// so this loop never needs to consult a separate set of
    /// cancellations.
    pub fn tick(&mut self, now: std::time::Instant) {
        // Settle completed background fetches first — their .then
        // callbacks run in the microtask drains below, same frame.
        self.deliver_fetch_completions();
        let due: Vec<crate::dom_bindings::ScheduledTimer> = {
            let timers = self.bindings.timers();
            let mut t = timers.lock().unwrap();
            let mut kept = Vec::with_capacity(t.len());
            let mut due = Vec::new();
            for entry in t.drain(..) {
                if entry.when <= now {
                    due.push(entry);
                } else {
                    kept.push(entry);
                }
            }
            *t = kept;
            due
        };
        // Sort by deadline so earlier timers fire first. Two
        // setTimeouts with the same `when` fire in insertion
        // order — Vec::sort_by is stable so that holds.
        let mut due = due;
        due.sort_by_key(|e| e.when);
        for entry in due {
            // Use host_call so a JS exception thrown inside the
            // callback comes back as Err and we route it to the
            // console buffer rather than panicking the host.
            if self
                .engine
                .vm()
                .host_call(entry.callback, &[])
                .is_err()
            {
                self.engine
                    .vm()
                    .output
                    .push("Uncaught exception in scheduled timer callback".into());
            }
            // Re-enqueue intervals.
            if let Some(repeat) = entry.repeat {
                let next = now + repeat;
                self.bindings
                    .timers()
                    .lock()
                    .unwrap()
                    .push(crate::dom_bindings::ScheduledTimer {
                        id: entry.id,
                        when: next,
                        callback: entry.callback,
                        repeat: Some(repeat),
                    });
            }
        }
        let _ = self.engine.vm().drain_microtasks();
        // Any DOM mutations from timers also fan out to
        // MutationObservers — same shape as event-handler
        // mutations.
        self.deliver_mutations();
    }

    /// Earliest pending timer deadline, if any. Lets the embedder
    /// schedule an event-loop wakeup at the right moment instead of
    /// relying on user input to trigger the tick that fires timers.
    pub fn next_timer_deadline(&self) -> Option<std::time::Instant> {
        let timers = self.bindings.timers();
        let t = timers.lock().unwrap();
        t.iter().map(|e| e.when).min()
    }

    /// Return — and consume — every console line the VM has
    /// pushed since the previous drain. Includes:
    /// - `console.log` / `console.warn` / `console.error` lines
    ///   the engine captured during the last dispatch.
    /// - `Uncaught exception in '<kind>' handler` lines pushed
    ///   by `dispatch_inner` when a JS listener throws.
    ///
    /// The embedder routes these into the dev-dock Console.
    /// Returns an empty Vec when nothing new appeared.
    pub fn take_console_lines(&mut self) -> Vec<String> {
        let buf = &self.engine.vm().output;
        if buf.len() <= self.output_cursor {
            return Vec::new();
        }
        let new_lines = buf[self.output_cursor..].to_vec();
        self.output_cursor = buf.len();
        new_lines
    }

    /// Drain the URL JS asked us to navigate to. Returns `None`
    /// if no script / handler set it since the last drain.
    pub fn take_pending_navigation(&self) -> Option<String> {
        self.bindings.take_pending_navigation()
    }

    /// Read + clear the dirty flag. The orchestrator polls this
    /// after every dispatch to decide whether to re-style + re-
    /// layout before the next paint.
    pub fn take_dirty(&mut self) -> bool {
        let flag = self.bindings.dirty();
        flag.swap(false, Ordering::SeqCst)
    }
}

fn is_script_error(result: &str) -> bool {
    result.starts_with("SyntaxError:")
        || result.starts_with("CompileError:")
        || result.starts_with("Error:")
        || result.starts_with("RuntimeError:")
}

/// One `<script>` element resolved for evaluation: either an
/// inline body or an external URL that needs fetching.
pub(crate) enum ScriptSource {
    Inline(String),
    External(String),
}

/// Walk the document for `<script>` elements in document order.
/// Inline scripts capture their text-node body; external
/// scripts carry the URL the embedder still needs to fetch
/// (deferred so the caller can use the live `Fetcher`).
fn collect_scripts(doc: &Document) -> Vec<(NodeId, ScriptSource)> {
    let mut out = Vec::new();
    for nid in doc.descendants(doc.root) {
        let Some(elem) = doc.element(nid) else {
            continue;
        };
        if elem.name != "script" {
            continue;
        }
        // `type="application/json"` etc. are data scripts — not
        // executable. Real browsers run only `text/javascript`
        // (default), `module`, or `text/ecmascript`. We don't
        // do modules yet so skip them too.
        let ty = elem
            .get_attr("type")
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        if !ty.is_empty()
            && ty != "text/javascript"
            && ty != "application/javascript"
            && ty != "application/ecmascript"
            && ty != "text/ecmascript"
        {
            continue;
        }
        if let Some(src) = elem.get_attr("src") {
            let src = src.trim();
            if !src.is_empty() {
                out.push((nid, ScriptSource::External(src.to_string())));
            }
            continue;
        }
        let mut source = String::new();
        let mut child = doc.node(nid).first_child;
        while let Some(c) = child {
            if let NodeKind::Text(t) = &doc.node(c).kind {
                source.push_str(t);
            }
            child = doc.node(c).next_sibling;
        }
        if !source.trim().is_empty() {
            out.push((nid, ScriptSource::Inline(source)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(html: &str) -> Document {
        let mut doc = Document::new();
        // Build a tiny synthetic tree: html > head > script with text.
        let html_id = doc.create_element("html");
        let head_id = doc.create_element("head");
        let script = doc.create_element("script");
        let txt = doc.create_text(html);
        doc.append_child(doc.root, html_id);
        doc.append_child(html_id, head_id);
        doc.append_child(head_id, script);
        doc.append_child(script, txt);
        doc
    }

    #[test]
    fn evaluates_simple_script() {
        let d = doc("var x = 2 + 3; console.log(x);");
        let results = execute_inline_scripts(&d);
        assert_eq!(results.len(), 1);
        // The script's last expression value is its result; console output is captured.
        assert!(results[0].output.iter().any(|line| line.contains("5")));
    }

    #[test]
    fn skips_external_scripts() {
        let mut d = Document::new();
        let html = d.create_element("html");
        let s = d.create_element("script");
        d.element_mut(s).unwrap().set_attr("src", "/foo.js");
        let body = d.create_text("console.log('should not run');");
        d.append_child(d.root, html);
        d.append_child(html, s);
        d.append_child(s, body);
        let results = execute_inline_scripts(&d);
        assert!(results.is_empty());
    }

    #[test]
    fn multiple_scripts_share_globals() {
        // Zinc 0.5 made VM state persistent across eval calls — so a global
        // set in one <script> is visible to the next. This is the contract
        // page-level scripts depend on.
        let mut d = Document::new();
        let html = d.create_element("html");
        let s1 = d.create_element("script");
        let t1 = d.create_text("var counter = 1;");
        let s2 = d.create_element("script");
        let t2 = d.create_text("counter += 41; console.log(counter);");
        d.append_child(d.root, html);
        d.append_child(html, s1);
        d.append_child(s1, t1);
        d.append_child(html, s2);
        d.append_child(s2, t2);
        let results = execute_inline_scripts(&d);
        assert_eq!(results.len(), 2);
        assert!(
            results[1].output.iter().any(|l| l.contains("42")),
            "expected 42 in second-script output, got {:?}",
            results[1].output
        );
    }

    #[test]
    fn host_fn_callable_from_js() {
        use std::sync::{Arc, Mutex};
        use zinc::runtime::value::Value;

        let mut engine = zinc::engine::Engine::new();

        // A counter that the host fn bumps every time JS calls it. Wrapped in
        // Arc<Mutex> because register_host_fn requires Send + Sync.
        let calls: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_for_fn = calls.clone();
        engine.register_host_fn("hostAddOne", move |_vm, _this, args| {
            let arg = args.first().and_then(|v| v.as_number()).unwrap_or(0.0);
            calls_for_fn.lock().unwrap().push(arg);
            Ok(Value::number(arg + 1.0))
        });

        let (result, _output) = engine.eval_with_output("hostAddOne(5) + hostAddOne(40)");
        // 6 + 41 = 47
        assert_eq!(result, "47");
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded, vec![5.0, 40.0]);
    }

    #[test]
    fn host_objects_round_trip_through_js() {
        // Strategy: pre-allocate the host objects we'll hand to JS (via the
        // public `Engine::alloc_host_object`), capture the resulting `Value`s
        // in the host-fn closure, and recover payloads via the public
        // `Engine::host_payload` after the script runs.
        //
        // This pattern fits a "freeze the DOM, hand out NodeId-keyed host
        // objects" model. For dynamic allocation from inside a host fn (the
        // shape `document.createElement` needs), Zinc currently doesn't
        // expose `heap` or a `Vm::alloc_host_object` mirror — see comment at
        // top of this file.
        use zinc::runtime::value::Value;

        let mut engine = zinc::engine::Engine::new();
        let elem_tag = engine.register_host_class("HTMLElement");

        // Pre-allocate three host objects standing in for DOM NodeIds.
        let nodes: Vec<Value> = (0..3)
            .map(|i| engine.alloc_host_object(elem_tag, 100 + i as u64))
            .collect();
        for (i, n) in nodes.iter().enumerate() {
            assert_eq!(engine.host_payload(*n), Some((elem_tag, 100 + i as u64)));
        }

        // Host fn returns the i-th node by index — like `getElementById` keyed
        // by ordinal.
        let captured = nodes.clone();
        engine.register_host_fn("nodeAt", move |_vm, _this, args| {
            let i = args
                .first()
                .and_then(|v| v.as_number())
                .map(|n| n as usize)
                .unwrap_or(usize::MAX);
            Ok(captured.get(i).copied().unwrap_or(Value::null()))
        });

        // JS picks node #2, runs it through a let binding, and hands the
        // host object back as the eval result.
        let result_val = engine
            .eval("let n = nodeAt(2); n")
            .expect("eval succeeds");
        assert_eq!(engine.host_payload(result_val), Some((elem_tag, 102)));

        // And the same object identity round-trips: nodeAt(0) called twice
        // returns the *same* JS object (===), proving the host objects are
        // first-class participants in the heap, not opaque copies.
        let same = engine
            .eval("nodeAt(0) === nodeAt(0)")
            .expect("eval succeeds");
        assert_eq!(same.as_bool(), Some(true));
    }

    /// Wire-level integration test for Phase 6 user-input dispatch:
    /// a page registers a `submit` listener on `document` during
    /// the inline script pass; later, the embedder fires a `submit`
    /// event on a form node via `JsContext::dispatch`. The event
    /// bubbles up to the document handler, which calls
    /// `preventDefault()` so the chrome suppresses its default
    /// navigate, and calls `location.assign(...)` so the chrome
    /// navigates to a JS-built URL instead.
    #[test]
    fn js_context_dispatches_submit_after_script_pass() {
        let mut d = Document::new();
        let html = d.create_element("html");
        let body = d.create_element("body");
        let form = d.create_element("form");
        d.element_mut(form).unwrap().set_attr("id", "f");
        let script = d.create_element("script");
        let src = d.create_text(
            "var fired = 0;\n\
             document.addEventListener('submit', function(e){\n\
                fired = fired + 1;\n\
                e.preventDefault();\n\
                location.assign('/from-js');\n\
             });",
        );
        d.append_child(d.root, html);
        d.append_child(html, body);
        d.append_child(body, form);
        d.append_child(html, script);
        d.append_child(script, src);

        let doc = Arc::new(Mutex::new(d));
        let (mut ctx, _outcomes) =
            JsContext::install_and_run(doc.clone(), "https://example.com/".into(), None);
        // No pending nav from the load event itself — the listener
        // is registered but not yet fired.
        assert!(ctx.take_pending_navigation().is_none());

        // Fire the synthetic submit at the form node — it bubbles
        // up to the document handler.
        let event = ctx.dispatch(Event::new("submit", form));
        assert!(
            event.flags.default_prevented,
            "handler called preventDefault, flag should fold back to Rust"
        );

        // Handler also called location.assign — drains as pending nav.
        let next = ctx
            .take_pending_navigation()
            .expect("handler called location.assign");
        assert_eq!(next, "/from-js");

        // And we observe the side-effect of the handler running.
        let (fired, _) = ctx.engine.eval_with_output("fired");
        assert_eq!(fired, "1");
    }

    /// External `<script src=…>` is fetched via the embedder's
    /// Fetcher and evaluated. Was: silently skipped, which
    /// meant every framework-driven page (Google, React apps,
    /// Vue apps) ran inline scripts in isolation and never
    /// loaded its actual bundle.
    #[test]
    fn external_script_is_fetched_and_executed() {
        let mut d = Document::new();
        let html = d.create_element("html");
        let body = d.create_element("body");
        let script = d.create_element("script");
        d.element_mut(script)
            .unwrap()
            .set_attr("src", "/app.js");
        d.append_child(d.root, html);
        d.append_child(html, body);
        d.append_child(html, script);

        let doc = Arc::new(Mutex::new(d));
        let fetcher: crate::Fetcher = std::sync::Arc::new(|url: &str| {
            assert_eq!(url, "/app.js");
            Some(crate::FetchResponse {
                status: 200,
                url: url.to_string(),
                body: b"globalThis.fromExternal = 42;".to_vec(),
            })
        });
        let (mut ctx, _outcomes) = JsContext::install_and_run(
            doc.clone(),
            "https://example.com/".into(),
            Some(fetcher),
        );
        let (got, _) = ctx.engine.eval_with_output("fromExternal");
        assert_eq!(got, "42");
    }

    /// External script with a non-200 response is recorded
    /// as a skipped outcome so the dev-dock Console shows the
    /// failure, but no JS evaluation runs. 404 / 500 / network
    /// failure all land here.
    #[test]
    fn external_script_with_404_records_skipped_outcome() {
        let mut d = Document::new();
        let html = d.create_element("html");
        let script = d.create_element("script");
        d.element_mut(script).unwrap().set_attr("src", "/missing.js");
        d.append_child(d.root, html);
        d.append_child(html, script);

        let doc = Arc::new(Mutex::new(d));
        let fetcher: crate::Fetcher = std::sync::Arc::new(|url: &str| {
            Some(crate::FetchResponse {
                status: 404,
                url: url.to_string(),
                body: b"not found".to_vec(),
            })
        });
        let (_ctx, outcomes) = JsContext::install_and_run(
            doc.clone(),
            "https://example.com/".into(),
            Some(fetcher),
        );
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].result.contains("HTTP 404"));
    }

    /// MutationObserver fires real records — setAttribute on an
    /// observed subtree queues an `attributes` record; appendChild
    /// queues a `childList` record. Was: stub that never fired.
    #[test]
    fn mutation_observer_fires_on_set_attribute() {
        let mut d = Document::new();
        let html = d.create_element("html");
        let body = d.create_element("body");
        let div = d.create_element("div");
        d.element_mut(div).unwrap().set_attr("id", "d");
        d.append_child(d.root, html);
        d.append_child(html, body);
        d.append_child(body, div);
        let script = d.create_element("script");
        let src = d.create_text(
            "globalThis.records = [];\n\
             var mo = new MutationObserver(function(recs){\n\
                 for (var i = 0; i < recs.length; i++) {\n\
                     globalThis.records.push(recs[i].type + ':' + recs[i].attributeName);\n\
                 }\n\
             });\n\
             mo.observe(document.querySelector('#d'), { attributes: true, attributeOldValue: true });\n\
             document.querySelector('#d').setAttribute('data-foo', 'bar');\n\
             document.querySelector('#d').setAttribute('data-baz', 'qux');",
        );
        d.append_child(d.root, html); // ensure html attached
        d.append_child(html, script);
        d.append_child(script, src);

        let doc = Arc::new(Mutex::new(d));
        let (mut ctx, _outcomes) =
            JsContext::install_and_run(doc.clone(), "https://example.com/".into(), None);

        // Records delivered after the script ran (load event
        // dispatch + microtask drain). Two setAttribute calls
        // → two records.
        let (got, _) = ctx
            .engine
            .eval_with_output("records.join(',')");
        assert_eq!(got, "attributes:data-foo,attributes:data-baz");
    }

    /// MutationObserver fires `childList` records on
    /// appendChild + removeChild. Verifies the observed
    /// target is the *parent*, with the added/removed
    /// children populating addedNodes / removedNodes.
    #[test]
    fn mutation_observer_fires_on_child_list() {
        let mut d = Document::new();
        let html = d.create_element("html");
        let body = d.create_element("body");
        let parent = d.create_element("div");
        d.element_mut(parent).unwrap().set_attr("id", "p");
        d.append_child(d.root, html);
        d.append_child(html, body);
        d.append_child(body, parent);
        let script = d.create_element("script");
        let src = d.create_text(
            "globalThis.log = [];\n\
             var mo = new MutationObserver(function(recs){\n\
                 for (var i = 0; i < recs.length; i++) {\n\
                     var r = recs[i];\n\
                     globalThis.log.push(r.type + ':+'+r.addedNodes.length+'-'+r.removedNodes.length);\n\
                 }\n\
             });\n\
             mo.observe(document.querySelector('#p'), { childList: true });\n\
             var span = document.createElement('span');\n\
             document.querySelector('#p').appendChild(span);\n\
             document.querySelector('#p').removeChild(span);",
        );
        d.append_child(html, script);
        d.append_child(script, src);

        let doc = Arc::new(Mutex::new(d));
        let (mut ctx, _outcomes) =
            JsContext::install_and_run(doc.clone(), "https://example.com/".into(), None);
        let (got, _) = ctx.engine.eval_with_output("log.join(',')");
        assert_eq!(got, "childList:+1-0,childList:+0-1");
    }

    /// `getBoundingClientRect()` returns the geometry the
    /// embedder published for the element after the most
    /// recent layout pass. Was: zero-stub.
    #[test]
    fn bounding_client_rect_reads_published_frame() {
        let mut d = Document::new();
        let html = d.create_element("html");
        let body = d.create_element("body");
        let div = d.create_element("div");
        d.element_mut(div).unwrap().set_attr("id", "d");
        d.append_child(d.root, html);
        d.append_child(html, body);
        d.append_child(body, div);

        let doc = Arc::new(Mutex::new(d));
        let (mut ctx, _outcomes) =
            JsContext::install_and_run(doc.clone(), "https://example.com/".into(), None);

        // Before publish: rect is all zeros (matching the
        // pre-paint browser default).
        let (before, _) = ctx
            .engine
            .eval_with_output(
                "(function(){\
                     var r = document.querySelector('#d').getBoundingClientRect();\
                     return r.x + ',' + r.y + ',' + r.width + ',' + r.height;\
                 })()",
            );
        assert_eq!(before, "0,0,0,0");

        // Publish a frame for the div, then re-read.
        ctx.publish_layout_frames(vec![(div, (10.0, 20.0, 300.0, 50.0))]);
        let (after, _) = ctx.engine.eval_with_output(
            "(function(){\
                 var r = document.querySelector('#d').getBoundingClientRect();\
                 return r.x + ',' + r.y + ',' + r.width + ',' + r.height + \
                        ',' + r.right + ',' + r.bottom;\
             })()",
        );
        assert_eq!(after, "10,20,300,50,310,70");

        let (ow, _) = ctx
            .engine
            .eval_with_output("document.querySelector('#d').offsetWidth");
        assert_eq!(ow, "300");
        let (oh, _) = ctx
            .engine
            .eval_with_output("document.querySelector('#d').offsetHeight");
        assert_eq!(oh, "50");
    }

    /// `MutationObserver` / `IntersectionObserver` /
    /// `ResizeObserver` constructors return a no-op-shaped
    /// object. Feature-probe code (the typical
    /// `if (window.MutationObserver) {…}` pattern) gets a
    /// truthy answer and `.observe(…)` doesn't throw.
    #[test]
    fn observer_constructors_return_noop_shapes() {
        let mut d = Document::new();
        let html = d.create_element("html");
        let body = d.create_element("body");
        d.append_child(d.root, html);
        d.append_child(html, body);

        let doc = Arc::new(Mutex::new(d));
        let (mut ctx, _outcomes) =
            JsContext::install_and_run(doc.clone(), "https://example.com/".into(), None);

        // Constructor probe succeeds.
        let (typ, _) = ctx
            .engine
            .eval_with_output("typeof MutationObserver");
        assert_eq!(typ, "function");

        // Constructed object exposes the expected method set.
        let (got, _) = ctx.engine.eval_with_output(
            "(function(){\
                 var mo = new MutationObserver(function(){});\
                 mo.observe(document.body, { childList: true });\
                 mo.disconnect();\
                 return mo.takeRecords().length;\
             })()",
        );
        assert_eq!(got, "0");

        // IntersectionObserver and ResizeObserver behave the
        // same way.
        let (io_typ, _) = ctx
            .engine
            .eval_with_output("typeof IntersectionObserver");
        assert_eq!(io_typ, "function");
        let (ro_typ, _) = ctx
            .engine
            .eval_with_output("typeof ResizeObserver");
        assert_eq!(ro_typ, "function");
    }

    /// Real `setTimeout` with a delay: a script schedules a
    /// 50 ms timer; calling `tick(now)` with `now` before the
    /// deadline doesn't fire it, calling `tick` past the
    /// deadline does. Locks in that the timer-queue
    /// integration actually wires both directions.
    #[test]
    fn set_timeout_with_delay_fires_when_tick_passes_deadline() {
        use std::time::{Duration, Instant};

        let mut d = Document::new();
        let html = d.create_element("html");
        let script = d.create_element("script");
        let src = d.create_text(
            "globalThis.fired = 0;\n\
             setTimeout(function(){ globalThis.fired = 1; }, 50);",
        );
        d.append_child(d.root, html);
        d.append_child(html, script);
        d.append_child(script, src);

        let doc = Arc::new(Mutex::new(d));
        let (mut ctx, _outcomes) =
            JsContext::install_and_run(doc.clone(), "https://example.com/".into(), None);

        // Immediately after install: the 50 ms timer is queued
        // but not yet due. tick(now) leaves it pending.
        ctx.tick(Instant::now());
        let (before, _) = ctx.engine.eval_with_output("fired");
        assert_eq!(before, "0");

        // 100 ms in the future is past the deadline → fires.
        ctx.tick(Instant::now() + Duration::from_millis(100));
        let (after, _) = ctx.engine.eval_with_output("fired");
        assert_eq!(after, "1");
    }

    /// clearTimeout cancels a pending timer before it fires.
    #[test]
    fn clear_timeout_cancels_pending() {
        use std::time::{Duration, Instant};

        let mut d = Document::new();
        let html = d.create_element("html");
        let script = d.create_element("script");
        let src = d.create_text(
            "globalThis.fired = 0;\n\
             var id = setTimeout(function(){ globalThis.fired = 1; }, 50);\n\
             clearTimeout(id);",
        );
        d.append_child(d.root, html);
        d.append_child(html, script);
        d.append_child(script, src);

        let doc = Arc::new(Mutex::new(d));
        let (mut ctx, _outcomes) =
            JsContext::install_and_run(doc.clone(), "https://example.com/".into(), None);

        ctx.tick(Instant::now() + Duration::from_millis(100));
        let (fired, _) = ctx.engine.eval_with_output("fired");
        assert_eq!(fired, "0");
    }

    /// Event-handler runtime exceptions are routed into the
    /// per-context console buffer (was: silently swallowed).
    /// The dev-dock Console picks these lines up via
    /// `take_console_lines` on every dispatch — the embedder no
    /// longer flies blind when a handler throws.
    #[test]
    fn uncaught_event_handler_exception_lands_in_console_buffer() {
        let mut d = Document::new();
        let html = d.create_element("html");
        let body = d.create_element("body");
        let form = d.create_element("form");
        let script = d.create_element("script");
        let src = d.create_text(
            "document.addEventListener('submit', function(){\n\
                 throw new Error('boom');\n\
             });",
        );
        d.append_child(d.root, html);
        d.append_child(html, body);
        d.append_child(body, form);
        d.append_child(html, script);
        d.append_child(script, src);

        let doc = Arc::new(Mutex::new(d));
        let (mut ctx, _outcomes) =
            JsContext::install_and_run(doc.clone(), "https://example.com/".into(), None);
        // Drain anything from the load event so the next drain
        // is just our submit listener.
        let _ = ctx.take_console_lines();

        let _ = ctx.dispatch(Event::new("submit", form));
        let lines = ctx.take_console_lines();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Uncaught exception") && l.contains("submit")),
            "expected Uncaught-exception line in {lines:?}",
        );
    }

    /// console.log calls inside an event handler land in
    /// `take_console_lines()` so the dev-dock Console renders
    /// them in real time. Was: lost in vm.output, never read.
    #[test]
    fn handler_console_log_lands_in_console_buffer() {
        let mut d = Document::new();
        let html = d.create_element("html");
        let body = d.create_element("body");
        let form = d.create_element("form");
        let script = d.create_element("script");
        let src = d.create_text(
            "document.addEventListener('submit', function(){\n\
                 console.log('handler ran');\n\
             });",
        );
        d.append_child(d.root, html);
        d.append_child(html, body);
        d.append_child(body, form);
        d.append_child(html, script);
        d.append_child(script, src);

        let doc = Arc::new(Mutex::new(d));
        let (mut ctx, _outcomes) =
            JsContext::install_and_run(doc.clone(), "https://example.com/".into(), None);
        let _ = ctx.take_console_lines();

        let _ = ctx.dispatch(Event::new("submit", form));
        let lines = ctx.take_console_lines();
        assert_eq!(lines, vec!["handler ran".to_string()]);
    }

    /// Phase 7: Promise.then callbacks inside an event handler
    /// run before `dispatch` returns. Real browsers expose this
    /// as "microtask drain after every task"; without explicit
    /// drainage, `.then(cb)` queues `cb` and never fires.
    ///
    /// This is the prerequisite for async fetch (Phase 9) and
    /// async/await in event handlers.
    #[test]
    fn promise_then_inside_event_handler_fires_before_dispatch_returns() {
        let mut d = Document::new();
        let html = d.create_element("html");
        let body = d.create_element("body");
        let form = d.create_element("form");
        let script = d.create_element("script");
        let src = d.create_text(
            "globalThis.steps = [];\n\
             document.addEventListener('submit', function(e){\n\
                 globalThis.steps.push('sync');\n\
                 Promise.resolve('async').then(function(v){\n\
                     globalThis.steps.push(v);\n\
                 });\n\
             });",
        );
        d.append_child(d.root, html);
        d.append_child(html, body);
        d.append_child(body, form);
        d.append_child(html, script);
        d.append_child(script, src);

        let doc = Arc::new(Mutex::new(d));
        let (mut ctx, _outcomes) =
            JsContext::install_and_run(doc.clone(), "https://example.com/".into(), None);
        let _ = ctx.dispatch(Event::new("submit", form));
        // Both the synchronous handler body and the Promise.then
        // continuation should have run by the time `dispatch`
        // returns — that's the contract microtask drainage gives
        // us.
        let (steps, _) = ctx.engine.eval_with_output("steps.join(',')");
        assert_eq!(steps, "sync,async");
    }

    /// Phase 7: async function with `await` inside an event
    /// handler also drains by the time `dispatch` returns. The
    /// `await` desugars into a Promise continuation queued onto
    /// the same microtask queue.
    #[test]
    fn async_await_inside_event_handler_runs_to_completion() {
        let mut d = Document::new();
        let html = d.create_element("html");
        let body = d.create_element("body");
        let form = d.create_element("form");
        let script = d.create_element("script");
        let src = d.create_text(
            "globalThis.result = null;\n\
             async function go() { return await Promise.resolve(42); }\n\
             document.addEventListener('submit', function(_e){\n\
                 go().then(function(v){ globalThis.result = v; });\n\
             });",
        );
        d.append_child(d.root, html);
        d.append_child(html, body);
        d.append_child(body, form);
        d.append_child(html, script);
        d.append_child(script, src);

        let doc = Arc::new(Mutex::new(d));
        let (mut ctx, _outcomes) =
            JsContext::install_and_run(doc.clone(), "https://example.com/".into(), None);
        let _ = ctx.dispatch(Event::new("submit", form));
        let (result, _) = ctx.engine.eval_with_output("result");
        assert_eq!(result, "42");
    }

    /// Regression: a JS listener that calls back into a host fn
    /// requiring the doc lock used to deadlock because the
    /// dispatch held the same lock across the listener walk.
    /// Now `JsContext::dispatch` resolves the ancestor chain
    /// up front and releases the lock before invoking
    /// listeners. This test wires Google's actual homepage
    /// pattern — handler does `e.target.getAttribute(...)` —
    /// and asserts the dispatch completes and the handler
    /// observed the real attribute value.
    ///
    /// Note: we can't move the dispatch off this thread
    /// (Zinc's `Engine` isn't `Send` because its JIT cache
    /// holds raw executable-page pointers), so if a future
    /// change reintroduces the deadlock this test hangs the
    /// suite. The hang is a strictly louder failure than a
    /// silent regression.
    #[test]
    fn dispatch_doesnt_deadlock_when_listener_calls_doc_locking_host_fn() {
        let mut d = Document::new();
        let html = d.create_element("html");
        let body = d.create_element("body");
        let form = d.create_element("form");
        d.element_mut(form).unwrap().set_attr("data-submitfalse", "1");
        let script = d.create_element("script");
        let src = d.create_text(
            "globalThis.seenC = 'not-fired';\n\
             document.addEventListener('submit', function(e){\n\
                 // The exact shape of Google's submit interceptor —\n\
                 // it reads an attribute off the event target, which\n\
                 // routes through a host fn that relocks the same\n\
                 // Mutex<Document> the dispatch was holding.\n\
                 globalThis.seenC = e.target.getAttribute('data-submitfalse');\n\
             });",
        );
        d.append_child(d.root, html);
        d.append_child(html, body);
        d.append_child(body, form);
        d.append_child(html, script);
        d.append_child(script, src);

        let doc = Arc::new(Mutex::new(d));
        let (mut ctx, _outcomes) =
            JsContext::install_and_run(doc.clone(), "https://example.com/".into(), None);
        let _ = ctx.dispatch(Event::new("submit", form));
        let (seen, _) = ctx.engine.eval_with_output("seenC");
        assert_eq!(seen, "1", "handler ran and read the form attribute");
    }

    /// Same shape as the document-level test above, but the
    /// listener is registered on the form element via the
    /// element-level `addEventListener` (the more common
    /// pattern in modern code). Locks in that
    /// `_wrapElem.addEventListener` routes through the host fn
    /// with the right target handle.
    #[test]
    fn element_level_add_event_listener_fires_on_target() {
        let mut d = Document::new();
        let html = d.create_element("html");
        let body = d.create_element("body");
        let form = d.create_element("form");
        d.element_mut(form).unwrap().set_attr("id", "f");
        let script = d.create_element("script");
        let src = d.create_text(
            "var fired = 0;\n\
             var f = document.getElementById('f');\n\
             f.addEventListener('submit', function(e){\n\
                fired = fired + 1;\n\
                e.preventDefault();\n\
             });",
        );
        d.append_child(d.root, html);
        d.append_child(html, body);
        d.append_child(body, form);
        d.append_child(html, script);
        d.append_child(script, src);

        let doc = Arc::new(Mutex::new(d));
        let (mut ctx, _outcomes) =
            JsContext::install_and_run(doc.clone(), "https://example.com/".into(), None);
        let event = ctx.dispatch(Event::new("submit", form));
        assert!(event.flags.default_prevented);
        let (fired, _) = ctx.engine.eval_with_output("fired");
        assert_eq!(fired, "1");
    }
}
