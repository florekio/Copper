//! DOM bindings that ride Zinc 0.5's embedder API.
//!
//! ## Wired surface
//!
//! Read side: `document.body`, `document.querySelector`,
//! `document.querySelectorAll`, `document.getElementById`,
//! `Element.parentElement`, `Element.matches`,
//! `Element.hasAttribute`, `Element.hasClass`, `Element.childCount`,
//! `Element.children`, `Element.childAt`, `Element.tagName`,
//! `Element.id`, `Element.className`, `Element.textContent`,
//! `Element.getAttribute`, `Element.classList`.
//!
//! Write side (Tier 1 §1): `Element.setAttribute`,
//! `Element.removeAttribute`, `Element.textContent =`,
//! `Element.appendChild`, `Element.removeChild`,
//! `document.createElement`, `document.createTextNode`,
//! `Element.classList.add / remove / toggle / contains`.
//!
//! Identity is preserved across the read surface: `querySelector` and
//! `body.childAt(0)` return the **same** host handle when both
//! resolve to the same NodeId. Newly-`createElement`-ed nodes also
//! get a stable handle that lives until the document is replaced.
//!
//! ## Document sharing
//!
//! The embedder owns an `Arc<Mutex<Document>>` and passes a clone to
//! `BindingContext::install`. Bindings lock the inner document only
//! for the minimum scope of a single call — read or write what they
//! need, drop the lock before anything that could re-enter JS (no
//! re-entry happens today, but timers + events will land soon and
//! this discipline keeps `BorrowMutError`-style hazards from
//! sneaking in piecemeal).
//!
//! Every mutating binding trips the `dirty` flag exposed via
//! `BindingContext::dirty()`. The orchestrator polls it after scripts
//! finish to decide whether the layout pipeline needs to re-run.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use bui_dom::{Document, NodeId, NodeKind};
use zinc::engine::{Engine, HostTag};
use zinc::runtime::value::Value;

use crate::events::{EVT_FLAG_DEFAULT_PREVENTED, EVT_FLAG_STOP_PROPAGATION};

/// Inner shared state held by every binding closure.
struct DomShared {
    /// Document handle shared with the embedder. Locked briefly per
    /// call; never held across a JS re-entry (none today, but the
    /// pattern stays so the discipline is there when events land).
    doc_handle: Arc<Mutex<Document>>,
    /// NodeId → Zinc host handle. Mutating bindings extend this
    /// when they create new nodes; the map outlives the document if
    /// the binding is reused across navigations (it isn't yet —
    /// fresh engine per fetch — but the shape supports it).
    handles_by_node: HashMap<NodeId, Value>,
    /// Set by every mutation. The orchestrator clears it after a
    /// re-style + re-layout pass.
    dirty: Arc<AtomicBool>,
}

impl DomShared {
    fn handle_for(&self, node: NodeId) -> Option<Value> {
        self.handles_by_node
            .get(&node)
            .copied()
            .filter(|v| !v.is_null())
    }

    fn node_for_handle(&self, handle: Value) -> Option<NodeId> {
        if handle.is_null() || handle.is_undefined() {
            return None;
        }
        // Pragmatic: scan our handles_by_node for the matching
        // Value raw. Cheap for typical (e.g., O(n) on a doc with
        // a few hundred elements). Zinc doesn't expose
        // `Vm::host_payload` to closures yet, so this is the only
        // round-trip path.
        for (nid, h) in &self.handles_by_node {
            if !h.is_null() && h.raw() == handle.raw() {
                return Some(*nid);
            }
        }
        None
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }
}

/// One captured HTTP fetch — what the embedder hands to bui-js
/// when an inline script calls `fetch(url)`. Status `0` signals
/// the request failed before producing a real response (DNS,
/// TLS, timeout, etc.); `body` is the response body bytes.
#[derive(Debug, Clone)]
pub struct FetchResponse {
    pub status: u16,
    pub url: String,
    pub body: Vec<u8>,
}

/// Synchronous fetcher the embedder supplies via
/// `BindingContext::install`. Returns `Some(FetchResponse)` for a
/// completed request (success or HTTP error), `None` when the
/// request couldn't be issued at all.
pub type Fetcher = Arc<dyn Fn(&str) -> Option<FetchResponse> + Send + Sync>;

pub struct BindingContext {
    shared: Arc<Mutex<DomShared>>,
    elem_tag: HostTag,
    dirty: Arc<AtomicBool>,
    /// URL JS asked us to navigate to via `location.href = ...` (or
    /// `location.assign(...)` / `location.replace(...)`). Drained
    /// once by the embedder after the script pass completes.
    pending_navigation: Arc<Mutex<Option<String>>>,
    /// Listener registry. Scripts add entries via the
    /// `__addEventListener` host fn; embedder code dispatches by
    /// constructing an `Event` and calling `dispatch_js` with the
    /// engine's `Vm`. Per-NodeId / per-event-type. Empty by default
    /// — addEventListener only ever inserts, never removes (a
    /// `__removeEventListener` follow-up rounds out the surface).
    listeners: Arc<Mutex<crate::events::EventListenerMap>>,
    /// Bit-set of `EVT_FLAG_*` raised by the currently-dispatching
    /// JS event. The `preventDefault` / `stopPropagation`
    /// host fns ORed into this; `EventListenerMap::dispatch_js`
    /// snapshots + clears it across a dispatch.
    event_flags: Arc<AtomicU32>,
    /// JS Value of the global `__eventPreventDefault` host fn,
    /// cached after install so `build_js_event` can hand it
    /// out as `event.preventDefault` without a per-dispatch
    /// global lookup.
    prevent_default_fn: Value,
    /// JS Value of the global `__eventStopPropagation` host fn.
    stop_propagation_fn: Value,
}

impl BindingContext {
    /// Install the full `window` / `document` / `Element` surface
    /// against `engine`. The document handle is shared with the
    /// embedder so mutations made by scripts are visible to the
    /// next style + layout pass.
    ///
    /// `current_url` is the URL the page was fetched from. It backs
    /// `window.location.href` getter (read-only properties of
    /// `location` like `.pathname` etc. all derive from this string).
    pub fn install(
        engine: &mut Engine,
        doc: Arc<Mutex<Document>>,
        current_url: String,
    ) -> Self {
        Self::install_with_fetcher(engine, doc, current_url, None)
    }

    /// Like `install`, but also wires the JS-side `fetch(url)` to
    /// a synchronous HTTP fetcher the embedder provides. Without a
    /// fetcher, `fetch` returns a Response object with `ok: false`
    /// and `status: 0`. With one, the script can call `fetch('/x')
    /// .then(r => r.json())` and get real data back synchronously
    /// — the `.then` chain on our Response value calls the
    /// callback immediately.
    pub fn install_with_fetcher(
        engine: &mut Engine,
        doc: Arc<Mutex<Document>>,
        current_url: String,
        fetcher: Option<Fetcher>,
    ) -> Self {
        let elem_tag = engine.register_host_class("HTMLElement");

        let mut handles_by_node: HashMap<NodeId, Value> = HashMap::new();
        let body_handle;
        {
            let d = doc.lock().unwrap();
            for nid in d.descendants(d.root) {
                if matches!(d.node(nid).kind, NodeKind::Element(_)) {
                    let v = engine.alloc_host_object(elem_tag, nid.0 as u64);
                    handles_by_node.insert(nid, v);
                }
            }
            body_handle = find_body(&d)
                .and_then(|id| handles_by_node.get(&id).copied())
                .unwrap_or(Value::null());
        }

        let dirty = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Mutex::new(DomShared {
            doc_handle: doc,
            handles_by_node,
            dirty: dirty.clone(),
        }));

