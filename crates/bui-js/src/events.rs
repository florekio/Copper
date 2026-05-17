//! DOM event dispatch.
//!
//! Implements `Event`, `EventListenerMap`, and the capture → target →
//! bubble walk over a `Document`. Listeners come in two flavours:
//!   * `Listener::Rust` — plain Rust closure. Used by tests and any
//!     embedder code that wants to react to a fired event without
//!     bouncing through JS.
//!   * `Listener::Js` — a Zinc `Value` callable. Registered by JS
//!     scripts via `addEventListener`; fired by `dispatch_js`, which
//!     calls each listener through `Vm::host_call`.
//!
//! What's deferred:
//!   * Wiring user input (click / keydown / submit) at the chrome
//!     layer so real events fan out into here.
//!   * Event interfaces (`MouseEvent`, `KeyboardEvent`, `InputEvent`,
//!     …). The current `Event` carries only `type` and a generic
//!     `data` map; the JS-side `Event` object surfaces just those.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use bui_dom::{Document, NodeId};
use zinc::runtime::value::Value;
use zinc::vm::vm::Vm;

/// Per-dispatch context plumbed from the embedder. Carries the
/// pre-allocated callables backing `event.preventDefault()` /
/// `event.stopPropagation()` so the JS handler can flip
/// Rust-side flags via host_fn, plus the host handle to set
/// as `event.target` (when one is registered for the target
/// NodeId), and the shared atomic the host fns mutate.
///
/// One atomic per `JsContext` is reused across dispatches — we
/// store the previous value, reset to zero, run the dispatch,
/// fold the resulting flags into the `Event`, then restore the
/// previous value. Re-entrant dispatches stay correct as long
/// as they don't share an event (they don't).
pub struct EventDispatchCtx {
    pub event_flags: Arc<AtomicU32>,
    pub prevent_default_fn: Value,
    pub stop_propagation_fn: Value,
    pub target_handle: Option<Value>,
}

pub const EVT_FLAG_DEFAULT_PREVENTED: u32 = 1;
pub const EVT_FLAG_STOP_PROPAGATION: u32 = 2;

#[derive(Debug, Clone)]
pub struct Event {
    pub kind: String,
    pub target: NodeId,
    pub current_target: NodeId,
    pub phase: Phase,
    pub bubbles: bool,
    pub cancelable: bool,
    pub flags: EventFlags,
    pub data: HashMap<String, String>,
}

impl Event {
    pub fn new(kind: impl Into<String>, target: NodeId) -> Self {
        Self {
            kind: kind.into(),
            target,
            current_target: target,
            phase: Phase::None,
            bubbles: true,
            cancelable: true,
            flags: EventFlags::default(),
            data: HashMap::new(),
        }
    }

    pub fn prevent_default(&mut self) {
        if self.cancelable {
            self.flags.default_prevented = true;
        }
    }

    pub fn stop_propagation(&mut self) {
        self.flags.stop_propagation = true;
    }

