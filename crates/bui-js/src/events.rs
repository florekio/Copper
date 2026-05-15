//! DOM event dispatch (Phase 9 scaffolding).
//!
//! Implements the structural pieces — `Event`, `EventListenerMap`, and the
//! capture → target → bubble walk over a `Document` — without depending on
//! Zinc. Once the Zinc patch from Phase 5.5 lands and we can register
//! `NativeFn`s for `addEventListener`, this module wires straight into the
//! engine: a JS listener becomes an entry in `EventListenerMap` keyed by
//! `(NodeId, type)`, and `dispatch` invokes them through the engine.
//!
//! What's deferred:
//!   * Actual JS callbacks. The current `Listener` is a plain `Box<dyn Fn>`
//!     so Rust callers can wire up listeners for tests / the shell layer.
//!   * `Event.preventDefault()` / `stopPropagation()` honored via
//!     `EventFlags`; the `defaultPrevented` field is updated but the shell
//!     doesn't yet ask it before performing default actions.
//!   * Event interfaces (`MouseEvent`, `KeyboardEvent`, `InputEvent`, …).
//!     The current `Event` only carries `type` and a generic `data` map.

use std::collections::HashMap;

use bui_dom::{Document, NodeId};

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

pub type Listener = Box<dyn FnMut(&mut Event)>;

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

    pub fn dispatch(&mut self, doc: &Document, mut event: Event) -> Event {
        // Build path from target up to root (inclusive).
        let mut path: Vec<NodeId> = Vec::new();
        let mut cur = Some(event.target);
        while let Some(id) = cur {
            path.push(id);
            cur = doc.node(id).parent;
        }

        // Capture phase: ancestors (path reversed without the target).
        event.phase = Phase::Capturing;
        for &node in path.iter().rev().take(path.len().saturating_sub(1)) {
            event.current_target = node;
            self.invoke(node, true, &mut event);
            if event.flags.stop_propagation {
                return event;
            }
        }
        // At target: both capture (true) and bubble (false) listeners fire.
        event.phase = Phase::AtTarget;
        event.current_target = event.target;
        self.invoke(event.target, true, &mut event);
        if !event.flags.stop_immediate {
            self.invoke(event.target, false, &mut event);
        }
        if event.flags.stop_propagation {
            return event;
        }
        // Bubble phase, only if event bubbles.
        if event.bubbles {
            event.phase = Phase::Bubbling;
            for &node in path.iter().skip(1) {
                event.current_target = node;
                self.invoke(node, false, &mut event);
                if event.flags.stop_propagation {
                    break;
                }
            }
        }
        event.phase = Phase::None;
        event
    }

    fn invoke(&mut self, node: NodeId, capture: bool, event: &mut Event) {
        let key = (node, event.kind.clone(), capture);
        if let Some(list) = self.entries.get_mut(&key) {
            for listener in list.iter_mut() {
                listener(event);
                if event.flags.stop_immediate {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bui_dom::Document;
    use std::cell::RefCell;
    use std::rc::Rc;

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
        let log = Rc::new(RefCell::new(Vec::<String>::new()));

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
            map.add(*id, "click", *capture, Box::new(move |_| log.borrow_mut().push(label.clone())));
        }

        let _ = map.dispatch(&doc, Event::new("click", c));
        assert_eq!(
            log.borrow().as_slice(),
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
        let log = Rc::new(RefCell::new(Vec::<String>::new()));
        let mut map = EventListenerMap::default();

        let log_a = log.clone();
        map.add(a, "click", false, Box::new(move |_| log_a.borrow_mut().push("a".into())));
        let log_b = log.clone();
        map.add(
            b,
            "click",
            false,
            Box::new(move |e| {
                log_b.borrow_mut().push("b".into());
                e.stop_propagation();
            }),
        );
        let log_c = log.clone();
        map.add(c, "click", false, Box::new(move |_| log_c.borrow_mut().push("c".into())));

        let _ = map.dispatch(&doc, Event::new("click", c));
        assert_eq!(log.borrow().as_slice(), &["c", "b"]);
    }
}