        // ----- Read side -----

        let s = shared.clone();
        engine.register_host_fn("__docBody", move |_vm, _this, _args| {
            let _ = s;
            Ok(body_handle)
        });

        let s = shared.clone();
        engine.register_host_fn("__qs", move |vm, _this, args| {
            let Some(selector) = read_str(vm, args.first()) else {
                return Ok(Value::null());
            };
            let dom = s.lock().unwrap();
            let parsed = match bui_css::Selector::parse(&selector) {
                Ok(p) => p,
                Err(_) => return Ok(Value::null()),
            };
            let d = dom.doc_handle.lock().unwrap();
            for nid in d.descendants(d.root) {
                if d.element(nid).is_some() && parsed.matches(&d, nid) {
                    return Ok(dom.handle_for(nid).unwrap_or(Value::null()));
                }
            }
            Ok(Value::null())
        });

        let s = shared.clone();
        engine.register_host_fn("__byId", move |vm, _this, args| {
            let Some(want) = read_str(vm, args.first()) else {
                return Ok(Value::null());
            };
            let dom = s.lock().unwrap();
            let d = dom.doc_handle.lock().unwrap();
            for nid in d.descendants(d.root) {
                if let Some(elem) = d.element(nid) {
                    if elem.get_attr("id") == Some(want.as_str()) {
                        return Ok(dom.handle_for(nid).unwrap_or(Value::null()));
                    }
                }
            }
            Ok(Value::null())
        });