    pub fn stop_immediate_propagation(&mut self) {
        self.flags.stop_propagation = true;
        self.flags.stop_immediate = true;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    None,
    Capturing,
    AtTarget,
    Bubbling,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EventFlags {
    pub default_prevented: bool,
    pub stop_propagation: bool,
    pub stop_immediate: bool,
}

pub enum Listener {
    /// A Rust closure. Cheap, no engine round-trip, used by tests
    /// and any embedder code that wants to react in-process. The
    /// `Send + Sync` bounds match the `Engine::register_host_fn`
    /// closure bound so the same listener type can be shared
    /// through the `Arc<Mutex<EventListenerMap>>` the bindings
    /// install.
    Rust(Box<dyn FnMut(&mut Event) + Send + Sync>),
    /// A JS callable. Registered via `addEventListener` from a
    /// script; fired by `dispatch_js` which routes through the VM.
    Js(Value),
}

#[derive(Default)]
pub struct EventListenerMap {
    entries: HashMap<(NodeId, String, bool), Vec<Listener>>,
}

impl EventListenerMap {
    pub fn add(&mut self, target: NodeId, kind: &str, capture: bool, listener: Listener) {
        self.entries
            .entry((target, kind.to_string(), capture))
            .or_default()
            .push(listener);
    }

    /// Convenience: add a Rust-closure listener.
    pub fn add_rust(
        &mut self,
        target: NodeId,
        kind: &str,
        capture: bool,
        listener: Box<dyn FnMut(&mut Event) + Send + Sync>,
    ) {
        self.add(target, kind, capture, Listener::Rust(listener));
    }

    /// Convenience: add a JS-callable listener.
    pub fn add_js(&mut self, target: NodeId, kind: &str, capture: bool, callable: Value) {
        self.add(target, kind, capture, Listener::Js(callable));
    }

    /// Dispatch firing only Rust-side listeners. JS-side listeners
    /// in the same entry are skipped because a VM isn't available.
    /// Used by tests and any host code without engine access.
    pub fn dispatch(&mut self, doc: &Document, event: Event) -> Event {
        let path = ancestor_path(doc, event.target);
        self.dispatch_inner(path, event, None, None)
    }

    /// Dispatch firing every listener — Rust-side and JS-side.
    /// The VM calls each `Listener::Js` value with a synthetic
    /// event object as its single argument. `ctx` provides the
    /// preventDefault / stopPropagation callables (bound to the
    /// shared atomic) plus the host handle to surface as
    /// `event.target`. Any thrown JS exception is swallowed (the
    /// dev-dock Console will pick it up in a follow-up that
    /// threads error reporting through here).
    pub fn dispatch_js(
        &mut self,
        doc: &Document,
        event: Event,
        vm: &mut Vm,
        ctx: &EventDispatchCtx,
    ) -> Event {
        let path = ancestor_path(doc, event.target);
        self.dispatch_js_path(path, event, vm, ctx)
    }

    /// Dispatch where the caller has already resolved the
    /// target's ancestor chain (root-most last). Use this
    /// when you can't hold the document lock across the
    /// dispatch — any JS listener that reaches back into a
    /// host fn (`getAttribute`, `querySelector`, …) tries to
    /// relock the same doc and would otherwise deadlock.
    pub fn dispatch_js_path(
        &mut self,
        path: Vec<NodeId>,
        event: Event,
        vm: &mut Vm,
        ctx: &EventDispatchCtx,
    ) -> Event {
        let prev = ctx.event_flags.swap(0, Ordering::SeqCst);
        let mut out = self.dispatch_inner(path, event, Some(vm), Some(ctx));
        let raised = ctx.event_flags.swap(prev, Ordering::SeqCst);
        if raised & EVT_FLAG_DEFAULT_PREVENTED != 0 {
            out.flags.default_prevented = true;
        }
        if raised & EVT_FLAG_STOP_PROPAGATION != 0 {
            out.flags.stop_propagation = true;
        }
        out
    }

    fn dispatch_inner(
        &mut self,
        path: Vec<NodeId>,
        mut event: Event,
        vm: Option<&mut Vm>,
        ctx: Option<&EventDispatchCtx>,
    ) -> Event {
        // Wrap the optional VM in a small cell so the borrow checker
        // is happy when `invoke` is called multiple times in this
        // function. We pass `&mut Option<&mut Vm>` down.
        let mut vm_slot = vm;

        // Capture phase: ancestors (path reversed without the target).
        event.phase = Phase::Capturing;
        for &node in path.iter().rev().take(path.len().saturating_sub(1)) {
            event.current_target = node;
            self.invoke(node, true, &mut event, &mut vm_slot, ctx);
            // Both Rust-side (event.flags) and JS-side (ctx
            // atomic) stop_propagation halt the walk.
            if event.flags.stop_propagation || ctx_stopped(ctx) {
                fold_atomic(ctx, &mut event);
                return event;
            }
        }
        // At target: both capture (true) and bubble (false) listeners fire.
        event.phase = Phase::AtTarget;
        event.current_target = event.target;
        self.invoke(event.target, true, &mut event, &mut vm_slot, ctx);
        if !event.flags.stop_immediate {
            self.invoke(event.target, false, &mut event, &mut vm_slot, ctx);
        }
        if event.flags.stop_propagation || ctx_stopped(ctx) {
            fold_atomic(ctx, &mut event);
            return event;
        }
        // Bubble phase, only if event bubbles.
        if event.bubbles {
            event.phase = Phase::Bubbling;
            for &node in path.iter().skip(1) {
                event.current_target = node;
                self.invoke(node, false, &mut event, &mut vm_slot, ctx);
                if event.flags.stop_propagation || ctx_stopped(ctx) {
                    break;
                }
            }
        }
        event.phase = Phase::None;
        fold_atomic(ctx, &mut event);
        event
    }

    fn invoke(
        &mut self,
        node: NodeId,
        capture: bool,
        event: &mut Event,
        vm: &mut Option<&mut Vm>,
        ctx: Option<&EventDispatchCtx>,
    ) {
        let key = (node, event.kind.clone(), capture);
        let Some(list) = self.entries.get_mut(&key) else { return };
        for listener in list.iter_mut() {
            match listener {
                Listener::Rust(f) => f(event),
                Listener::Js(callable) => {
                    if let Some(vm_ref) = vm.as_deref_mut() {
                        let event_obj = build_js_event(vm_ref, event, ctx);
                        if vm_ref.host_call(*callable, &[event_obj]).is_err() {
                            // Surface uncaught exceptions to the
                            // embedder's console drain via the
                            // VM's `output` buffer. We don't have
                            // public Vm access to resolve the
                            // error Value's `.message` into a
                            // string here (the interner is
                            // crate-private); a generic label is
                            // honest about that limit and still
                            // strictly better than the silent
                            // swallow we had before.
                            vm_ref
                                .output
                                .push(format!("Uncaught exception in '{}' handler", event.kind));
                        }
                    }
                }
            }
            if event.flags.stop_immediate {
                break;
            }
        }
    }
}

/// Walk `target` → root, collecting NodeIds in document-leaf-first
/// order. Caller holds the doc lock just long enough to build the
/// path, then drops it before invoking listeners — so a JS handler
/// can call back into a host fn that relocks the same doc.
pub fn ancestor_path(doc: &Document, target: NodeId) -> Vec<NodeId> {
    let mut path = Vec::new();
    let mut cur = Some(target);
    while let Some(id) = cur {
        path.push(id);
        cur = doc.node(id).parent;
    }
    path
}

fn ctx_stopped(ctx: Option<&EventDispatchCtx>) -> bool {
    ctx.map(|c| c.event_flags.load(Ordering::SeqCst) & EVT_FLAG_STOP_PROPAGATION != 0)
        .unwrap_or(false)
}

/// Snapshot the shared atomic into the in-flight `Event` so
/// subsequent native (Rust-side) listeners observe the same
/// state JS just set via preventDefault / stopPropagation.
fn fold_atomic(ctx: Option<&EventDispatchCtx>, event: &mut Event) {
    let Some(c) = ctx else { return };
    let raised = c.event_flags.load(Ordering::SeqCst);
    if raised & EVT_FLAG_DEFAULT_PREVENTED != 0 {
        event.flags.default_prevented = true;
    }
    if raised & EVT_FLAG_STOP_PROPAGATION != 0 {
        event.flags.stop_propagation = true;
    }
}

/// Build a minimal JS `Event` object the listener callback sees as
/// its single argument. Carries `type`, `bubbles`, `cancelable`,
/// `defaultPrevented`, `target` (set to the host handle from
/// `ctx.target_handle` when provided, `null` otherwise), and
/// `preventDefault` / `stopPropagation` methods that mutate the
/// shared `event_flags` atomic so the Rust side can observe
/// cancellation after dispatch.
fn build_js_event(vm: &mut Vm, event: &Event, ctx: Option<&EventDispatchCtx>) -> Value {
    let obj = vm.alloc_object();
    let type_val = vm.value_from_str(&event.kind);
    vm.set_property(obj, "type", type_val);
    vm.set_property(obj, "bubbles", Value::boolean(event.bubbles));
    vm.set_property(obj, "cancelable", Value::boolean(event.cancelable));
    vm.set_property(
        obj,
        "defaultPrevented",
        Value::boolean(event.flags.default_prevented),
    );
    if let Some(c) = ctx {
        vm.set_property(obj, "target", c.target_handle.unwrap_or(Value::null()));
        // currentTarget tracks the node the listener was bound to
        // during the walk. For the synthetic event we surface the
        // same handle as target — it's a small inaccuracy when a
        // listener fires during bubble / capture on an ancestor,
        // but the common addEventListener('submit', form, …)
        // pattern reads only `event.target` anyway.
        vm.set_property(
            obj,
            "currentTarget",
            c.target_handle.unwrap_or(Value::null()),
        );
        vm.set_property(obj, "preventDefault", c.prevent_default_fn);
        vm.set_property(obj, "stopPropagation", c.stop_propagation_fn);
    } else {
        vm.set_property(obj, "target", Value::null());
        vm.set_property(obj, "currentTarget", Value::null());
    }
    obj
}

#[cfg(test)]
mod tests {
    use super::*;
    use bui_dom::Document;
    use std::sync::{Arc, Mutex};

    fn three_level() -> (Document, NodeId, NodeId, NodeId) {
        let mut d = Document::new();
        let a = d.create_element("section");
        let b = d.create_element("div");
        let c = d.create_element("button");
        d.append_child(d.root, a);
        d.append_child(a, b);
        d.append_child(b, c);
        (d, a, b, c)
    }

    #[test]
    fn capture_target_bubble_order() {
        let (doc, a, b, c) = three_level();
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let mut map = EventListenerMap::default();
        for (id, name, capture) in &[
            (a, "A-cap", true),
            (b, "B-cap", true),
            (c, "C-target-cap", true),
            (c, "C-target-bub", false),
            (b, "B-bub", false),
            (a, "A-bub", false),
        ] {
            let log = log.clone();
            let label = name.to_string();
            map.add_rust(*id, "click", *capture, Box::new(move |_| {
                log.lock().unwrap().push(label.clone())
            }));
        }

        let _ = map.dispatch(&doc, Event::new("click", c));
        assert_eq!(
            log.lock().unwrap().as_slice(),
            &[
                "A-cap",
                "B-cap",
                "C-target-cap",
                "C-target-bub",
                "B-bub",
                "A-bub",
            ]
        );
    }

    #[test]
    fn stop_propagation_halts_bubble() {
        let (doc, a, b, c) = three_level();
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut map = EventListenerMap::default();

        let log_a = log.clone();
        map.add_rust(a, "click", false, Box::new(move |_| {
            log_a.lock().unwrap().push("a".into())
        }));
        let log_b = log.clone();
        map.add_rust(
            b,
            "click",
            false,
            Box::new(move |e| {
                log_b.lock().unwrap().push("b".into());
                e.stop_propagation();
            }),
        );
        let log_c = log.clone();
        map.add_rust(c, "click", false, Box::new(move |_| {
            log_c.lock().unwrap().push("c".into())
        }));

        let _ = map.dispatch(&doc, Event::new("click", c));
        assert_eq!(log.lock().unwrap().as_slice(), &["c", "b"]);
    }

    fn test_dispatch_ctx(event_flags: Arc<AtomicU32>) -> EventDispatchCtx {
        EventDispatchCtx {
            event_flags,
            // For tests that don't exercise preventDefault, a
            // null callable is fine — the JS handler doesn't
            // reach for it.
            prevent_default_fn: Value::null(),
            stop_propagation_fn: Value::null(),
            target_handle: None,
        }
    }

    #[test]
    fn js_listener_fires_via_dispatch_js() {
        // End-to-end: register a JS callback via addEventListener,
        // dispatch a synthetic event, observe the side effect the
        // callback wrote to a global.
        use zinc::engine::Engine;

        let mut engine = Engine::new();
        let (doc, _a, _b, c) = three_level();

        // Shared map captured by the host fn closure.
        let map: Arc<Mutex<EventListenerMap>> = Arc::new(Mutex::new(EventListenerMap::default()));

        let map_for_fn = map.clone();
        let target = c;
        engine.register_host_fn("regClick", move |_vm, _this, args| {
            let Some(callable) = args.get(1).copied() else { return Ok(Value::null()) };
            if let Ok(mut m) = map_for_fn.lock() {
                m.add_js(target, "click", false, callable);
            }
            Ok(Value::null())
        });

        engine
            .eval("var fired = 0; regClick('click', function(){ fired = fired + 1; });")
            .expect("register");

        let flags = Arc::new(AtomicU32::new(0));
        let ctx = test_dispatch_ctx(flags.clone());
        let vm = engine.vm();
        let _ = map
            .lock()
            .unwrap()
            .dispatch_js(&doc, Event::new("click", c), vm, &ctx);

        let (out, _) = engine.eval_with_output("fired");
        assert_eq!(out, "1");
    }

    #[test]
    fn js_listener_can_prevent_default() {
        // Wire up preventDefault end-to-end: the host fn flips the
        // shared atomic; dispatch_js folds it into Event.flags.
        use zinc::engine::Engine;

        let mut engine = Engine::new();
        let (doc, _a, _b, c) = three_level();

        let flags = Arc::new(AtomicU32::new(0));
        let flags_for_fn = flags.clone();
        engine.register_host_fn("__pd", move |_vm, _this, _args| {
            flags_for_fn.fetch_or(EVT_FLAG_DEFAULT_PREVENTED, Ordering::SeqCst);
            Ok(Value::null())
        });
        let prevent_default_fn = engine.eval("__pd").expect("fetch __pd");

        let map: Arc<Mutex<EventListenerMap>> = Arc::new(Mutex::new(EventListenerMap::default()));
        let map_for_fn = map.clone();
        let target = c;
        engine.register_host_fn("regSubmit", move |_vm, _this, args| {
            let Some(callable) = args.get(1).copied() else { return Ok(Value::null()) };
            if let Ok(mut m) = map_for_fn.lock() {
                m.add_js(target, "submit", false, callable);
            }
            Ok(Value::null())
        });
        engine
            .eval("regSubmit('submit', function(e){ e.preventDefault(); });")
            .expect("register");

        let ctx = EventDispatchCtx {
            event_flags: flags,
            prevent_default_fn,
            stop_propagation_fn: Value::null(),
            target_handle: None,
        };
        let vm = engine.vm();
        let out = map
            .lock()
            .unwrap()
            .dispatch_js(&doc, Event::new("submit", c), vm, &ctx);
        assert!(out.flags.default_prevented);
    }
}
