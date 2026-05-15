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

pub mod dom_bindings;
pub mod events;

pub use dom_bindings::BindingContext;

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
    let scripts = collect_inline_scripts(doc);
    let mut out = Vec::with_capacity(scripts.len());
    for (node, source) in scripts {
        let (result, output) = engine.eval_with_output(&source);
        out.push(ScriptOutcome {
            node,
            source,
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
    // Collect script sources up front. We hold the doc lock only
    // for the walk so install + eval can take it themselves.
    let scripts = {
        let d = doc.lock().unwrap();
        collect_inline_scripts(&d)
    };

    let mut engine = Engine::new();
    let ctx = BindingContext::install(&mut engine, doc, current_url);
    let dirty_flag = ctx.dirty();

    let mut out = Vec::with_capacity(scripts.len());
    for (node, source) in scripts {
        // Zinc has a known panic in upvalue-closing on deeply-nested
        // closures (vm.rs:663). On heavy pages (google.com) this
        // sometimes fires. `catch_unwind` keeps a single bad script
        // from killing the entire browser; the panicked script's
        // contribution is dropped and execution continues with the
        // next `<script>` block.
        //
        // `AssertUnwindSafe` is fine here: `engine` is mutable
        // state we discard right after the loop if anything went
        // wrong, and the DOM is recovered by the outer style + layout
        // pass independent of script execution.
        let result_pair = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            engine.eval_with_output(&source)
        }));
        let (result, mut output) = match result_pair {
            Ok(pair) => pair,
            Err(_) => (
                "Error: zinc VM panic during script (skipped)".to_string(),
                Vec::new(),
            ),
        };
        // Zinc's eval_with_output returns the error message in
        // `result_str` (prefixed `SyntaxError:` / `CompileError:` /
        // `Error:`) when a script faults. Surface it as a log line
        // so the dev-dock Console renders the failure instead of
        // silently swallowing it.
        if is_script_error(&result) {
            output.push(format!("Uncaught {result}"));
        }
        out.push(ScriptOutcome {
            node,
            source,
            result,
            output,
        });
    }
    let dirty = dirty_flag.load(Ordering::SeqCst);
    let pending_nav = ctx.take_pending_navigation();
    (out, dirty, pending_nav)
}

fn is_script_error(result: &str) -> bool {
    result.starts_with("SyntaxError:")
        || result.starts_with("CompileError:")
        || result.starts_with("Error:")
        || result.starts_with("RuntimeError:")
}

/// Walk the document for inline `<script>` elements (no `src`) and
/// concatenate their text-node children into a list of
/// `(NodeId, source)` pairs in document order.
fn collect_inline_scripts(doc: &Document) -> Vec<(NodeId, String)> {
    let mut out = Vec::new();
    for nid in doc.descendants(doc.root) {
        let Some(elem) = doc.element(nid) else {
            continue;
        };
        if elem.name != "script" {
            continue;
        }
        if elem.get_attr("src").is_some() {
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
            out.push((nid, source));
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
}