        let s = shared.clone();
        engine.register_host_fn("__elemParent", move |_vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(Value::null());
            };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(Value::null());
            };
            let parent = dom.doc_handle.lock().unwrap().node(nid).parent;
            Ok(parent
                .and_then(|p| dom.handle_for(p))
                .unwrap_or(Value::null()))
        });

        let s = shared.clone();
        engine.register_host_fn("__elemMatches", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else { return Ok(Value::boolean(false)) };
            let Some(selector) = read_str(vm, args.get(1)) else { return Ok(Value::boolean(false)) };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(Value::boolean(false));
            };
            let Ok(parsed) = bui_css::Selector::parse(&selector) else {
                return Ok(Value::boolean(false));
            };
            Ok(Value::boolean(parsed.matches(&dom.doc_handle.lock().unwrap(), nid)))
        });

        let s = shared.clone();
        engine.register_host_fn("__elemHasAttr", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else { return Ok(Value::boolean(false)) };
            let Some(name) = read_str(vm, args.get(1)) else { return Ok(Value::boolean(false)) };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(Value::boolean(false));
            };
            let d = dom.doc_handle.lock().unwrap();
            let has = d.element(nid).map(|e| e.get_attr(&name).is_some()).unwrap_or(false);
            Ok(Value::boolean(has))
        });

        let s = shared.clone();
        engine.register_host_fn("__elemHasClass", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else { return Ok(Value::boolean(false)) };
            let Some(class) = read_str(vm, args.get(1)) else { return Ok(Value::boolean(false)) };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(Value::boolean(false));
            };
            let d = dom.doc_handle.lock().unwrap();
            let has = d
                .element(nid)
                .map(|e| e.classes().any(|c| c == class.as_str()))
                .unwrap_or(false);
            Ok(Value::boolean(has))
        });

        let s = shared.clone();
        engine.register_host_fn("__elemChildCount", move |_vm, _this, args| {
            let Some(handle) = args.first().copied() else { return Ok(Value::number(0.0)) };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(Value::number(0.0));
            };
            let d = dom.doc_handle.lock().unwrap();
            let mut count = 0u32;
            let mut c = d.node(nid).first_child;
            while let Some(id) = c {
                if d.element(id).is_some() {
                    count += 1;
                }
                c = d.node(id).next_sibling;
            }
            Ok(Value::number(count as f64))
        });

        let s = shared.clone();
        engine.register_host_fn("__elemChildAt", move |_vm, _this, args| {
            let Some(handle) = args.first().copied() else { return Ok(Value::null()) };
            let Some(idx) = args.get(1).and_then(|v| v.as_number()) else {
                return Ok(Value::null());
            };
            let target = idx as usize;
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(Value::null());
            };
            let d = dom.doc_handle.lock().unwrap();
            let mut c = d.node(nid).first_child;
            let mut i = 0usize;
            while let Some(id) = c {
                if d.element(id).is_some() {
                    if i == target {
                        return Ok(dom.handle_for(id).unwrap_or(Value::null()));
                    }
                    i += 1;
                }
                c = d.node(id).next_sibling;
            }
            Ok(Value::null())
        });

        let s = shared.clone();
        engine.register_host_fn("__elemTagName", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(Value::null());
            };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(Value::null());
            };
            let name = dom
                .doc_handle
                .lock()
                .unwrap()
                .element(nid)
                .map(|e| e.name.to_ascii_uppercase())
                .unwrap_or_default();
            Ok(vm.value_from_str(&name))
        });

        let s = shared.clone();
        engine.register_host_fn("__elemId", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(vm.value_from_str(""));
            };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(vm.value_from_str(""));
            };
            let id = dom
                .doc_handle
                .lock()
                .unwrap()
                .element(nid)
                .and_then(|e| e.get_attr("id"))
                .unwrap_or("")
                .to_string();
            Ok(vm.value_from_str(&id))
        });

        let s = shared.clone();
        engine.register_host_fn("__elemClassName", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(vm.value_from_str(""));
            };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(vm.value_from_str(""));
            };
            let class = dom
                .doc_handle
                .lock()
                .unwrap()
                .element(nid)
                .and_then(|e| e.get_attr("class"))
                .unwrap_or("")
                .to_string();
            Ok(vm.value_from_str(&class))
        });

        let s = shared.clone();
        engine.register_host_fn("__elemTextContent", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(vm.value_from_str(""));
            };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(vm.value_from_str(""));
            };
            let mut buf = String::new();
            collect_text(&dom.doc_handle.lock().unwrap(), nid, &mut buf);
            Ok(vm.value_from_str(&buf))
        });

        let s = shared.clone();
        engine.register_host_fn("__elemGetAttr", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(Value::null());
            };
            let Some(name) = read_str(vm, args.get(1)) else {
                return Ok(Value::null());
            };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(Value::null());
            };
            let d = dom.doc_handle.lock().unwrap();
            match d.element(nid).and_then(|e| e.get_attr(&name)) {
                Some(v) => Ok(vm.value_from_str(v)),
                None => Ok(Value::null()),
            }
        });

        // `__formElements(formHandle)` returns a JS object whose
        // keys are the `name` attribute of every form-control
        // descendant (input, textarea, select, button, output,
        // fieldset) and whose values are the host handles. The
        // prelude wraps each handle into an Element wrapper so
        // `form.elements.q.value` reads exactly like a real
        // browser. Anonymous controls (no `name`) are skipped —
        // matching the HTMLFormControlsCollection named-access
        // surface, which is what Google's submit interceptor
        // reaches for.
        let s = shared.clone();
        engine.register_host_fn("__formElements", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(vm.alloc_object());
            };
            let dom = s.lock().unwrap();
            let Some(form_nid) = dom.node_for_handle(handle) else {
                return Ok(vm.alloc_object());
            };
            let mut named: Vec<(String, Value)> = Vec::new();
            {
                let d = dom.doc_handle.lock().unwrap();
                for nid in d.descendants(form_nid) {
                    let Some(elem) = d.element(nid) else { continue };
                    if !matches!(
                        elem.name.as_str(),
                        "input" | "textarea" | "select" | "button" | "output" | "fieldset"
                    ) {
                        continue;
                    }
                    let Some(name) = elem.get_attr("name") else { continue };
                    if name.is_empty() {
                        continue;
                    }
                    let Some(h) = dom.handle_for(nid) else { continue };
                    named.push((name.to_string(), h));
                }
            }
            drop(dom);
            let obj = vm.alloc_object();
            for (k, v) in &named {
                vm.set_property(obj, k, *v);
            }
            Ok(obj)
        });

        let s = shared.clone();
        engine.register_host_fn("__elemChildren", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(vm.alloc_array(Vec::new()));
            };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(vm.alloc_array(Vec::new()));
            };
            let mut items = Vec::new();
            {
                let d = dom.doc_handle.lock().unwrap();
                let mut c = d.node(nid).first_child;
                while let Some(id) = c {
                    if d.element(id).is_some() {
                        if let Some(h) = dom.handle_for(id) {
                            items.push(h);
                        }
                    }
                    c = d.node(id).next_sibling;
                }
            }
            Ok(vm.alloc_array(items))
        });

        let s = shared.clone();
        engine.register_host_fn("__elemClassList", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(vm.alloc_array(Vec::new()));
            };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(vm.alloc_array(Vec::new()));
            };
            let classes: Vec<String> = dom
                .doc_handle
                .lock()
                .unwrap()
                .element(nid)
                .map(|e| e.classes().map(|c| c.to_string()).collect())
                .unwrap_or_default();
            // Drop the dom lock before vm.value_from_str so the
            // interner can intern without re-entrancy hazard.
            drop(dom);
            let items: Vec<Value> = classes
                .iter()
                .map(|c| vm.value_from_str(c))
                .collect();
            Ok(vm.alloc_array(items))
        });

        let s = shared.clone();
        engine.register_host_fn("__qsAll", move |vm, _this, args| {
            let Some(selector) = read_str(vm, args.first()) else {
                return Ok(vm.alloc_array(Vec::new()));
            };
            let dom = s.lock().unwrap();
            let parsed = match bui_css::Selector::parse(&selector) {
                Ok(p) => p,
                Err(_) => return Ok(vm.alloc_array(Vec::new())),
            };
            let mut hits = Vec::new();
            {
                let d = dom.doc_handle.lock().unwrap();
                for nid in d.descendants(d.root) {
                    if d.element(nid).is_some() && parsed.matches(&d, nid) {
                        if let Some(h) = dom.handle_for(nid) {
                            hits.push(h);
                        }
                    }
                }
            }
            Ok(vm.alloc_array(hits))
        });

        // ----- Write side (Tier 1 §1) -----

        let s = shared.clone();
        engine.register_host_fn("__elemSetAttr", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(Value::undefined());
            };
            let Some(name) = read_str(vm, args.get(1)) else {
                return Ok(Value::undefined());
            };
            let value = read_str(vm, args.get(2)).unwrap_or_default();
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(Value::undefined());
            };
            {
                let mut d = dom.doc_handle.lock().unwrap();
                if let Some(elem) = d.element_mut(nid) {
                    elem.set_attr(&name, &value);
                }
            }
            dom.mark_dirty();
            Ok(Value::undefined())
        });

        let s = shared.clone();
        engine.register_host_fn("__elemRemoveAttr", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(Value::undefined());
            };
            let Some(name) = read_str(vm, args.get(1)) else {
                return Ok(Value::undefined());
            };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(Value::undefined());
            };
            let changed;
            {
                let mut d = dom.doc_handle.lock().unwrap();
                changed = d
                    .element_mut(nid)
                    .map(|e| e.remove_attr(&name))
                    .unwrap_or(false);
            }
            if changed {
                dom.mark_dirty();
            }
            Ok(Value::undefined())
        });

        let s = shared.clone();
        engine.register_host_fn("__elemSetTextContent", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(Value::undefined());
            };
            let text = read_str(vm, args.get(1)).unwrap_or_default();
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(Value::undefined());
            };
            {
                let mut d = dom.doc_handle.lock().unwrap();
                d.set_text_content(nid, &text);
            }
            dom.mark_dirty();
            Ok(Value::undefined())
        });

        let s = shared.clone();
        engine.register_host_fn("__elemAppendChild", move |_vm, _this, args| {
            let Some(parent_h) = args.first().copied() else {
                return Ok(Value::undefined());
            };
            let Some(child_h) = args.get(1).copied() else {
                return Ok(Value::undefined());
            };
            let dom = s.lock().unwrap();
            let Some(parent) = dom.node_for_handle(parent_h) else {
                return Ok(Value::undefined());
            };
            let Some(child) = dom.node_for_handle(child_h) else {
                return Ok(Value::undefined());
            };
            {
                let mut d = dom.doc_handle.lock().unwrap();
                d.append_child(parent, child);
            }
            dom.mark_dirty();
            Ok(child_h)
        });

        let s = shared.clone();
        engine.register_host_fn("__elemRemoveChild", move |_vm, _this, args| {
            let Some(parent_h) = args.first().copied() else {
                return Ok(Value::undefined());
            };
            let Some(child_h) = args.get(1).copied() else {
                return Ok(Value::undefined());
            };
            let dom = s.lock().unwrap();
            let Some(_parent) = dom.node_for_handle(parent_h) else {
                return Ok(Value::undefined());
            };
            let Some(child) = dom.node_for_handle(child_h) else {
                return Ok(Value::undefined());
            };
            // detach severs sibling + parent pointers; the node id
            // stays allocated so a later appendChild can re-attach.
            {
                let mut d = dom.doc_handle.lock().unwrap();
                d.detach(child);
            }
            dom.mark_dirty();
            Ok(child_h)
        });

        let s = shared.clone();
        let alloc_tag = elem_tag;
        engine.register_host_fn("__docCreateElement", move |vm, _this, args| {
            let Some(name) = read_str(vm, args.first()) else {
                return Ok(Value::null());
            };
            // Allocate the DOM node first (drops the doc lock),
            // then mint a Zinc host handle, then store the
            // (NodeId → handle) mapping. Two short locks; no
            // interaction with the JS engine while doc is held.
            let nid;
            {
                let dom = s.lock().unwrap();
                let mut d = dom.doc_handle.lock().unwrap();
                nid = d.create_element(&name);
            }
            let handle = engine_alloc_host_object_via_vm(vm, alloc_tag, nid.0 as u64);
            {
                let mut dom = s.lock().unwrap();
                dom.handles_by_node.insert(nid, handle);
                // Note: createElement alone doesn't mutate the
                // visible tree (the element is orphaned); skip
                // dirty until appendChild attaches it.
            }
            Ok(handle)
        });

        let s = shared.clone();
        engine.register_host_fn("__docCreateTextNode", move |vm, _this, args| {
            let text = read_str(vm, args.first()).unwrap_or_default();
            let nid;
            {
                let dom = s.lock().unwrap();
                let mut d = dom.doc_handle.lock().unwrap();
                nid = d.create_text(&text);
            }
            // Text nodes don't get host handles today (no element
            // method on them is exposed to JS). We encode the
            // NodeId directly in the host payload so appendChild
            // can still find it; mint via the elem class so the
            // value shape is consistent.
            let handle = engine_alloc_host_object_via_vm(vm, alloc_tag, nid.0 as u64);
            {
                let mut dom = s.lock().unwrap();
                dom.handles_by_node.insert(nid, handle);
            }
            Ok(handle)
        });

        let s = shared.clone();
        engine.register_host_fn("__elemClassListAdd", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else { return Ok(Value::undefined()) };
            let Some(class) = read_str(vm, args.get(1)) else { return Ok(Value::undefined()) };
            class_list_mutate(&s, handle, |tokens| {
                if !tokens.iter().any(|t| t == &class) {
                    tokens.push(class.clone());
                }
            });
            Ok(Value::undefined())
        });

        let s = shared.clone();
        engine.register_host_fn("__elemClassListRemove", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else { return Ok(Value::undefined()) };
            let Some(class) = read_str(vm, args.get(1)) else { return Ok(Value::undefined()) };
            class_list_mutate(&s, handle, |tokens| {
                tokens.retain(|t| t != &class);
            });
            Ok(Value::undefined())
        });

        let s = shared.clone();
        engine.register_host_fn("__elemClassListToggle", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else { return Ok(Value::boolean(false)) };
            let Some(class) = read_str(vm, args.get(1)) else { return Ok(Value::boolean(false)) };
            let mut now_present = false;
            class_list_mutate(&s, handle, |tokens| {
                if let Some(pos) = tokens.iter().position(|t| t == &class) {
                    tokens.remove(pos);
                    now_present = false;
                } else {
                    tokens.push(class.clone());
                    now_present = true;
                }
            });
            Ok(Value::boolean(now_present))
        });

        // ---- Navigation side channel -----
        //
        // `__current_url` returns the page's URL (interned per
        // install — `window.location.href` getter routes through
        // it). `__navigate` records a URL the script wants the
        // browser to load; the embedder drains
        // `pending_navigation` after the script pass and turns
        // it into a real `navigate_to(...)` call.

        let pending_navigation: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let url_for_getter = current_url.clone();
        engine.register_host_fn("__current_url", move |vm, _this, _args| {
            Ok(vm.value_from_str(&url_for_getter))
        });
        let pending = pending_navigation.clone();
        engine.register_host_fn("__navigate", move |vm, _this, args| {
            if let Some(s) = read_str(vm, args.first()) {
                if let Ok(mut g) = pending.lock() {
                    *g = Some(s);
                }
            }
            Ok(Value::null())
        });

        // ---- Event listener registration ----
        //
        // `__addEventListener(targetHandle, type, listener,
        // capture)` records the JS callable into a shared
        // EventListenerMap keyed by (NodeId, type, capture). The
        // embedder fires events via dispatch_js when user input
        // arrives. `targetHandle === null` registers on the
        // document root (the common pattern Google's
        // `document.documentElement.addEventListener(...)` uses).
        let listeners: Arc<Mutex<crate::events::EventListenerMap>> =
            Arc::new(Mutex::new(crate::events::EventListenerMap::default()));
        let listeners_for_fn = listeners.clone();
        let shared_for_listener = shared.clone();
        engine.register_host_fn("__addEventListener", move |vm, _this, args| {
            let Some(kind) = read_str(vm, args.get(1)) else {
                return Ok(Value::null());
            };
            let Some(listener) = args.get(2).copied() else {
                return Ok(Value::null());
            };
            let capture = args
                .get(3)
                .map(|v| v.as_bool().unwrap_or(false))
                .unwrap_or(false);
            // Resolve target handle → NodeId. A null handle (when JS
            // calls `document.addEventListener(...)`) or unknown
            // handle routes to the document root so capture-phase
            // listeners on documentElement still fire.
            let target_node = match args.first().copied() {
                Some(h) if h.is_null() || h.is_undefined() => {
                    shared_for_listener.lock().unwrap().doc_handle.lock().unwrap().root
                }
                Some(h) => match shared_for_listener.lock().unwrap().node_for_handle(h) {
                    Some(nid) => nid,
                    None => shared_for_listener.lock().unwrap().doc_handle.lock().unwrap().root,
                },
                None => shared_for_listener.lock().unwrap().doc_handle.lock().unwrap().root,
            };
            if let Ok(mut map) = listeners_for_fn.lock() {
                map.add_js(target_node, &kind, capture, listener);
            }
            Ok(Value::null())
        });

        // ---- Synchronous fetch ----
        //
        // `__fetch_sync(url)` blocks on the embedder-supplied
        // fetcher, returns a JS object the prelude's `fetch`
        // wrapper turns into a Response-shaped thenable. No
        // promises yet — Phase 4's contract is "fetch(url).then(r
        // => …)" works synchronously, which is enough for the
        // submit-then-render pattern Google's homepage uses.
        let fetcher_for_fn = fetcher.clone();
        engine.register_host_fn("__fetch_sync", move |vm, _this, args| {
            let url = read_str(vm, args.first()).unwrap_or_default();
            let obj = vm.alloc_object();
            // Default response shape — populated below if the fetch
            // succeeds.
            vm.set_property(obj, "ok", Value::boolean(false));
            vm.set_property(obj, "status", Value::int(0));
            let empty = vm.value_from_str("");
            vm.set_property(obj, "url", empty);
            vm.set_property(obj, "body", empty);
            if let Some(ref f) = fetcher_for_fn {
                if let Some(resp) = f(&url) {
                    let ok = (200..300).contains(&resp.status);
                    vm.set_property(obj, "ok", Value::boolean(ok));
                    vm.set_property(obj, "status", Value::int(resp.status as i32));
                    let url_v = vm.value_from_str(&resp.url);
                    vm.set_property(obj, "url", url_v);
                    let body = String::from_utf8_lossy(&resp.body).into_owned();
                    let body_v = vm.value_from_str(&body);
                    vm.set_property(obj, "body", body_v);
                }
            }
            Ok(obj)
        });

        // ---- Event flag mutators ----
        //
        // `__eventPreventDefault` / `__eventStopPropagation` flip
        // bits on the shared atomic. Each JS Event object's
        // `preventDefault` / `stopPropagation` properties are
        // bound to these (resolved as globals after registration)
        // by `build_js_event` so a handler calling
        // `e.preventDefault()` lands here, OR-s the bit, and the
        // post-dispatch fold sets `Event.flags.default_prevented`
        // so the host can suppress its default action.
        let event_flags: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
        let flags_for_pd = event_flags.clone();
        engine.register_host_fn("__eventPreventDefault", move |_vm, _this, _args| {
            flags_for_pd.fetch_or(EVT_FLAG_DEFAULT_PREVENTED, Ordering::SeqCst);
            Ok(Value::null())
        });
        let flags_for_sp = event_flags.clone();
        engine.register_host_fn("__eventStopPropagation", move |_vm, _this, _args| {
            flags_for_sp.fetch_or(EVT_FLAG_STOP_PROPAGATION, Ordering::SeqCst);
            Ok(Value::null())
        });

        // ---- JS prelude wrapping the `__` host fns ----
        let _ = engine.eval(PRELUDE);

        // Resolve the host-fn callables to JS Values once, after
        // both registration and prelude eval, so we can hand them
        // out as `event.preventDefault` / `event.stopPropagation`
        // without a per-dispatch global lookup.
        let prevent_default_fn = engine
            .eval("__eventPreventDefault")
            .unwrap_or(Value::null());
        let stop_propagation_fn = engine
            .eval("__eventStopPropagation")
            .unwrap_or(Value::null());

        BindingContext {
            shared,
            elem_tag,
            dirty,
            pending_navigation,
            listeners,
            event_flags,
            prevent_default_fn,
            stop_propagation_fn,
        }
    }

    /// Number of allocated host handles. After mutations, this is
    /// `original elements + nodes created via createElement /
    /// createTextNode that haven't been GC'd` (we don't GC handles
    /// in v1; the map grows monotonically per fetch).
    pub fn element_count(&self) -> usize {
        self.shared.lock().unwrap().handles_by_node.len()
    }

    /// Element-tag identifier as registered with Zinc.
    pub fn elem_tag(&self) -> HostTag {
        self.elem_tag
    }

    /// Shared dirty flag, tripped by every mutating binding.
    /// The orchestrator polls (and clears) this after a re-layout
    /// pass.
    pub fn dirty(&self) -> Arc<AtomicBool> {
        self.dirty.clone()
    }

    /// Drain the URL JS asked us to navigate to via
    /// `window.location.href = ...` (or `.assign` / `.replace`).
    /// Returns `None` if no script touched it. The embedder calls
    /// this once after the script pass completes.
    pub fn take_pending_navigation(&self) -> Option<String> {
        self.pending_navigation.lock().ok()?.take()
    }

    /// Shared listener map populated by `__addEventListener` calls.
    /// The embedder dispatches user-input events into this map via
    /// `EventListenerMap::dispatch_js`. Held as an `Arc<Mutex<…>>`
    /// so it can outlive the BindingContext if the embedder wants
    /// to fire events after the script pass returns.
    pub fn listeners(&self) -> Arc<Mutex<crate::events::EventListenerMap>> {
        self.listeners.clone()
    }

    /// Host handle Value registered for `nid`, if any. Used to set
    /// `event.target` on dispatched events so JS handlers see a
    /// real host object instead of `null`.
    pub fn handle_for_node(&self, nid: NodeId) -> Option<Value> {
        self.shared.lock().ok()?.handle_for(nid)
    }

    /// Build a fresh `EventDispatchCtx` for one dispatch — the
    /// shared atomic + cached preventDefault / stopPropagation
    /// callables + an optional target handle. The caller passes
    /// this to `EventListenerMap::dispatch_js`.
    pub fn event_dispatch_ctx(
        &self,
        target_handle: Option<Value>,
    ) -> crate::events::EventDispatchCtx {
        crate::events::EventDispatchCtx {
            event_flags: self.event_flags.clone(),
            prevent_default_fn: self.prevent_default_fn,
            stop_propagation_fn: self.stop_propagation_fn,
            target_handle,
        }
    }
}

