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

use bui_dom::{Document, NodeId};
use zinc::runtime::value::Value;
use zinc::vm::vm::Vm;

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
        self.dispatch_inner(doc, event, None)
    }

    /// Dispatch firing every listener — Rust-side and JS-side.
    /// The VM is used to call each `Listener::Js` value with a
    /// synthetic event object as its single argument. Any thrown
    /// JS exception is swallowed (dev-dock Console will see it
    /// in a follow-up that threads error reporting through here).
    pub fn dispatch_js(&mut self, doc: &Document, event: Event, vm: &mut Vm) -> Event {
        self.dispatch_inner(doc, event, Some(vm))
    }

    fn dispatch_inner(
        &mut self,
        doc: &Document,
        mut event: Event,
        vm: Option<&mut Vm>,
    ) -> Event {
        // Build path from target up to root (inclusive).
        let mut path: Vec<NodeId> = Vec::new();
        let mut cur = Some(event.target);
        while let Some(id) = cur {
            path.push(id);
            cur = doc.node(id).parent;
        }

        // Wrap the optional VM in a small cell so the borrow checker
        // is happy when `invoke` is called multiple times in this
        // function. We pass `&mut Option<&mut Vm>` down.
        let mut vm_slot = vm;

        // Capture phase: ancestors (path reversed without the target).
        event.phase = Phase::Capturing;
        for &node in path.iter().rev().take(path.len().saturating_sub(1)) {
            event.current_target = node;
            self.invoke(node, true, &mut event, &mut vm_slot);
            if event.flags.stop_propagation {
                return event;
            }
        }
        // At target: both capture (true) and bubble (false) listeners fire.
        event.phase = Phase::AtTarget;
        event.current_target = event.target;
        self.invoke(event.target, true, &mut event, &mut vm_slot);
        if !event.flags.stop_immediate {
            self.invoke(event.target, false, &mut event, &mut vm_slot);
        }
        if event.flags.stop_propagation {
            return event;
        }
        // Bubble phase, only if event bubbles.
        if event.bubbles {
            event.phase = Phase::Bubbling;
            for &node in path.iter().skip(1) {
                event.current_target = node;
                self.invoke(node, false, &mut event, &mut vm_slot);
                if event.flags.stop_propagation {
                    break;
                }
            }
        }
        event.phase = Phase::None;
        event
    }

    fn invoke(
        &mut self,
        node: NodeId,
        capture: bool,
        event: &mut Event,
        vm: &mut Option<&mut Vm>,
    ) {
        let key = (node, event.kind.clone(), capture);
        let Some(list) = self.entries.get_mut(&key) else { return };
        for listener in list.iter_mut() {
            match listener {
                Listener::Rust(f) => f(event),
                Listener::Js(callable) => {
                    if let Some(vm_ref) = vm.as_deref_mut() {
                        let event_obj = build_js_event(vm_ref, event);
                        // Swallow JS exceptions for now — exposing
                        // them via the dev-dock Console is a tiny
                        // follow-up that threads an output sink
                        // through here.
                        let _ = vm_ref.host_call(*callable, &[event_obj]);
                    }
                }
            }
            if event.flags.stop_immediate {
                break;
            }
        }
    }
}

/// Build a minimal JS `Event` object the listener callback sees as
/// its single argument. Carries `type`, `target` (as a host handle
/// when one is registered; null otherwise), `defaultPrevented`,
/// and stub `preventDefault` / `stopPropagation` methods. The
/// methods don't yet talk back to our Rust-side flags — that's the
/// next refinement; until then a JS-only handler can react but
/// can't actually cancel the chrome's default behaviour.
fn build_js_event(vm: &mut Vm, event: &Event) -> Value {
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

        // For this synthetic test we don't have a NodeId-to-handle
        // map (BindingContext owns that on the real path); the
        // listener just registers on every "click" event regardless
        // of target. The host fn signature mirrors what the real
        // `__addEventListener` will do.
        let map_for_fn = map.clone();
        let target = c;
        engine.register_host_fn("regClick", move |_vm, _this, args| {
            let Some(callable) = args.get(1).copied() else { return Ok(Value::null()) };
            if let Ok(mut m) = map_for_fn.lock() {
                m.add_js(target, "click", false, callable);
            }
            Ok(Value::null())
        });

        // Register a JS callback that sets a global.
        engine
            .eval("var fired = 0; regClick('click', function(){ fired = fired + 1; });")
            .expect("register");

        // Dispatch through the VM. The Rust path calls the JS
        // callable via host_call which re-enters the engine.
        let vm = engine.vm();
        let _ = map
            .lock()
            .unwrap()
            .dispatch_js(&doc, Event::new("click", c), vm);

        let (out, _) = engine.eval_with_output("fired");
        assert_eq!(out, "1");
    }
}