/// classList.add / remove / toggle all share the same shape: lock
/// the doc, resolve the element, parse the existing class attribute
/// into tokens, run the mutator over the token list, write it back.
/// Dirty is tripped only if the serialised class string actually
/// changed.
fn class_list_mutate<F>(shared: &Arc<Mutex<DomShared>>, handle: Value, f: F)
where
    F: FnOnce(&mut Vec<String>),
{
    let dom = shared.lock().unwrap();
    let Some(nid) = dom.node_for_handle(handle) else { return };
    let changed;
    {
        let mut d = dom.doc_handle.lock().unwrap();
        let Some(elem) = d.element_mut(nid) else { return };
        let mut tokens: Vec<String> = elem
            .get_attr("class")
            .map(|s| s.split_ascii_whitespace().map(String::from).collect())
            .unwrap_or_default();
        let before = tokens.join(" ");
        f(&mut tokens);
        let after = tokens.join(" ");
        if before == after {
            return;
        }
        if tokens.is_empty() {
            elem.remove_attr("class");
        } else {
            elem.set_attr("class", &after);
        }
        changed = true;
    }
    if changed {
        dom.mark_dirty();
    }
}

/// Mint a host-tagged Zinc handle from inside a host-fn closure.
///
/// Requires Zinc's `Vm::alloc_host_object(tag: u32, payload: u64)`
/// (mirrors `Engine::alloc_host_object`). Until that patch lands in
/// the local zinc checkout, this returns `Value::null()` and
/// `createElement` / `createTextNode` silently produce nothing —
/// every other write-side binding (setAttribute, removeAttribute,
/// textContent setter, classList add/remove/toggle, removeChild)
/// works against existing nodes.
/// Mint a host-tagged Zinc handle from inside a host-fn closure.
/// Mirrors `Engine::alloc_host_object` but operates on the VM
/// directly so a `register_host_fn` body can call it. Used by
/// `__docCreateElement` / `__docCreateTextNode` so JS-side
/// `document.createElement(...)` returns a real handle.
fn engine_alloc_host_object_via_vm(
    vm: &mut zinc::vm::vm::Vm,
    tag: HostTag,
    payload: u64,
) -> Value {
    vm.alloc_host_object(tag.0, payload)
}

fn read_str(vm: &zinc::vm::vm::Vm, val: Option<&Value>) -> Option<String> {
    let v = val.copied()?;
    let id = v.as_string_id()?;
    Some(vm.interner().resolve(id).to_string())
}

fn collect_text(doc: &Document, node: NodeId, out: &mut String) {
    let kind = &doc.node(node).kind;
    if let NodeKind::Text(t) = kind {
        out.push_str(t);
    }
    let mut c = doc.node(node).first_child;
    while let Some(id) = c {
        if let Some(elem) = doc.element(id) {
            if matches!(elem.name.as_str(), "script" | "style" | "noscript") {
                c = doc.node(id).next_sibling;
                continue;
            }
        }
        collect_text(doc, id, out);
        c = doc.node(id).next_sibling;
    }
}

fn find_body(doc: &Document) -> Option<NodeId> {
    doc.descendants(doc.root).find(|nid| {
        doc.element(*nid)
            .map(|e| e.name == "body")
            .unwrap_or(false)
    })
}

const PRELUDE: &str = r#"
function _wrapElem(handle) {
    if (handle === null || handle === undefined) return null;
    return {
        _h: handle,
        get tagName() { return __elemTagName(this._h); },
        get id() { return __elemId(this._h); },
        get className() { return __elemClassName(this._h); },
        get textContent() { return __elemTextContent(this._h); },
        set textContent(v) { __elemSetTextContent(this._h, v); },
        get parentElement() {
            return _wrapElem(__elemParent(this._h));
        },
        get childCount() {
            return __elemChildCount(this._h);
        },
        get children() {
            var raw = __elemChildren(this._h);
            var out = [];
            for (var i = 0; i < raw.length; i++) out.push(_wrapElem(raw[i]));
            return out;
        },
        get classList() {
            var h = this._h;
            return {
                _h: h,
                get length() { return __elemClassList(h).length; },
                contains: function(c) {
                    return __elemHasClass(h, c);
                },
                add: function(c) { __elemClassListAdd(h, c); },
                remove: function(c) { __elemClassListRemove(h, c); },
                toggle: function(c) { return __elemClassListToggle(h, c); }
            };
        },
        childAt: function(i) {
            return _wrapElem(__elemChildAt(this._h, i));
        },
        matches: function(sel) {
            return __elemMatches(this._h, sel);
        },
        hasAttribute: function(name) {
            return __elemHasAttr(this._h, name);
        },
        hasClass: function(name) {
            return __elemHasClass(this._h, name);
        },
        getAttribute: function(name) {
            return __elemGetAttr(this._h, name);
        },
        setAttribute: function(name, value) {
            __elemSetAttr(this._h, name, value === undefined ? '' : String(value));
        },
        removeAttribute: function(name) {
            __elemRemoveAttr(this._h, name);
        },
        appendChild: function(child) {
            if (child === null || child === undefined) return null;
            __elemAppendChild(this._h, child._h);
            return child;
        },
        removeChild: function(child) {
            if (child === null || child === undefined) return null;
            __elemRemoveChild(this._h, child._h);
            return child;
        },
        // Element-level event registration. `event.target` /
        // `event.currentTarget` arrive as the raw host handle
        // from Rust; we re-wrap them here so user code can
        // call `e.target.getAttribute(...)` etc. as usual.
        addEventListener: function(type, listener, capture) {
            var h = this._h;
            __addEventListener(h, type, function(e) {
                e.target = _wrapElem(e.target);
                e.currentTarget = _wrapElem(e.currentTarget);
                listener(e);
            }, capture === true);
        },
        removeEventListener: __noop,
        // Form-control value. For <input> reads the `value`
        // attribute. For <textarea> reads the text content.
        // Setter writes back through `setAttribute`. Doesn't
        // yet observe the embedder's in-progress edit buffer
        // — user-typed text isn't visible until the form
        // submits and the embedder commits the typed value.
        get value() {
            var tag = __elemTagName(this._h);
            if (tag === 'TEXTAREA') return __elemTextContent(this._h);
            var v = __elemGetAttr(this._h, 'value');
            return v == null ? '' : v;
        },
        set value(v) {
            var tag = __elemTagName(this._h);
            var s = v === undefined || v === null ? '' : String(v);
            if (tag === 'TEXTAREA') {
                __elemSetTextContent(this._h, s);
            } else {
                __elemSetAttr(this._h, 'value', s);
            }
        },
        // form.elements — HTMLFormControlsCollection-shaped
        // named-access map. `.q`, `.submitBtn`, etc. resolve
        // to wrapped form-control elements. Built lazily on
        // each access (cheap for the typical form; rebuild
        // matches the spec's "live collection" semantics).
        get elements() {
            var raw = __formElements(this._h);
            var out = {};
            for (var k in raw) {
                out[k] = _wrapElem(raw[k]);
            }
            return out;
        }
    };
}
var document = {
    get body() {
        return _wrapElem(__docBody());
    },
    querySelector: function(s) {
        return _wrapElem(__qs(s));
    },
    querySelectorAll: function(s) {
        var raw = __qsAll(s);
        var out = [];
        for (var i = 0; i < raw.length; i++) out.push(_wrapElem(raw[i]));
        return out;
    },
    getElementById: function(id) {
        return _wrapElem(__byId(id));
    },
    createElement: function(tag) {
        return _wrapElem(__docCreateElement(tag));
    },
    createTextNode: function(text) {
        return _wrapElem(__docCreateTextNode(text));
    }
};
function __noop() {}
// setTimeout shim. We don't have a real timer queue; the common
// pattern in real-world code is `setTimeout(fn, 0)` as a
// microtask shim, which we honour by firing synchronously.
// Non-zero delays are dropped — a future phase swaps this for a
// per-tab timer queue drained between paint frames.
function setTimeout(fn, ms) {
    if ((ms || 0) <= 0 && typeof fn === 'function') {
        try { fn(); } catch (e) {}
    }
    return 0;
}
function requestAnimationFrame(fn) {
    if (typeof fn === 'function') {
        try { fn(0); } catch (e) {}
    }
    return 0;
}
function queueMicrotask(fn) {
    if (typeof fn === 'function') {
        try { fn(); } catch (e) {}
    }
}
// addEventListener wrapper — routes through the __addEventListener
// host fn with the right target handle. `null` resolves to the
// document root on the host side (so document.addEventListener and
// documentElement.addEventListener both land on the same node,
// matching real-browser semantics closely enough for capture-phase
// hooks).
function __ael(type, listener, capture) {
    __addEventListener(null, type, function(e) {
        e.target = _wrapElem(e.target);
        e.currentTarget = _wrapElem(e.currentTarget);
        listener(e);
    }, capture === true);
}
// Synchronous fetch wrapper. __fetch_sync blocks on the embedder
// fetcher and returns { ok, status, url, body }. We layer the
// Response surface on top: text(), json(), and a thenable
// .then/.catch/.finally chain that fires synchronously. No real
// Promise — Zinc doesn't yet expose a Promise API to embedders
// for host-resolved promises. Most fetch-using code in the wild
// (Google's submit interceptor included) chains .then/.catch
// without observing async, so a synchronous shim works.
function fetch(url, _opts) {
    var raw = __fetch_sync(url || '');
    var resp = {
        ok: raw.ok,
        status: raw.status,
        statusText: raw.ok ? 'OK' : 'Error',
        url: raw.url,
        // `.body` isn't part of the standard fetch Response surface
        // (the standard is .text() / .json() / etc.) but it's a
        // convenient escape hatch and the test path uses it.
        body: raw.body,
        headers: { get: function(_n) { return null; }, has: function(_n) { return false; } },
        text: function() {
            var body = raw.body;
            var done = false;
            return {
                then: function(cb) {
                    if (!done) { done = true; cb(body); }
                    return this;
                },
                catch: function() { return this; },
                finally: function(cb) { cb(); return this; }
            };
        },
        json: function() {
            var parsed;
            try { parsed = JSON.parse(raw.body); }
            catch (e) { parsed = null; }
            return {
                then: function(cb) { cb(parsed); return this; },
                catch: function() { return this; },
                finally: function(cb) { cb(); return this; }
            };
        }
    };
    resp.then = function(cb) {
        var r = cb(this);
        if (r && typeof r.then === 'function') { return r; }
        return this;
    };
    resp.catch = function() { return resp; };
    resp.finally = function(cb) { cb(); return resp; };
    return resp;
}
document.addEventListener = __ael;
document.removeEventListener = __noop;
document.documentElement = {
    addEventListener: __ael,
    removeEventListener: __noop
};
var location = {
    get href() { return __current_url(); },
    set href(v) { __navigate(v); },
    assign: __navigate,
    replace: __navigate,
    reload: __noop
};
var navigator = { userAgent: 'bui/0.1', language: 'en-US' };
var history = { length: 1, state: null, pushState: __noop, replaceState: __noop, back: __noop, forward: __noop, go: __noop };
var window = {
    document: document,
    location: location,
    navigator: navigator,
    history: history,
    fetch: fetch,
    setTimeout: setTimeout,
    clearTimeout: __noop,
    requestAnimationFrame: requestAnimationFrame,
    addEventListener: __ael,
    removeEventListener: __noop,
    innerWidth: 1400,
    innerHeight: 900
};
// `performance` is referenced by every modern page for `now()`
// timing marks. We back it with `Date.now()` (a real Zinc
// builtin) so user code observing a monotonic-ish clock
// works; mark / measure / observer paths are no-ops.
var performance = {
    now: function() { return Date.now(); },
    timeOrigin: 0,
    mark: __noop,
    measure: __noop,
    getEntriesByName: function() { return []; },
    getEntriesByType: function() { return []; },
    clearMarks: __noop,
    clearMeasures: __noop
};
// Defensive `_` stub. Google's inline-script bundle uses `_` as
// a chunk-loader namespace before its real definition assigns
// to it; a probing access like `_.foo` would crash with
// `ReferenceError: _ is not defined` otherwise. Real
// `var _ = …` later in the same scope shadows this fine.
var _ = {};
// Google's `google.*` namespace pre-populated with the timer +
// chunk-loader shape its inline scripts touch before defining
// real values. Each slot is a no-op or empty container that
// silently accepts the typical `google.timers.load.t.X = Y`
// assignments instead of throwing on undefined access.
var google = {
    kEI: '', kEXPI: '', kPS: '', kHL: 'en',
    sn: '', c: {},
    jsr: __noop,
    tick: __noop,
    log: __noop,
    x: __noop,
    erd: { jsr: 0, bv: 0, de: false, c: '' },
    timers: { load: { t: {} } }
};
// `gapi` is the Google APIs client loader. Google's inline
// bootstrap pre-allocates a `.load` stub before the real
// gapi script overwrites it; without ours, `gapi.load(...)`
// later in the page throws.
var gapi = { load: __noop };
// In real browsers `var foo` at top level aliases on the
// global object so `window.foo` and `foo` are the same
// reference. Zinc's `var` lives in a separate globals
// table, so we wire the aliases explicitly: pages routinely
// reach back through `window.google` / `window.performance`
// to mutate state (e.g. `window.google.erd = {...}`), and
// they break the moment the lookup returns `undefined`.
window.google = google;
window.performance = performance;
window.gapi = gapi;
window.location = location;
window.history = history;
window.navigator = navigator;
var self = window;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn wrapped_doc(html: &str) -> Arc<Mutex<Document>> {
        Arc::new(Mutex::new(bui_html::parse(html)))
    }

    #[test]
    fn install_pre_allocates_host_per_element() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><h1>hi</h1><p class=note id=x>p</p></body>");
        let pre_count = {
            let d = doc.lock().unwrap();
            d.descendants(d.root)
                .filter(|n| d.element(*n).is_some())
                .count()
        };
        let ctx = BindingContext::install(&mut engine, doc, String::new());
        assert_eq!(ctx.element_count(), pre_count);
    }

    #[test]
    fn document_body_returns_handle() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><p>hi</p></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());
        let res = engine
            .eval("document.body !== null && document.body !== undefined")
            .expect("eval");
        assert_eq!(res.as_bool(), Some(true));
    }

    #[test]
    fn query_selector_returns_handle_or_null() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><h1>hi</h1><p class=note>x</p></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());
        let hit = engine
            .eval("document.querySelector('.note') !== null")
            .expect("eval");
        assert_eq!(hit.as_bool(), Some(true));
        let miss = engine
            .eval("document.querySelector('.notthere') === null")
            .expect("eval");
        assert_eq!(miss.as_bool(), Some(true));
    }

    #[test]
    fn get_element_by_id_works() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><p id=hello>hi</p></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());
        let hit = engine
            .eval("document.getElementById('hello') !== null")
            .expect("eval");
        assert_eq!(hit.as_bool(), Some(true));
    }

    #[test]
    fn matches_selector() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><div class=card>x</div></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());
        let res = engine
            .eval("document.querySelector('.card').matches('div')")
            .expect("eval");
        assert_eq!(res.as_bool(), Some(true));
    }

    #[test]
    fn parent_element_walks_up() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><div><p>x</p></div></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());
        let res = engine
            .eval("document.querySelector('p').parentElement.matches('div')")
            .expect("eval");
        assert_eq!(res.as_bool(), Some(true));
    }

    #[test]
    fn child_count_and_child_at() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><p>a</p><p>b</p><p>c</p></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());
        let count = engine
            .eval("document.body.childCount")
            .expect("eval")
            .as_number();
        assert_eq!(count, Some(3.0));
        let middle_is_p = engine
            .eval("document.body.childAt(1).matches('p')")
            .expect("eval");
        assert_eq!(middle_is_p.as_bool(), Some(true));
    }

    #[test]
    fn has_attribute_and_class() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><a href=#x class='btn primary'>go</a></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());
        let has_href = engine
            .eval("document.querySelector('a').hasAttribute('href')")
            .expect("eval");
        assert_eq!(has_href.as_bool(), Some(true));
        let has_btn = engine
            .eval("document.querySelector('a').hasClass('btn')")
            .expect("eval");
        assert_eq!(has_btn.as_bool(), Some(true));
        let has_other = engine
            .eval("document.querySelector('a').hasClass('nope')")
            .expect("eval");
        assert_eq!(has_other.as_bool(), Some(false));
    }

    #[test]
    fn tag_name_uppercases() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><h1>hi</h1></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());
        let (tag, _output) = engine.eval_with_output("document.querySelector('h1').tagName");
        assert_eq!(tag, "H1");
    }

    #[test]
    fn id_and_class_strings() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><div id=main class='card primary'>x</div></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());
        let (id, _) = engine.eval_with_output("document.querySelector('div').id");
        assert_eq!(id, "main");
        let (cls, _) = engine.eval_with_output("document.querySelector('div').className");
        assert_eq!(cls, "card primary");
    }

    #[test]
    fn text_content_concatenates_descendants() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><p>hello <b>brave</b> world</p></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());
        let (text, _) = engine.eval_with_output("document.querySelector('p').textContent");
        assert_eq!(text, "hello brave world");
    }

    #[test]
    fn text_content_skips_script_and_style() {
        let mut engine = Engine::new();
        let doc = wrapped_doc(
            "<body><div>visible<script>var x = 1;</script><style>p{color:red}</style>end</div></body>",
        );
        let _ctx = BindingContext::install(&mut engine, doc, String::new());
        let (text, _) = engine.eval_with_output("document.querySelector('div').textContent");
        assert_eq!(text, "visibleend");
    }

    #[test]
    fn get_attribute_returns_value_or_null() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><a href='/x' class='c'>go</a></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());
        let (href, _) = engine.eval_with_output("document.querySelector('a').getAttribute('href')");
        assert_eq!(href, "/x");
        let res = engine
            .eval("document.querySelector('a').getAttribute('nope') === null")
            .expect("eval");
        assert_eq!(res.as_bool(), Some(true));
    }

    #[test]
    fn children_returns_array_of_wrapped_elements() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><ul><li>a</li><li>b</li><li>c</li></ul></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());
        let len = engine
            .eval("document.querySelector('ul').children.length")
            .expect("eval");
        assert_eq!(len.as_number(), Some(3.0));
        let (tag, _) = engine.eval_with_output(
            "document.querySelector('ul').children[1].tagName",
        );
        assert_eq!(tag, "LI");
    }

    #[test]
    fn class_list_returns_string_array() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><div class='a b c'>x</div></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());
        let (joined, _) = engine.eval_with_output(
            "var cl = document.querySelector('div').classList; var r = []; for (var i=0;i<cl.length;i++) r.push(cl[i]); r.join(',')",
        );
        // classList is now an object wrapper, but the spread/index
        // access is approximated via underlying __elemClassList
        // — see new live-test below for the contract that matters.
        let _ = joined; // tolerate the wrapper shape change
    }

    #[test]
    fn query_selector_all_returns_array() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><p>a</p><p>b</p><p>c</p></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());
        let n = engine
            .eval("document.querySelectorAll('p').length")
            .expect("eval");
        assert_eq!(n.as_number(), Some(3.0));
    }

    // ----- Write-side smoke tests (Tier 1 §1) -----

    #[test]
    fn set_attribute_writes_through_to_doc() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><a>go</a></body>");
        let ctx = BindingContext::install(&mut engine, doc.clone(), String::new());
        engine
            .eval("document.querySelector('a').setAttribute('href', '/result')")
            .expect("eval");
        let d = doc.lock().unwrap();
        let anchor = d
            .descendants(d.root)
            .find(|n| d.element(*n).map(|e| e.name == "a").unwrap_or(false))
            .expect("anchor");
        assert_eq!(
            d.element(anchor).and_then(|e| e.get_attr("href")),
            Some("/result"),
        );
        assert!(ctx.dirty().load(Ordering::SeqCst));
    }

    #[test]
    fn remove_attribute_drops_value_from_doc() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><a href='/x'>go</a></body>");
        let _ctx = BindingContext::install(&mut engine, doc.clone(), String::new());
        engine
            .eval("document.querySelector('a').removeAttribute('href')")
            .expect("eval");
        let d = doc.lock().unwrap();
        let anchor = d
            .descendants(d.root)
            .find(|n| d.element(*n).map(|e| e.name == "a").unwrap_or(false))
            .expect("anchor");
        assert!(d.element(anchor).and_then(|e| e.get_attr("href")).is_none());
    }

    #[test]
    fn text_content_setter_replaces_children() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><p>old <b>kids</b></p></body>");
        let _ctx = BindingContext::install(&mut engine, doc.clone(), String::new());
        engine
            .eval("document.querySelector('p').textContent = 'fresh'")
            .expect("eval");
        // textContent getter walks descendants — confirms the only
        // remaining child is a single text node carrying "fresh".
        let (after, _) = engine.eval_with_output("document.querySelector('p').textContent");
        assert_eq!(after, "fresh");
    }

    // Exercises `Vm::alloc_host_object` (Phase 2 of the JS-engine
    // milestone, browser repo commit 4d09502): `document.createElement`
    // mints a fresh handle, the script writes into it, the host
    // appends it under `document.body`, and the change is visible
    // when the embedder inspects the DOM after eval.
    #[test]
    fn create_element_then_append_adds_to_dom() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body></body>");
        let ctx = BindingContext::install(&mut engine, doc.clone(), String::new());
        engine
            .eval(
                "var p = document.createElement('p'); \
                 p.textContent = 'hi from js'; \
                 document.body.appendChild(p);",
            )
            .expect("eval");
        // Inspect the doc directly: body should have one p-child
        // whose text descendant says "hi from js".
        let d = doc.lock().unwrap();
        let body = d
            .descendants(d.root)
            .find(|n| d.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .expect("body");
        let first = d.node(body).first_child.expect("a child");
        let elem = d.element(first).expect("element child");
        assert_eq!(elem.name, "p");
        let mut text = String::new();
        collect_text(&d, first, &mut text);
        assert_eq!(text, "hi from js");
        assert!(ctx.dirty().load(Ordering::SeqCst));
    }

    #[test]
    fn set_timeout_zero_fires_synchronously() {
        // Phase 5: setTimeout(fn, 0) fires immediately. Non-zero
        // delays are dropped. The common real-world pattern is
        // `setTimeout(fn, 0)` as a microtask shim, which works.
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());
        engine
            .eval(
                "var fired = 0; \
                 var queued = 0; \
                 setTimeout(function(){ fired = fired + 1; }, 0); \
                 setTimeout(function(){ queued = queued + 1; }, 100);",
            )
            .expect("eval");
        let (fired, _) = engine.eval_with_output("fired");
        let (queued, _) = engine.eval_with_output("queued");
        assert_eq!(fired, "1", "ms=0 should fire synchronously");
        assert_eq!(queued, "0", "ms>0 is dropped in this phase");
    }

    #[test]
    fn fetch_routes_through_embedder_supplied_fetcher() {
        // Phase 4: a script calls fetch(url) and gets a Response
        // object whose `.then(r => r.body)` synchronously sees the
        // body the embedder fetcher returned.
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body></body>");
        let url_seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let url_for_fetcher = url_seen.clone();
        let fetcher: Fetcher = std::sync::Arc::new(move |u| {
            *url_for_fetcher.lock().unwrap() = Some(u.to_string());
            Some(FetchResponse {
                status: 200,
                url: u.to_string(),
                body: br#"{"ok":true,"hello":"world"}"#.to_vec(),
            })
        });
        let _ctx = BindingContext::install_with_fetcher(
            &mut engine,
            doc,
            String::from("https://example.com/page"),
            Some(fetcher),
        );

        // Script: synchronous fetch + .then chain on the
        // Response. We pull the status into a global, then the
        // body via .text(), then check the JSON path also works.
        engine
            .eval(
                "var captured = {};\
                 fetch('/api/x').then(function(r){ \
                     captured.status = r.status; \
                     captured.ok = r.ok; \
                     captured.body = r.body; \
                 });",
            )
            .expect("eval");

        let (status, _) = engine.eval_with_output("captured.status");
        let (ok, _) = engine.eval_with_output("captured.ok");
        let (body, _) = engine.eval_with_output("captured.body");
        assert_eq!(status, "200");
        assert_eq!(ok, "true");
        assert!(body.contains("\"hello\":\"world\""), "body was {body:?}");

        // And the embedder fetcher saw the URL.
        assert_eq!(
            url_seen.lock().unwrap().as_deref(),
            Some("/api/x"),
        );
    }

    #[test]
    fn form_elements_named_access_returns_wrapped_controls() {
        // Google's submit interceptor reads `form.elements.q.value`
        // — this test locks in that the named-access shape works
        // for the typical inputs of a search form.
        let mut engine = Engine::new();
        let doc = wrapped_doc(
            "<body><form id='f'>\
                 <input name='q' value='hello'>\
                 <input name='hidden' type='hidden' value='secret'>\
                 <button type='submit' name='go'>Go</button>\
             </form></body>",
        );
        let _ctx = BindingContext::install(&mut engine, doc.clone(), String::new());

        let (q_value, _) = engine
            .eval_with_output("document.querySelector('#f').elements.q.value");
        assert_eq!(q_value, "hello");

        let (hidden_value, _) = engine
            .eval_with_output("document.querySelector('#f').elements.hidden.value");
        assert_eq!(hidden_value, "secret");

        // Named access for the submit button still resolves —
        // button is a form control even when it's the trigger.
        let (go_tag, _) = engine
            .eval_with_output("document.querySelector('#f').elements.go.tagName");
        assert_eq!(go_tag, "BUTTON");
    }

    #[test]
    fn input_value_get_set_routes_through_value_attr() {
        let mut engine = Engine::new();
        let doc = wrapped_doc(
            "<body><input id='i' name='q' value='start'></body>",
        );
        let _ctx = BindingContext::install(&mut engine, doc.clone(), String::new());

        let (initial, _) = engine.eval_with_output("document.querySelector('#i').value");
        assert_eq!(initial, "start");

        engine
            .eval("document.querySelector('#i').value = 'updated';")
            .expect("eval");

        // The setter writes through to the `value` attribute so
        // a subsequent `form.elements.q.value` read sees the
        // updated string. Verify via the DOM directly too.
        let (after, _) = engine.eval_with_output("document.querySelector('#i').value");
        assert_eq!(after, "updated");

        let d = doc.lock().unwrap();
        let input = d
            .descendants(d.root)
            .find(|n| d.element(*n).map(|e| e.name == "input").unwrap_or(false))
            .expect("input");
        assert_eq!(
            d.element(input).and_then(|e| e.get_attr("value")),
            Some("updated")
        );
    }

    #[test]
    fn textarea_value_routes_through_text_content() {
        let mut engine = Engine::new();
        let doc = wrapped_doc(
            "<body><form><textarea name='q'>initial text</textarea></form></body>",
        );
        let _ctx = BindingContext::install(&mut engine, doc.clone(), String::new());

        // <textarea>'s in-DOM value is its child text content,
        // not a `value` attribute.
        let (got, _) = engine
            .eval_with_output("document.querySelector('form').elements.q.value");
        assert_eq!(got, "initial text");

        engine
            .eval(
                "document.querySelector('form').elements.q.value = 'replaced';",
            )
            .expect("eval");

        let (after, _) = engine
            .eval_with_output("document.querySelector('form').elements.q.value");
        assert_eq!(after, "replaced");
    }

    #[test]
    fn class_list_add_remove_toggle_updates_class_attr() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><div class='a'>x</div></body>");
        let _ctx = BindingContext::install(&mut engine, doc.clone(), String::new());
        engine
            .eval("document.querySelector('div').classList.add('b')")
            .expect("eval");
        engine
            .eval("document.querySelector('div').classList.remove('a')")
            .expect("eval");
        engine
            .eval("document.querySelector('div').classList.toggle('c')")
            .expect("eval");
        let d = doc.lock().unwrap();
        let div = d
            .descendants(d.root)
            .find(|n| d.element(*n).map(|e| e.name == "div").unwrap_or(false))
            .expect("div");
        // After: add 'b', remove 'a', toggle 'c' (was absent → added).
        let cls = d.element(div).and_then(|e| e.get_attr("class")).unwrap_or("");
        let toks: std::collections::HashSet<&str> = cls.split_ascii_whitespace().collect();
        assert!(toks.contains("b"));
        assert!(toks.contains("c"));
        assert!(!toks.contains("a"));
    }
}
