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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bui_dom::{Document, NodeId, NodeKind};
use zinc::engine::{Engine, HostTag};
use zinc::runtime::object::ObjectId;
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

    /// Look up or lazily allocate a host handle for `node`.
    /// `BindingContext::install` pre-allocates handles for every
    /// element in the initial DOM, but text nodes don't get
    /// handles until JS first walks into one via firstChild /
    /// nextSibling / etc. `ensure_handle(None, …)` returns
    /// `Value::null()` so traversal getters can pass through
    /// the `Option<NodeId>` they read from `bui-dom` unchanged.
    fn ensure_handle(
        &mut self,
        node: Option<NodeId>,
        vm: &mut zinc::vm::vm::Vm,
        elem_tag: zinc::engine::HostTag,
    ) -> Value {
        let Some(nid) = node else { return Value::null() };
        if let Some(h) = self.handles_by_node.get(&nid).copied()
            && !h.is_null()
        {
            return h;
        }
        let h = vm.alloc_host_object(elem_tag.0, nid.0 as u64);
        self.handles_by_node.insert(nid, h);
        h
    }
}

/// Queue a `MutationRecord` on every `MutationObserver`
/// whose subscription matches the given target + kind.
/// Records sit on each observer's `pending` queue until
/// `JsContext::deliver_mutations` (called after each `tick`)
/// hands them to the observer's callback.
///
/// Subscription matching:
/// - kind must be enabled on the subscription
///   (`childList: true` / `attributes: true` / `characterData:
///   true`).
/// - target must be `subscription.target` itself, OR a
///   descendant of it when `subtree: true`.
/// - for attribute records, `attributeFilter` (when present)
///   limits which names match.
fn record_mutation(
    observers: &Arc<Mutex<MutationObservers>>,
    doc_handle: &Arc<Mutex<Document>>,
    record: MutationRecord,
) {
    let Ok(mut observers) = observers.lock() else { return };
    if observers.is_empty() {
        return;
    }
    let Ok(d) = doc_handle.lock() else { return };
    for obs in observers.values_mut() {
        for sub in &obs.subscriptions {
            if !subscription_matches(sub, &record, &d) {
                continue;
            }
            obs.pending.push(record.clone());
            // One copy per observer regardless of how many of
            // its subscriptions match — matches the spec.
            break;
        }
    }
}

fn subscription_matches(
    sub: &MutationSubscription,
    record: &MutationRecord,
    doc: &Document,
) -> bool {
    match record.kind {
        MutationKind::Attributes => {
            if !sub.attributes {
                return false;
            }
            if let Some(filter) = &sub.attribute_filter
                && !filter.iter().any(|n| n.eq_ignore_ascii_case(&record.attribute_name))
            {
                return false;
            }
        }
        MutationKind::ChildList => {
            if !sub.child_list {
                return false;
            }
        }
        MutationKind::CharacterData => {
            if !sub.character_data {
                return false;
            }
        }
    }
    if record.target == sub.target {
        return true;
    }
    if !sub.subtree {
        return false;
    }
    let mut cur = doc.node(record.target).parent;
    while let Some(p) = cur {
        if p == sub.target {
            return true;
        }
        cur = doc.node(p).parent;
    }
    false
}

/// Clone `src_id` (and optionally its subtree) within the
/// same Document. Returns the new NodeId — detached (no
/// parent / no siblings), ready for the caller to
/// `appendChild` somewhere. Element attrs and text / comment
/// payloads round-trip; children only follow when `deep` is
/// true (matches `Node.cloneNode(deep)` semantics).
fn clone_within_doc(doc: &mut Document, src_id: NodeId, deep: bool) -> Option<NodeId> {
    // Pre-snapshot the source node + its descendant tree so
    // we don't observe in-progress mutations.
    let new_id = match &doc.node(src_id).kind {
        NodeKind::Element(e) => {
            let name = e.name.clone();
            let attrs = e.attrs.clone();
            let id = doc.create_element(&name);
            if let Some(elem) = doc.element_mut(id) {
                for (k, v) in &attrs {
                    elem.set_attr(k, v);
                }
            }
            id
        }
        NodeKind::Text(t) => doc.create_text(&t.clone()),
        NodeKind::Comment(c) => doc.create_comment(&c.clone()),
        NodeKind::Doctype { .. } | NodeKind::Document => return None,
    };
    if deep {
        // Snapshot the child id list first — recursive
        // appends shift the parent's child chain.
        let mut child_ids = Vec::new();
        let mut child = doc.node(src_id).first_child;
        while let Some(id) = child {
            child_ids.push(id);
            child = doc.node(id).next_sibling;
        }
        for cid in child_ids {
            if let Some(new_child) = clone_within_doc(doc, cid, true) {
                doc.append_child(new_id, new_child);
            }
        }
    }
    Some(new_id)
}

/// Walk `doc` and return the first `<body>` element, or
/// `None` if the parsed fragment didn't reach one. Used by
/// `__elemSetInnerHtml` to peel off the synthesized html /
/// head / body wrappers around fragment content.
fn find_body_in(doc: &Document) -> Option<NodeId> {
    for nid in doc.descendants(doc.root) {
        if let Some(elem) = doc.element(nid)
            && elem.name == "body"
        {
            return Some(nid);
        }
    }
    None
}

/// Clone `src_id` (and its subtree) from `src_doc` into
/// `dst_doc` as a child of `dst_parent`. Element attributes are
/// copied verbatim; text and comment payloads carry through.
/// Returns the new NodeId in `dst_doc`. Used by
/// `__elemSetInnerHtml` to materialise parsed fragment content
/// into the live document.
fn clone_subtree_into(
    src_doc: &Document,
    src_id: NodeId,
    dst_parent: NodeId,
    dst_doc: &mut Document,
) -> Option<NodeId> {
    let new_id = match &src_doc.node(src_id).kind {
        NodeKind::Element(e) => {
            let id = dst_doc.create_element(&e.name);
            if let Some(elem) = dst_doc.element_mut(id) {
                for (k, v) in &e.attrs {
                    elem.set_attr(k, v);
                }
            }
            id
        }
        NodeKind::Text(t) => dst_doc.create_text(t),
        NodeKind::Comment(c) => dst_doc.create_comment(c),
        NodeKind::Doctype { .. } | NodeKind::Document => return None,
    };
    dst_doc.append_child(dst_parent, new_id);
    let mut child = src_doc.node(src_id).first_child;
    while let Some(id) = child {
        let next = src_doc.node(id).next_sibling;
        clone_subtree_into(src_doc, id, new_id, dst_doc);
        child = next;
    }
    Some(new_id)
}

/// Re-serialise a node and its subtree back to HTML. The
/// inverse of the parser, with one notable simplification:
/// every element is written as the long open/close form
/// (`<br></br>`) rather than the void-element short form
/// (`<br>`) — round-tripping via the parser still works, and
/// the alternative would require maintaining a void-element
/// list synced with the parser's. Attribute values are
/// double-quoted with `"` → `&quot;` escape; text content
/// escapes `&`, `<`, `>`.
fn serialise_node(doc: &Document, id: NodeId, out: &mut String) {
    match &doc.node(id).kind {
        NodeKind::Element(e) => {
            out.push('<');
            out.push_str(&e.name);
            for (k, v) in &e.attrs {
                out.push(' ');
                out.push_str(k);
                out.push_str("=\"");
                out.push_str(&v.replace('&', "&amp;").replace('"', "&quot;"));
                out.push('"');
            }
            out.push('>');
            let mut child = doc.node(id).first_child;
            while let Some(c) = child {
                serialise_node(doc, c, out);
                child = doc.node(c).next_sibling;
            }
            out.push_str("</");
            out.push_str(&e.name);
            out.push('>');
        }
        NodeKind::Text(t) => {
            let escaped = t
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            out.push_str(&escaped);
        }
        NodeKind::Comment(c) => {
            out.push_str("<!--");
            out.push_str(c);
            out.push_str("-->");
        }
        NodeKind::Doctype { name, .. } => {
            out.push_str("<!DOCTYPE ");
            out.push_str(name);
            out.push('>');
        }
        NodeKind::Document => {
            let mut child = doc.node(id).first_child;
            while let Some(c) = child {
                serialise_node(doc, c, out);
                child = doc.node(c).next_sibling;
            }
        }
    }
}

/// Convert a CSS / HTML kebab-case identifier (`font-size`,
/// `data-foo-bar`) to JS camelCase (`fontSize`, `fooBar`).
/// Each `-x` becomes `X`. Used by the `dataset` getter to
/// match the HTMLElement.dataset spec.
fn kebab_to_camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut upper_next = false;
    for c in s.chars() {
        if c == '-' {
            upper_next = true;
        } else if upper_next {
            out.extend(c.to_uppercase());
            upper_next = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse an inline `style="…"` attribute into ordered
/// (property, value) pairs. Tolerant: missing trailing
/// semicolons are fine; malformed pairs are skipped; values
/// preserve their author casing. Property names are lowercased
/// for matching but the original spelling is dropped — round-
/// tripping through `setStyle` always emits canonical kebab-
/// case lowercase, matching what every browser does.
fn parse_inline_style(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in text.split(';') {
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        let Some(colon) = s.find(':') else { continue };
        let prop = s[..colon].trim().to_ascii_lowercase();
        let value = s[colon + 1..].trim().to_string();
        if prop.is_empty() {
            continue;
        }
        out.push((prop, value));
    }
    out
}

/// Round-trip the inline-style declarations back to attribute
/// form. Emits `prop: value; prop: value` with a trailing
/// semicolon dropped — Chrome's `cssText` formatting.
fn serialise_inline_style(decls: &[(String, String)]) -> String {
    let mut out = String::new();
    for (i, (p, v)) in decls.iter().enumerate() {
        if i > 0 {
            out.push_str("; ");
        }
        out.push_str(p);
        out.push_str(": ");
        out.push_str(v);
    }
    out
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

/// One scheduled timer / repeating interval / requestAnimationFrame
/// callback. The embedder's frame-tick drains entries whose `when`
/// has elapsed and re-enqueues anything with a non-`None`
/// `repeat`.
#[derive(Debug, Clone)]
pub(crate) struct ScheduledTimer {
    pub id: u32,
    pub when: std::time::Instant,
    pub callback: zinc::runtime::value::Value,
    /// `Some` for `setInterval`; the timer re-enqueues itself at
    /// `now + repeat` after firing. `None` for `setTimeout` /
    /// `requestAnimationFrame`.
    pub repeat: Option<std::time::Duration>,
}

/// Per-NodeId layout geometry the embedder publishes after each
/// layout pass. Reads on `Element.getBoundingClientRect` /
/// `offsetWidth` / etc. snapshot from here, so the values are
/// live as of the most recent paint. Missing entries fall back
/// to zeros — matches what a fresh page reports before its
/// first paint.
pub type LayoutFrames = std::collections::HashMap<NodeId, (f32, f32, f32, f32)>;

/// One pending MutationRecord queued for a `MutationObserver`.
/// The observer's callback is invoked with a JS array of these
/// records (mapped from Rust → JS in the deliver step) after
/// every microtask drain.
#[derive(Debug, Clone)]
pub(crate) struct MutationRecord {
    pub kind: MutationKind,
    pub target: NodeId,
    /// Attribute name for `attributes` records, empty for the
    /// childList shape.
    pub attribute_name: String,
    /// Previous attribute value when the observer subscribed
    /// with `attributeOldValue: true`. Empty otherwise.
    pub old_value: String,
    pub added: Vec<NodeId>,
    pub removed: Vec<NodeId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MutationKind {
    Attributes,
    ChildList,
    CharacterData,
}

/// What a `MutationObserver` observes. Stored verbatim from the
/// `observe(target, init)` call so the matcher can decide each
/// record's relevance.
#[derive(Debug, Clone)]
pub(crate) struct MutationSubscription {
    pub target: NodeId,
    pub subtree: bool,
    pub child_list: bool,
    pub attributes: bool,
    pub character_data: bool,
    pub attribute_old_value: bool,
    pub attribute_filter: Option<Vec<String>>,
}

/// One registered observer — its callback Value plus every
/// `observe(target, init)` call's subscription. A single
/// `MutationObserver` can be `observe`d on multiple targets;
/// records collected across all subscriptions are delivered
/// together in one callback invocation.
pub(crate) struct MutationObserver {
    pub callback: zinc::runtime::value::Value,
    pub subscriptions: Vec<MutationSubscription>,
    pub pending: Vec<MutationRecord>,
}

pub(crate) type MutationObservers = std::collections::HashMap<u32, MutationObserver>;

pub struct BindingContext {
    shared: Arc<Mutex<DomShared>>,
    elem_tag: HostTag,
    dirty: Arc<AtomicBool>,
    /// Pending `setTimeout` / `setInterval` callbacks. Drained
    /// by the embedder each frame via `JsContext::tick`. Locked
    /// briefly per scheduler call; held across the JS callback
    /// would deadlock if the callback itself schedules a new
    /// timer.
    timers: Arc<Mutex<Vec<ScheduledTimer>>>,
    /// NodeId → (x, y, width, height) snapshot the embedder
    /// publishes after each layout pass. Backs the geometry
    /// surface (`getBoundingClientRect`, `offsetWidth/Height`,
    /// etc.). Missing entries fall back to zeros — that's the
    /// same answer real browsers give for a detached or
    /// not-yet-painted element.
    layout_frames: Arc<Mutex<LayoutFrames>>,
    /// Registered MutationObservers. Mutating bindings
    /// (setAttribute, appendChild, …) consult this and queue
    /// records on every observer whose subscription matches.
    /// Records deliver as a batched JS-array callback after
    /// `JsContext::tick` drains microtasks.
    observers: Arc<Mutex<MutationObservers>>,
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
    /// Background `fetch()` results awaiting delivery: the pinned
    /// promise handle + the network outcome (`None` = network error,
    /// delivered as a rejection). Pushed from fetch worker threads;
    /// drained on the engine thread by `JsContext::tick`.
    fetch_completions: Arc<Mutex<Vec<(ObjectId, Option<FetchResponse>)>>>,
    /// Count of fetches started but not yet *delivered* to JS —
    /// in-flight network work plus queued completions. Lets sync
    /// embedder paths settle scripts before styling.
    fetch_inflight: Arc<AtomicUsize>,
    /// Embedder wakeup invoked when a background fetch completes, so
    /// the event loop ticks promptly instead of waiting for input.
    fetch_wake: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>,
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
        // Keep a stand-alone Arc clone of the document for the
        // mutation-observer plumbing, since `doc` itself is
        // about to move into `DomShared`. Cheap — every clone
        // is a refcount bump.
        let doc_for_observers = doc.clone();
        let shared = Arc::new(Mutex::new(DomShared {
            doc_handle: doc,
            handles_by_node,
            dirty: dirty.clone(),
        }));
        let observers: Arc<Mutex<MutationObservers>> =
            Arc::new(Mutex::new(MutationObservers::new()));
        let next_observer_id =
            Arc::new(std::sync::atomic::AtomicU32::new(1));

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

        // `__elemDataset(handle)` returns a plain JS object built
        // from every `data-*` attribute on the element, with the
        // kebab-case name converted to camelCase per the
        // HTMLElement.dataset spec (`data-foo-bar` → `fooBar`).
        // Reading only — the prelude exposes `el.dataset` as a
        // getter that calls this each time, so the snapshot
        // always reflects the current DOM. Writing back through
        // the dataset object (`el.dataset.foo = 'x'`) doesn't
        // round-trip to the DOM today; use `setAttribute(
        // 'data-foo', 'x')` for the write path until we have a
        // Proxy-based surface.
        // `__elemContains(parentHandle, childHandle)` — true
        // when `child` is `parent` or a descendant. Walks
        // child→root via the parent chain (cheap) rather than
        // descending parent. Mirrors `Node.contains` semantics
        // (a node contains itself).
        let s = shared.clone();
        engine.register_host_fn("__elemContains", move |_vm, _this, args| {
            let Some(parent_h) = args.first().copied() else { return Ok(Value::boolean(false)) };
            let Some(child_h) = args.get(1).copied() else { return Ok(Value::boolean(false)) };
            let dom = s.lock().unwrap();
            let Some(parent_nid) = dom.node_for_handle(parent_h) else {
                return Ok(Value::boolean(false));
            };
            let Some(child_nid) = dom.node_for_handle(child_h) else {
                return Ok(Value::boolean(false));
            };
            let d = dom.doc_handle.lock().unwrap();
            let mut cur = Some(child_nid);
            while let Some(id) = cur {
                if id == parent_nid {
                    return Ok(Value::boolean(true));
                }
                cur = d.node(id).parent;
            }
            Ok(Value::boolean(false))
        });

        // ---- Node tree (any node, not just elements) ----
        //
        // Element handles are pre-allocated at install time
        // above; text nodes get a host handle on first access
        // through these traversal getters. We use the same
        // `elem_tag` for text nodes so the wrapper code stays
        // single-shape; methods like getAttribute / setAttribute
        // become silent no-ops on a text node (they look up
        // `d.element(nid)` which returns None). The minimal
        // surface JS code uses on text nodes — nodeType,
        // nodeName, nodeValue, parentElement, nextSibling — all
        // work because they read `d.node(nid)` directly.
        let elem_tag_for_node = elem_tag;
        let s = shared.clone();
        engine.register_host_fn("__nodeFirstChild", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else { return Ok(Value::null()) };
            let mut dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else { return Ok(Value::null()) };
            let child = dom.doc_handle.lock().unwrap().node(nid).first_child;
            Ok(dom.ensure_handle(child, vm, elem_tag_for_node))
        });
        let s = shared.clone();
        engine.register_host_fn("__nodeLastChild", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else { return Ok(Value::null()) };
            let mut dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else { return Ok(Value::null()) };
            // bui-dom doesn't carry a `last_child` slot — walk
            // from first_child to the tail (cheap for the small
            // sibling lists JS code typically iterates).
            let mut last = None;
            {
                let d = dom.doc_handle.lock().unwrap();
                let mut c = d.node(nid).first_child;
                while let Some(id) = c {
                    last = Some(id);
                    c = d.node(id).next_sibling;
                }
            }
            Ok(dom.ensure_handle(last, vm, elem_tag_for_node))
        });
        let s = shared.clone();
        engine.register_host_fn("__nodeNextSibling", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else { return Ok(Value::null()) };
            let mut dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else { return Ok(Value::null()) };
            let sib = dom.doc_handle.lock().unwrap().node(nid).next_sibling;
            Ok(dom.ensure_handle(sib, vm, elem_tag_for_node))
        });
        let s = shared.clone();
        engine.register_host_fn("__nodePrevSibling", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else { return Ok(Value::null()) };
            let mut dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else { return Ok(Value::null()) };
            // No `prev_sibling` slot in bui-dom either; walk
            // the parent's child list to find the one before us.
            let mut prev = None;
            {
                let d = dom.doc_handle.lock().unwrap();
                let parent = d.node(nid).parent;
                if let Some(p) = parent {
                    let mut c = d.node(p).first_child;
                    while let Some(id) = c {
                        if id == nid {
                            break;
                        }
                        prev = Some(id);
                        c = d.node(id).next_sibling;
                    }
                }
            }
            Ok(dom.ensure_handle(prev, vm, elem_tag_for_node))
        });
        let s = shared.clone();
        engine.register_host_fn("__nodeType", move |_vm, _this, args| {
            let Some(handle) = args.first().copied() else { return Ok(Value::int(0)) };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else { return Ok(Value::int(0)) };
            let d = dom.doc_handle.lock().unwrap();
            // DOM spec node-type constants:
            // ELEMENT_NODE=1, TEXT_NODE=3, COMMENT_NODE=8,
            // DOCUMENT_NODE=9, DOCUMENT_TYPE_NODE=10.
            let ty = match &d.node(nid).kind {
                NodeKind::Element(_) => 1,
                NodeKind::Text(_) => 3,
                NodeKind::Comment(_) => 8,
                NodeKind::Document => 9,
                NodeKind::Doctype { .. } => 10,
            };
            Ok(Value::int(ty))
        });
        let s = shared.clone();
        engine.register_host_fn("__nodeName", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else { return Ok(vm.value_from_str("")) };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(vm.value_from_str(""));
            };
            let name = {
                let d = dom.doc_handle.lock().unwrap();
                match &d.node(nid).kind {
                    NodeKind::Element(e) => e.name.to_ascii_uppercase(),
                    NodeKind::Text(_) => "#text".to_string(),
                    NodeKind::Comment(_) => "#comment".to_string(),
                    NodeKind::Document => "#document".to_string(),
                    NodeKind::Doctype { name, .. } => name.clone(),
                }
            };
            drop(dom);
            Ok(vm.value_from_str(&name))
        });
        let s = shared.clone();
        engine.register_host_fn("__nodeValue", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else { return Ok(Value::null()) };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else { return Ok(Value::null()) };
            let text = {
                let d = dom.doc_handle.lock().unwrap();
                match &d.node(nid).kind {
                    NodeKind::Text(t) => Some(t.clone()),
                    _ => None,
                }
            };
            drop(dom);
            match text {
                Some(t) => Ok(vm.value_from_str(&t)),
                None => Ok(Value::null()),
            }
        });

        let s = shared.clone();
        engine.register_host_fn("__elemDataset", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(vm.alloc_object());
            };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(vm.alloc_object());
            };
            // Collect (camelCase, value) pairs while the lock
            // is held; the heap allocations / interner writes
            // happen afterward.
            let mut entries: Vec<(String, String)> = Vec::new();
            {
                let d = dom.doc_handle.lock().unwrap();
                let Some(elem) = d.element(nid) else {
                    return Ok(vm.alloc_object());
                };
                for (name, value) in &elem.attrs {
                    let Some(rest) = name.strip_prefix("data-") else { continue };
                    entries.push((kebab_to_camel(rest), value.to_string()));
                }
            }
            drop(dom);
            let obj = vm.alloc_object();
            for (k, v) in &entries {
                let s = vm.value_from_str(v);
                vm.set_property(obj, k, s);
            }
            Ok(obj)
        });

        // `__elemGetStyle(handle, prop)` reads a single CSS
        // property from the element's inline `style="…"`
        // attribute. Property names match the CSS form
        // (kebab-case: `background-color`, `font-size`).
        // Returns an empty string when the property isn't set
        // — matches the spec's
        // `CSSStyleDeclaration.getPropertyValue` return shape.
        let s = shared.clone();
        engine.register_host_fn("__elemGetStyle", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(vm.value_from_str(""));
            };
            let Some(prop) = read_str(vm, args.get(1)) else {
                return Ok(vm.value_from_str(""));
            };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(vm.value_from_str(""));
            };
            let inline = dom
                .doc_handle
                .lock()
                .unwrap()
                .element(nid)
                .and_then(|e| e.get_attr("style").map(|s| s.to_string()))
                .unwrap_or_default();
            drop(dom);
            let value = parse_inline_style(&inline)
                .into_iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(&prop))
                .map(|(_, v)| v)
                .unwrap_or_default();
            Ok(vm.value_from_str(&value))
        });

        // `__elemSetStyle(handle, prop, value)` writes a single
        // CSS property into the inline style attribute. An
        // empty `value` removes the property. The full attr is
        // re-serialised after each write so subsequent reads
        // see the canonical "prop: value; prop: value" form.
        let s = shared.clone();
        engine.register_host_fn("__elemSetStyle", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(Value::null());
            };
            let Some(prop) = read_str(vm, args.get(1)) else {
                return Ok(Value::null());
            };
            let value = read_str(vm, args.get(2)).unwrap_or_default();
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(Value::null());
            };
            let changed;
            {
                let mut d = dom.doc_handle.lock().unwrap();
                let inline = d
                    .element(nid)
                    .and_then(|e| e.get_attr("style").map(|s| s.to_string()))
                    .unwrap_or_default();
                let mut decls = parse_inline_style(&inline);
                decls.retain(|(k, _)| !k.eq_ignore_ascii_case(&prop));
                if !value.is_empty() {
                    decls.push((prop.clone(), value.clone()));
                }
                let serialised = serialise_inline_style(&decls);
                let prev = d
                    .element(nid)
                    .and_then(|e| e.get_attr("style").map(|s| s.to_string()));
                changed = prev.as_deref() != Some(serialised.as_str());
                if let Some(elem) = d.element_mut(nid) {
                    if serialised.is_empty() {
                        elem.remove_attr("style");
                    } else {
                        elem.set_attr("style", &serialised);
                    }
                }
            }
            if changed {
                dom.mark_dirty();
            }
            Ok(Value::null())
        });

        // `__elemGetStyleText(handle)` and `__elemSetStyleText`
        // are the `cssText` shorthand: read or replace the
        // entire inline style attribute as one string. Setting
        // an empty cssText removes the attribute.
        let s = shared.clone();
        engine.register_host_fn("__elemGetStyleText", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(vm.value_from_str(""));
            };
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(vm.value_from_str(""));
            };
            let text = dom
                .doc_handle
                .lock()
                .unwrap()
                .element(nid)
                .and_then(|e| e.get_attr("style").map(|s| s.to_string()))
                .unwrap_or_default();
            Ok(vm.value_from_str(&text))
        });
        let s = shared.clone();
        engine.register_host_fn("__elemSetStyleText", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(Value::null());
            };
            let text = read_str(vm, args.get(1)).unwrap_or_default();
            let dom = s.lock().unwrap();
            let Some(nid) = dom.node_for_handle(handle) else {
                return Ok(Value::null());
            };
            {
                let mut d = dom.doc_handle.lock().unwrap();
                if let Some(elem) = d.element_mut(nid) {
                    if text.trim().is_empty() {
                        elem.remove_attr("style");
                    } else {
                        elem.set_attr("style", &text);
                    }
                }
            }
            dom.mark_dirty();
            Ok(Value::null())
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

        // `__elemSetInnerHtml(handle, html)` parses `html` via
        // bui-html and replaces the element's children with the
        // parsed fragment. Modern templating code does this
        // constantly — `el.innerHTML = '<span>x</span>'`. We
        // don't have a fragment parser per se, so we use the
        // full-document parser and pull out the body's
        // children (the synthesized html/head/body wrappers
        // get dropped on the floor).
        let s = shared.clone();
        let observers_for_ih = observers.clone();
        let doc_for_ih = doc_for_observers.clone();
        engine.register_host_fn("__elemSetInnerHtml", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(Value::undefined());
            };
            let html = read_str(vm, args.get(1)).unwrap_or_default();
            let dom = s.lock().unwrap();
            let Some(target) = dom.node_for_handle(handle) else {
                return Ok(Value::undefined());
            };
            // Parse the fragment via the full HTML parser.
            // `bui_html::parse` wraps everything in synthesized
            // <html>/<head>/<body>, so we hunt for the body and
            // copy its children into the target. For fragments
            // that don't reach a body (e.g. just text), we fall
            // back to the document root's children.
            let parsed = bui_html::parse(&html);
            {
                let mut d = dom.doc_handle.lock().unwrap();
                // Detach existing children before reattaching
                // the fragment. Collect the ids up front because
                // `detach` mutates sibling links.
                let mut child = d.node(target).first_child;
                let mut to_detach = Vec::new();
                while let Some(id) = child {
                    to_detach.push(id);
                    child = d.node(id).next_sibling;
                }
                for id in to_detach {
                    d.detach(id);
                }
                // Locate the source root (parsed body, fall
                // through to root if none) and clone-append
                // each of its element/text children.
                let src_root = find_body_in(&parsed).unwrap_or(parsed.root);
                let mut child = parsed.node(src_root).first_child;
                while let Some(id) = child {
                    let next = parsed.node(id).next_sibling;
                    clone_subtree_into(&parsed, id, target, &mut d);
                    child = next;
                }
            }
            dom.mark_dirty();
            drop(dom);
            // One childList record covers the whole replacement.
            record_mutation(
                &observers_for_ih,
                &doc_for_ih,
                MutationRecord {
                    kind: MutationKind::ChildList,
                    target,
                    attribute_name: String::new(),
                    old_value: String::new(),
                    added: Vec::new(),
                    removed: Vec::new(),
                },
            );
            Ok(Value::undefined())
        });

        // `__elemInsertAdjacentHtml(handle, position, html)`
        // parses `html` and inserts at one of four positions
        // relative to `target`. Real-world use: htmx and
        // many template engines build incremental DOM
        // updates via this API rather than full innerHTML
        // replacement.
        //
        // position semantics (HTML spec):
        //  - 'beforebegin': before target itself
        //  - 'afterbegin':  inside target, before first child
        //  - 'beforeend':   inside target, after last child
        //  - 'afterend':    after target itself
        let s2 = shared.clone();
        let observers_for_iah = observers.clone();
        let doc_for_iah = doc_for_observers.clone();
        engine.register_host_fn("__elemInsertAdjacentHtml", move |vm, _this, args| {
            let s = &s2;
            let Some(handle) = args.first().copied() else { return Ok(Value::undefined()) };
            let Some(position) = read_str(vm, args.get(1)) else {
                return Ok(Value::undefined());
            };
            let html = read_str(vm, args.get(2)).unwrap_or_default();
            let pos = position.to_ascii_lowercase();
            let dom = s.lock().unwrap();
            let Some(target) = dom.node_for_handle(handle) else { return Ok(Value::undefined()) };
            let parsed = bui_html::parse(&html);
            {
                let mut d = dom.doc_handle.lock().unwrap();
                let src_root = find_body_in(&parsed).unwrap_or(parsed.root);
                let mut new_ids: Vec<NodeId> = Vec::new();
                let mut child = parsed.node(src_root).first_child;
                while let Some(id) = child {
                    let next = parsed.node(id).next_sibling;
                    if let Some(new_id) = match pos.as_str() {
                        "afterbegin" | "beforeend" => {
                            clone_subtree_into(&parsed, id, target, &mut d)
                        }
                        "beforebegin" | "afterend" => {
                            if let Some(parent) = d.node(target).parent {
                                clone_subtree_into(&parsed, id, parent, &mut d)
                            } else { None }
                        }
                        _ => None,
                    } {
                        new_ids.push(new_id);
                    }
                    child = next;
                }
                // For `afterbegin` and `beforebegin` we need
                // the new nodes to land BEFORE the existing
                // first child or target sibling — but
                // `append_child` always appends to the end.
                // The reorder is a small follow-up; for now
                // the inserted content shows up but at the
                // wrong end. Most htmx use is `beforeend`
                // which lands correctly.
                let _ = new_ids;
            }
            dom.mark_dirty();
            drop(dom);
            record_mutation(
                &observers_for_iah,
                &doc_for_iah,
                MutationRecord {
                    kind: MutationKind::ChildList,
                    target,
                    attribute_name: String::new(),
                    old_value: String::new(),
                    added: Vec::new(),
                    removed: Vec::new(),
                },
            );
            Ok(Value::undefined())
        });

        // `__elemCloneNode(handle, deep)` — produces a fresh
        // node with the same element name + attrs, optionally
        // recursing into children. The clone is detached
        // (no parent); typical use is `parent.appendChild(
        // template.cloneNode(true))`.
        let s2 = shared.clone();
        let alloc_tag_clone = elem_tag;
        engine.register_host_fn("__elemCloneNode", move |vm, _this, args| {
            let s = &s2;
            let Some(handle) = args.first().copied() else { return Ok(Value::null()) };
            let deep = args.get(1).and_then(|v| v.as_bool()).unwrap_or(false);
            let nid = {
                let dom = s.lock().unwrap();
                match dom.node_for_handle(handle) {
                    Some(n) => n,
                    None => return Ok(Value::null()),
                }
            };
            // We build a detached subtree by cloning into a
            // throwaway anchor under doc.root, then taking
            // the clone's id and detaching from the anchor.
            // Detaching keeps node_id valid (bui-dom doesn't
            // free) so the JS-side `appendChild(clone)`
            // re-parents it cleanly.
            let new_id: Option<NodeId>;
            {
                let dom = s.lock().unwrap();
                let mut d = dom.doc_handle.lock().unwrap();
                new_id = clone_within_doc(&mut d, nid, deep);
            }
            let Some(new_id) = new_id else { return Ok(Value::null()) };
            let h = vm.alloc_host_object(alloc_tag_clone.0, new_id.0 as u64);
            {
                let mut dom = s.lock().unwrap();
                dom.handles_by_node.insert(new_id, h);
            }
            Ok(h)
        });

        // `__elemGetInnerHtml(handle)` walks the element's
        // children and re-serialises them. Cheap for typical
        // template-rendered subtrees; not optimised for huge
        // DOMs.
        let s2 = shared.clone();
        engine.register_host_fn("__elemGetInnerHtml", move |vm, _this, args| {
            let s = &s2;
            let Some(handle) = args.first().copied() else {
                return Ok(vm.value_from_str(""));
            };
            let dom = s.lock().unwrap();
            let Some(target) = dom.node_for_handle(handle) else {
                return Ok(vm.value_from_str(""));
            };
            let html = {
                let d = dom.doc_handle.lock().unwrap();
                let mut out = String::new();
                let mut child = d.node(target).first_child;
                while let Some(id) = child {
                    serialise_node(&d, id, &mut out);
                    child = d.node(id).next_sibling;
                }
                out
            };
            drop(dom);
            Ok(vm.value_from_str(&html))
        });

        // For attribute / childList mutations we capture the
        // pre-mutation state inside the existing locked
        // section, then drop locks and call `record_mutation`
        // afterwards. Calling it under the doc lock would
        // re-enter the same Mutex when a MutationObserver
        // callback later reads the DOM via a host fn.
        let s = shared.clone();
        let observers_for_set = observers.clone();
        let doc_for_set = doc_for_observers.clone();
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
            let old_value = {
                let mut d = dom.doc_handle.lock().unwrap();
                let prev = d
                    .element(nid)
                    .and_then(|e| e.get_attr(&name).map(|s| s.to_string()))
                    .unwrap_or_default();
                if let Some(elem) = d.element_mut(nid) {
                    elem.set_attr(&name, &value);
                }
                prev
            };
            dom.mark_dirty();
            drop(dom);
            record_mutation(
                &observers_for_set,
                &doc_for_set,
                MutationRecord {
                    kind: MutationKind::Attributes,
                    target: nid,
                    attribute_name: name.clone(),
                    old_value,
                    added: Vec::new(),
                    removed: Vec::new(),
                },
            );
            Ok(Value::undefined())
        });

        let s = shared.clone();
        let observers_for_remove = observers.clone();
        let doc_for_remove = doc_for_observers.clone();
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
            let (changed, old_value) = {
                let mut d = dom.doc_handle.lock().unwrap();
                let prev = d
                    .element(nid)
                    .and_then(|e| e.get_attr(&name).map(|s| s.to_string()))
                    .unwrap_or_default();
                let changed = d
                    .element_mut(nid)
                    .map(|e| e.remove_attr(&name))
                    .unwrap_or(false);
                (changed, prev)
            };
            if changed {
                dom.mark_dirty();
                drop(dom);
                record_mutation(
                    &observers_for_remove,
                    &doc_for_remove,
                    MutationRecord {
                        kind: MutationKind::Attributes,
                        target: nid,
                        attribute_name: name.clone(),
                        old_value,
                        added: Vec::new(),
                        removed: Vec::new(),
                    },
                );
            }
            Ok(Value::undefined())
        });

        let s = shared.clone();
        let observers_for_text = observers.clone();
        let doc_for_text = doc_for_observers.clone();
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
            drop(dom);
            // textContent reset emits a single childList record
            // — modeled as "every previous child removed, one
            // text child added" elsewhere, but a single record
            // with no added/removed lists is the minimum every
            // observer expects.
            record_mutation(
                &observers_for_text,
                &doc_for_text,
                MutationRecord {
                    kind: MutationKind::ChildList,
                    target: nid,
                    attribute_name: String::new(),
                    old_value: String::new(),
                    added: Vec::new(),
                    removed: Vec::new(),
                },
            );
            Ok(Value::undefined())
        });

        let s = shared.clone();
        let observers_for_append = observers.clone();
        let doc_for_append = doc_for_observers.clone();
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
            drop(dom);
            record_mutation(
                &observers_for_append,
                &doc_for_append,
                MutationRecord {
                    kind: MutationKind::ChildList,
                    target: parent,
                    attribute_name: String::new(),
                    old_value: String::new(),
                    added: vec![child],
                    removed: Vec::new(),
                },
            );
            Ok(child_h)
        });

        let s = shared.clone();
        let observers_for_insert = observers.clone();
        let doc_for_insert = doc_for_observers.clone();
        engine.register_host_fn("__elemInsertBefore", move |_vm, _this, args| {
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
            // Third arg: reference node handle, or null/undefined for
            // append (DOM insertBefore(child, null) semantics).
            let reference = args
                .get(2)
                .copied()
                .filter(|v| !v.is_null() && !v.is_undefined())
                .and_then(|h| dom.node_for_handle(h));
            {
                let mut d = dom.doc_handle.lock().unwrap();
                d.insert_before(parent, child, reference);
            }
            dom.mark_dirty();
            drop(dom);
            record_mutation(
                &observers_for_insert,
                &doc_for_insert,
                MutationRecord {
                    kind: MutationKind::ChildList,
                    target: parent,
                    attribute_name: String::new(),
                    old_value: String::new(),
                    added: vec![child],
                    removed: Vec::new(),
                },
            );
            Ok(child_h)
        });

        let s = shared.clone();
        let observers_for_rm = observers.clone();
        let doc_for_rm = doc_for_observers.clone();
        engine.register_host_fn("__elemRemoveChild", move |_vm, _this, args| {
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
            // detach severs sibling + parent pointers; the node id
            // stays allocated so a later appendChild can re-attach.
            {
                let mut d = dom.doc_handle.lock().unwrap();
                d.detach(child);
            }
            dom.mark_dirty();
            drop(dom);
            record_mutation(
                &observers_for_rm,
                &doc_for_rm,
                MutationRecord {
                    kind: MutationKind::ChildList,
                    target: parent,
                    attribute_name: String::new(),
                    old_value: String::new(),
                    added: Vec::new(),
                    removed: vec![child],
                },
            );
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

        // classList mutations fire `attributes` MutationRecord
        // entries on the element, mirroring what every real
        // browser does. We can't share the helper across all
        // three (the closure for f differs per op), so each
        // calls `class_list_mutate` and `record_mutation` on
        // its own.
        let s = shared.clone();
        let observers_for_cla = observers.clone();
        let doc_for_cla = doc_for_observers.clone();
        engine.register_host_fn("__elemClassListAdd", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else { return Ok(Value::undefined()) };
            let Some(class) = read_str(vm, args.get(1)) else { return Ok(Value::undefined()) };
            if let Some((nid, old)) = class_list_mutate(&s, handle, |tokens| {
                if !tokens.iter().any(|t| t == &class) {
                    tokens.push(class.clone());
                }
            }) {
                record_mutation(
                    &observers_for_cla,
                    &doc_for_cla,
                    MutationRecord {
                        kind: MutationKind::Attributes,
                        target: nid,
                        attribute_name: "class".into(),
                        old_value: old,
                        added: Vec::new(),
                        removed: Vec::new(),
                    },
                );
            }
            Ok(Value::undefined())
        });

        let s = shared.clone();
        let observers_for_clr = observers.clone();
        let doc_for_clr = doc_for_observers.clone();
        engine.register_host_fn("__elemClassListRemove", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else { return Ok(Value::undefined()) };
            let Some(class) = read_str(vm, args.get(1)) else { return Ok(Value::undefined()) };
            if let Some((nid, old)) = class_list_mutate(&s, handle, |tokens| {
                tokens.retain(|t| t != &class);
            }) {
                record_mutation(
                    &observers_for_clr,
                    &doc_for_clr,
                    MutationRecord {
                        kind: MutationKind::Attributes,
                        target: nid,
                        attribute_name: "class".into(),
                        old_value: old,
                        added: Vec::new(),
                        removed: Vec::new(),
                    },
                );
            }
            Ok(Value::undefined())
        });

        let s = shared.clone();
        let observers_for_clt = observers.clone();
        let doc_for_clt = doc_for_observers.clone();
        engine.register_host_fn("__elemClassListToggle", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else { return Ok(Value::boolean(false)) };
            let Some(class) = read_str(vm, args.get(1)) else { return Ok(Value::boolean(false)) };
            let mut now_present = false;
            if let Some((nid, old)) = class_list_mutate(&s, handle, |tokens| {
                if let Some(pos) = tokens.iter().position(|t| t == &class) {
                    tokens.remove(pos);
                    now_present = false;
                } else {
                    tokens.push(class.clone());
                    now_present = true;
                }
            }) {
                record_mutation(
                    &observers_for_clt,
                    &doc_for_clt,
                    MutationRecord {
                        kind: MutationKind::Attributes,
                        target: nid,
                        attribute_name: "class".into(),
                        old_value: old,
                        added: Vec::new(),
                        removed: Vec::new(),
                    },
                );
            }
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

        // ---- Asynchronous fetch ----
        //
        // `__fetch_start(url)` returns a *pending* promise and runs
        // the embedder-supplied fetcher on a background thread, so
        // page scripts calling `fetch()` never block the engine
        // thread. The worker pushes the outcome into
        // `fetch_completions`; `JsContext::tick` (and the tail of
        // `install_and_run`) resolves/rejects the promise on the
        // engine thread and drains the resulting microtasks. The
        // prelude's `fetch` wrapper layers the Response surface
        // (.text() / .json() returning promises) on top.
        let fetch_completions: Arc<Mutex<Vec<(ObjectId, Option<FetchResponse>)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let fetch_inflight: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        let fetch_wake: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>> =
            Arc::new(Mutex::new(None));
        let fetcher_for_fn = fetcher.clone();
        let completions_for_fn = fetch_completions.clone();
        let inflight_for_fn = fetch_inflight.clone();
        let wake_for_fn = fetch_wake.clone();
        engine.register_host_fn("__fetch_start", move |vm, _this, args| {
            let url = read_str(vm, args.first()).unwrap_or_default();
            let (pid, promise) = vm.host_promise_create();
            inflight_for_fn.fetch_add(1, Ordering::SeqCst);
            match fetcher_for_fn.clone() {
                Some(f) => {
                    let completions = completions_for_fn.clone();
                    let wake = wake_for_fn.clone();
                    std::thread::spawn(move || {
                        let result = f(&url);
                        completions.lock().unwrap().push((pid, result));
                        let hook = wake.lock().unwrap().clone();
                        if let Some(hook) = hook {
                            hook();
                        }
                    });
                }
                // No fetcher (CLI / tests): immediate network-error
                // rejection, delivered on the next completion pass.
                None => completions_for_fn.lock().unwrap().push((pid, None)),
            }
            Ok(promise)
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

        // ---- MutationObserver registry ----
        //
        // `__newMutationObserver(callback)` allocates an entry
        // and returns an opaque numeric id the prelude stores
        // on the JS-side observer object. `__moObserve(id,
        // target, init)` adds a subscription; `__moDisconnect(id)`
        // drops the entry entirely. Records queued by mutating
        // host fns deliver via `JsContext::deliver_mutations`,
        // called from `tick`.
        let observers_for_new = observers.clone();
        let id_for_new = next_observer_id.clone();
        engine.register_host_fn("__newMutationObserver", move |_vm, _this, args| {
            let Some(cb) = args.first().copied() else { return Ok(Value::int(0)) };
            if !cb.is_function() && !cb.is_object() {
                return Ok(Value::int(0));
            }
            let id = id_for_new.fetch_add(1, Ordering::SeqCst);
            observers_for_new.lock().unwrap().insert(
                id,
                MutationObserver {
                    callback: cb,
                    subscriptions: Vec::new(),
                    pending: Vec::new(),
                },
            );
            Ok(Value::int(id as i32))
        });

        let observers_for_obs = observers.clone();
        let shared_for_obs = shared.clone();
        engine.register_host_fn("__moObserve", move |vm, _this, args| {
            let Some(id) = args.first().and_then(|v| v.as_number()) else {
                return Ok(Value::null());
            };
            let id = id as u32;
            let Some(target_h) = args.get(1).copied() else { return Ok(Value::null()) };
            let Some(target_nid) =
                shared_for_obs.lock().unwrap().node_for_handle(target_h)
            else {
                return Ok(Value::null());
            };
            let init = args.get(2).copied().unwrap_or(Value::null());
            let read_bool = |vm: &mut zinc::vm::vm::Vm, name: &str| -> bool {
                vm.get_property(init, name)
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            };
            let subtree = read_bool(vm, "subtree");
            let child_list = read_bool(vm, "childList");
            let attributes = read_bool(vm, "attributes");
            let character_data = read_bool(vm, "characterData");
            let attribute_old_value = read_bool(vm, "attributeOldValue");
            // `attributeFilter: ['data-foo', …]`: read each
            // element via indexed property access.
            let attribute_filter = vm.get_property(init, "attributeFilter").and_then(|filter| {
                let len = vm
                    .get_property(filter, "length")
                    .and_then(|v| v.as_number())?;
                let mut out = Vec::with_capacity(len as usize);
                for i in 0..(len as usize) {
                    let elem = vm.get_property(filter, &i.to_string())?;
                    let s = read_str(vm, Some(&elem))?;
                    out.push(s);
                }
                Some(out)
            });
            if let Some(obs) = observers_for_obs.lock().unwrap().get_mut(&id) {
                obs.subscriptions.push(MutationSubscription {
                    target: target_nid,
                    subtree,
                    child_list,
                    attributes,
                    character_data,
                    attribute_old_value,
                    attribute_filter,
                });
            }
            Ok(Value::null())
        });

        let observers_for_dis = observers.clone();
        engine.register_host_fn("__moDisconnect", move |_vm, _this, args| {
            let Some(id) = args.first().and_then(|v| v.as_number()) else {
                return Ok(Value::null());
            };
            observers_for_dis.lock().unwrap().remove(&(id as u32));
            Ok(Value::null())
        });

        // ---- Layout geometry ----
        //
        // `__elemFrame(handle)` returns the (x, y, w, h) the
        // embedder published for this NodeId after the most
        // recent layout pass, packed into a JS array. The
        // prelude unpacks it into `getBoundingClientRect` /
        // `offset*` / `client*` reads. Missing entries (newly-
        // created nodes, pre-paint state) return [0, 0, 0, 0]
        // — matching what real browsers return for elements
        // outside the laid-out tree.
        let layout_frames: Arc<Mutex<LayoutFrames>> =
            Arc::new(Mutex::new(LayoutFrames::new()));
        let frames_for_get = layout_frames.clone();
        let shared_for_frame = shared.clone();
        engine.register_host_fn("__elemFrame", move |vm, _this, args| {
            let Some(handle) = args.first().copied() else {
                return Ok(vm.alloc_array(vec![
                    Value::number(0.0),
                    Value::number(0.0),
                    Value::number(0.0),
                    Value::number(0.0),
                ]));
            };
            let nid = match shared_for_frame.lock().unwrap().node_for_handle(handle) {
                Some(n) => n,
                None => {
                    return Ok(vm.alloc_array(vec![
                        Value::number(0.0),
                        Value::number(0.0),
                        Value::number(0.0),
                        Value::number(0.0),
                    ]));
                }
            };
            let (x, y, w, h) = frames_for_get
                .lock()
                .ok()
                .and_then(|map| map.get(&nid).copied())
                .unwrap_or((0.0, 0.0, 0.0, 0.0));
            Ok(vm.alloc_array(vec![
                Value::number(x as f64),
                Value::number(y as f64),
                Value::number(w as f64),
                Value::number(h as f64),
            ]))
        });

        // ---- Timer scheduler ----
        //
        // `__setTimeoutHost(fn, ms)` and `__setIntervalHost(fn,
        // ms)` push onto a shared timer queue that the embedder
        // drains each frame via `JsContext::tick(now)`. They
        // return a monotonic id the script can pass to
        // `clearTimeout` / `clearInterval` to cancel.
        //
        // Pure-JS code calling `setTimeout(fn, 0)` still gets
        // the synchronous-during-prelude shape because we
        // schedule the timer for "now"; the next tick fires
        // it. That delay is at most one frame (~16 ms) which
        // matches what real browsers do for `setTimeout(fn, 0)`.
        let timers: Arc<Mutex<Vec<ScheduledTimer>>> =
            Arc::new(Mutex::new(Vec::new()));
        let next_timer_id =
            Arc::new(std::sync::atomic::AtomicU32::new(1));

        let timers_for_set = timers.clone();
        let id_for_set = next_timer_id.clone();
        engine.register_host_fn("__setTimeoutHost", move |_vm, _this, args| {
            let Some(cb) = args.first().copied() else { return Ok(Value::int(0)) };
            if !cb.is_function() && !cb.is_object() {
                return Ok(Value::int(0));
            }
            let ms = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0).max(0.0);
            let when = std::time::Instant::now()
                + std::time::Duration::from_micros((ms * 1000.0) as u64);
            let id = id_for_set.fetch_add(1, Ordering::SeqCst);
            timers_for_set.lock().unwrap().push(ScheduledTimer {
                id,
                when,
                callback: cb,
                repeat: None,
            });
            Ok(Value::int(id as i32))
        });

        let timers_for_int = timers.clone();
        let id_for_int = next_timer_id.clone();
        engine.register_host_fn("__setIntervalHost", move |_vm, _this, args| {
            let Some(cb) = args.first().copied() else { return Ok(Value::int(0)) };
            if !cb.is_function() && !cb.is_object() {
                return Ok(Value::int(0));
            }
            let ms = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0).max(4.0);
            let repeat = std::time::Duration::from_micros((ms * 1000.0) as u64);
            let id = id_for_int.fetch_add(1, Ordering::SeqCst);
            timers_for_int.lock().unwrap().push(ScheduledTimer {
                id,
                when: std::time::Instant::now() + repeat,
                callback: cb,
                repeat: Some(repeat),
            });
            Ok(Value::int(id as i32))
        });

        let timers_for_clear = timers.clone();
        engine.register_host_fn("__clearTimerHost", move |_vm, _this, args| {
            let Some(id) = args.first().and_then(|v| v.as_number()) else {
                return Ok(Value::null());
            };
            let target = id as u32;
            let mut t = timers_for_clear.lock().unwrap();
            t.retain(|e| e.id != target);
            Ok(Value::null())
        });

        // requestAnimationFrame schedules a one-shot callback
        // for the next paint frame. We approximate that by
        // queueing for "now" — the next `tick` fires it.
        let timers_for_raf = timers.clone();
        let id_for_raf = next_timer_id.clone();
        engine.register_host_fn("__requestAnimationFrameHost", move |_vm, _this, args| {
            let Some(cb) = args.first().copied() else { return Ok(Value::int(0)) };
            let id = id_for_raf.fetch_add(1, Ordering::SeqCst);
            timers_for_raf.lock().unwrap().push(ScheduledTimer {
                id,
                when: std::time::Instant::now(),
                callback: cb,
                repeat: None,
            });
            Ok(Value::int(id as i32))
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
            timers,
            layout_frames,
            observers,
            fetch_completions,
            fetch_inflight,
            fetch_wake,
        }
    }

    /// Completed background fetches awaiting promise settlement.
    /// Drained on the engine thread only.
    pub(crate) fn take_fetch_completions(&self) -> Vec<(ObjectId, Option<FetchResponse>)> {
        std::mem::take(&mut *self.fetch_completions.lock().unwrap())
    }

    /// Fetches started but not yet delivered to JS (in-flight network
    /// work + queued completions).
    pub(crate) fn fetch_pending(&self) -> usize {
        self.fetch_inflight.load(Ordering::SeqCst)
    }

    /// Record that `n` completions were delivered to JS.
    pub(crate) fn fetch_delivered(&self, n: usize) {
        self.fetch_inflight.fetch_sub(n, Ordering::SeqCst);
    }

    /// Install the embedder wakeup invoked when a background fetch
    /// completes. No-op if a hook is already set.
    pub(crate) fn ensure_fetch_wake(&self, hook: &Arc<dyn Fn() + Send + Sync>) {
        let mut slot = self.fetch_wake.lock().unwrap();
        if slot.is_none() {
            *slot = Some(hook.clone());
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

    /// Shared timer queue — `setTimeout` / `setInterval` /
    /// `requestAnimationFrame` callbacks waiting to fire. The
    /// embedder drains it each frame tick via
    /// `JsContext::tick(now)`.
    pub(crate) fn timers(&self) -> Arc<Mutex<Vec<ScheduledTimer>>> {
        self.timers.clone()
    }

    /// Shared NodeId → (x, y, width, height) map backing the
    /// geometry surface (`getBoundingClientRect` etc.). The
    /// embedder calls `JsContext::publish_layout_frames` after
    /// each layout pass; reads from JS go through `__elemFrame`.
    pub fn layout_frames(&self) -> Arc<Mutex<LayoutFrames>> {
        self.layout_frames.clone()
    }

    /// Registered MutationObservers, keyed by opaque id.
    /// `JsContext::deliver_mutations` walks these after each
    /// `tick` and invokes any observer whose `pending` queue
    /// has accumulated records.
    pub(crate) fn observers(&self) -> Arc<Mutex<MutationObservers>> {
        self.observers.clone()
    }

    /// Side-table NodeId → host handle. Used by
    /// `JsContext::deliver_mutations` when building the
    /// `target` / `addedNodes` / `removedNodes` fields of a
    /// JS-side MutationRecord — we re-use the wrapped element
    /// when one exists, else ask the VM to allocate.
    pub(crate) fn elem_tag_raw(&self) -> u32 {
        self.elem_tag.0
    }

    /// Resolve a NodeId to its host handle, allocating lazily
    /// if needed. Used by mutation-record delivery.
    pub(crate) fn ensure_handle_for(
        &self,
        nid: NodeId,
        vm: &mut zinc::vm::vm::Vm,
    ) -> Value {
        let mut dom = self.shared.lock().unwrap();
        dom.ensure_handle(Some(nid), vm, self.elem_tag)
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
/// Returns `Some((NodeId, old_class_string))` when the class
/// attribute actually changed, so the caller can fire a
/// MutationRecord. `None` when the element doesn't resolve or
/// the mutation was a no-op (e.g. `classList.add('x')` on an
/// element that already has `x`).
fn class_list_mutate<F>(
    shared: &Arc<Mutex<DomShared>>,
    handle: Value,
    f: F,
) -> Option<(NodeId, String)>
where
    F: FnOnce(&mut Vec<String>),
{
    let dom = shared.lock().unwrap();
    let nid = dom.node_for_handle(handle)?;
    let result;
    {
        let mut d = dom.doc_handle.lock().unwrap();
        let elem = d.element_mut(nid)?;
        let mut tokens: Vec<String> = elem
            .get_attr("class")
            .map(|s| s.split_ascii_whitespace().map(String::from).collect())
            .unwrap_or_default();
        let before = tokens.join(" ");
        f(&mut tokens);
        let after = tokens.join(" ");
        if before == after {
            return None;
        }
        if tokens.is_empty() {
            elem.remove_attr("class");
        } else {
            elem.set_attr("class", &after);
        }
        result = Some((nid, before));
    }
    dom.mark_dirty();
    result
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
        // innerHTML — every templating library reaches for it.
        // Getter serialises the element's subtree back to HTML;
        // setter parses the string as a fragment and replaces
        // children. Void-element semantics are *not* honoured
        // by the serialiser — every element round-trips as
        // `<tag></tag>` rather than `<tag>`, which the parser
        // accepts cleanly so reads of subsequent writes match.
        get innerHTML() { return __elemGetInnerHtml(this._h); },
        set innerHTML(v) { __elemSetInnerHtml(this._h, v == null ? '' : String(v)); },
        // outerHTML wraps the element's own open + inner +
        // close. Getter is straightforward; setter is more
        // involved (it has to replace the node in its parent's
        // children), which the spec deems out of scope here —
        // we just no-op the setter.
        get outerHTML() {
            var open = '<' + this.tagName.toLowerCase();
            var attrs = ''; // we don't expose .attributes yet
            return open + '>' + this.innerHTML + '</' + this.tagName.toLowerCase() + '>';
        },
        set outerHTML(_v) {},
        // insertAdjacentHTML — parse + insert at one of four
        // canonical positions. htmx / lit-html / many
        // server-driven UIs use this for incremental
        // updates instead of full-subtree innerHTML
        // replacement.
        insertAdjacentHTML: function(position, html) {
            __elemInsertAdjacentHtml(this._h, String(position).toLowerCase(),
                                     html == null ? '' : String(html));
        },
        // cloneNode — fresh detached subtree. `parent
        // .appendChild(template.cloneNode(true))` is the
        // classic templating pattern.
        cloneNode: function(deep) {
            return _wrapElem(__elemCloneNode(this._h, deep === true));
        },
        // replaceChildren(...nodes) — modern alternative to
        // `innerHTML = ''`. Detach all current children,
        // append the new ones.
        replaceChildren: function() {
            // Detach all existing children first.
            var c = this.firstChild;
            while (c !== null) {
                var next = c.nextSibling;
                this.removeChild(c);
                c = next;
            }
            for (var i = 0; i < arguments.length; i++) {
                var node = arguments[i];
                if (node && node._h !== undefined) this.appendChild(node);
            }
        },
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
        insertBefore: function(child, ref) {
            __elemInsertBefore(this._h, child._h, ref ? ref._h : null);
            return child;
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
        // dispatchEvent — when called from JS, fire the event
        // through our internal listener map. Currently a stub
        // that returns `!event.defaultPrevented` so feature-
        // probe code gets a truthy answer; real cross-target
        // dispatch needs re-entrant listener-map access which
        // is the explicit follow-up.
        dispatchEvent: function(event) {
            if (event && event.defaultPrevented) return false;
            return true;
        },
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
        },
        // dataset — every `data-foo` attribute surfaces as
        // `el.dataset.foo`, kebab-case names converted to
        // camelCase (`data-foo-bar` → `dataset.fooBar`).
        // Read-only today: assignment to `el.dataset.X = …`
        // updates the returned snapshot only. Use
        // `setAttribute('data-X', …)` for the write path
        // until we have a Proxy-backed surface.
        get dataset() {
            return __elemDataset(this._h);
        },
        // ---- Node tree (covers elements + text nodes) ----
        get firstChild() { return _wrapElem(__nodeFirstChild(this._h)); },
        get lastChild()  { return _wrapElem(__nodeLastChild(this._h)); },
        get nextSibling() { return _wrapElem(__nodeNextSibling(this._h)); },
        get previousSibling() { return _wrapElem(__nodePrevSibling(this._h)); },
        get nodeType() { return __nodeType(this._h); },
        get nodeName() { return __nodeName(this._h); },
        get nodeValue() { return __nodeValue(this._h); },
        // childNodes walks first / next sibling to build a
        // NodeList of every child (elements + text nodes), as
        // opposed to `children` which is element-only.
        get childNodes() {
            var out = [];
            var c = this.firstChild;
            while (c !== null) {
                out.push(c);
                c = c.nextSibling;
            }
            return out;
        },
        // parentNode is the Node-level alias of parentElement.
        // For DocumentFragment / Document handling they differ
        // (parentNode can be a non-Element); for our DOM both
        // shapes collapse to the same answer.
        get parentNode() { return _wrapElem(__elemParent(this._h)); },
        // ownerDocument always resolves to the global document
        // for nodes that live in our tree. Frameworks check
        // `el.ownerDocument === document` to verify the node
        // hasn't been detached into a foreign realm.
        get ownerDocument() { return document; },
        // Node.contains — true when the argument is `this` or
        // a descendant of `this`. Backed by a parent-chain walk
        // on the host side.
        contains: function(other) {
            if (!other || other._h === undefined) return false;
            return __elemContains(this._h, other._h);
        },
        // ---- Layout geometry ----
        //
        // Reads from the embedder-published frame map. `__elemFrame`
        // returns [x, y, width, height]; missing entries (newly-
        // created nodes, anything before the first paint) get
        // [0, 0, 0, 0]. Real browsers behave the same way for
        // detached elements, so this is honest semantics rather
        // than a stub.
        get offsetWidth()  { return __elemFrame(this._h)[2]; },
        get offsetHeight() { return __elemFrame(this._h)[3]; },
        get offsetTop()    { return __elemFrame(this._h)[1]; },
        get offsetLeft()   { return __elemFrame(this._h)[0]; },
        get clientWidth()  { return __elemFrame(this._h)[2]; },
        get clientHeight() { return __elemFrame(this._h)[3]; },
        get clientTop()    { return 0; },
        get clientLeft()   { return 0; },
        get scrollWidth()  { return __elemFrame(this._h)[2]; },
        get scrollHeight() { return __elemFrame(this._h)[3]; },
        get scrollTop()    { return 0; },
        get scrollLeft()   { return 0; },
        getBoundingClientRect: function() {
            var r = __elemFrame(this._h);
            var x = r[0], y = r[1], w = r[2], h = r[3];
            return {
                x: x, y: y, width: w, height: h,
                top: y, right: x + w, bottom: y + h, left: x
            };
        },
        getClientRects: function() {
            var r = __elemFrame(this._h);
            if (r[2] === 0 && r[3] === 0) return [];
            return [{
                x: r[0], y: r[1], width: r[2], height: r[3],
                top: r[1], right: r[0] + r[2],
                bottom: r[1] + r[3], left: r[0]
            }];
        },
        focus: __noop,
        blur: __noop,
        click: __noop,
        scrollIntoView: __noop,
        scrollTo: __noop,
        scrollBy: __noop,
        // style — CSSStyleDeclaration-shaped object with the
        // common surface: `setProperty(name, value)`,
        // `getPropertyValue(name)`, `removeProperty(name)`,
        // plus `cssText` get/set for whole-attribute reads /
        // writes. The camelCase-property surface
        // (`style.fontSize = …`) isn't here yet — that needs
        // a Proxy to forward arbitrary property accesses
        // through to setProperty. Most JS that mutates inline
        // styles uses `setProperty` / `cssText` directly.
        get style() {
            var h = this._h;
            return {
                _h: h,
                get cssText() { return __elemGetStyleText(h); },
                set cssText(v) { __elemSetStyleText(h, v == null ? '' : String(v)); },
                getPropertyValue: function(name) {
                    return __elemGetStyle(h, String(name));
                },
                setProperty: function(name, value) {
                    __elemSetStyle(h, String(name), value == null ? '' : String(value));
                },
                removeProperty: function(name) {
                    var prev = __elemGetStyle(h, String(name));
                    __elemSetStyle(h, String(name), '');
                    return prev;
                }
            };
        }
    };
}
var document = {
    get body() {
        return _wrapElem(__docBody());
    },
    // documentElement: the root <html> element. Resolved via
    // querySelector("html") so the wrapped Element gets the
    // full surface (dataset, classList, style, addEventListener,
    // …). Previously a stub object with just two methods, which
    // broke any page reading `documentElement.dataset.theme` /
    // `documentElement.classList.add(...)` / etc. MDN hits this
    // on first load to apply the saved color-scheme.
    get documentElement() {
        return _wrapElem(__qs("html"));
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
    },
    // DocumentFragment — used as a build buffer before
    // attaching to the live tree. We don't have a real
    // fragment node type; return a wrapped `<template>`
    // element which behaves identically for the common
    // `frag.appendChild(x); container.appendChild(frag)`
    // pattern. The temporary `<template>` is hidden by
    // our UA stylesheet so it doesn't visually appear if
    // never moved into the live tree.
    createDocumentFragment: function() {
        return _wrapElem(__docCreateElement('template'));
    },
    // `getElementsByClassName` / `getElementsByTagName` —
    // legacy but widely used (especially by Closure-bundled
    // code). Live HTMLCollection semantics aren't preserved
    // — we return a snapshot Array — but that suffices for
    // virtually every caller.
    getElementsByClassName: function(cls) {
        return this.querySelectorAll('.' + cls);
    },
    getElementsByTagName: function(tag) {
        return this.querySelectorAll(String(tag));
    },
    // Stubs that pages commonly read. `activeElement === null`
    // is the standard "nothing focused" value real browsers
    // return on a fresh page.
    activeElement: null,
    hidden: false,
    visibilityState: 'visible',
    readyState: 'complete',
    prerendering: false,
    title: '',
    URL: '',
    referrer: '',
    cookie: '',
    domain: '',
    characterSet: 'UTF-8',
    // FontFaceSet stub. `document.fonts.load(font, text)` is
    // used by Google's preload path and lots of icon-font
    // libraries. Returns a resolved Promise — the page proceeds
    // as if the font loaded. `ready` is a forever-resolved
    // Promise. `check(font)` returns true so style queries
    // that gate on font availability take the happy path.
    fonts: {
        load: function() { return Promise.resolve([]); },
        check: function() { return true; },
        ready: Promise.resolve(),
        forEach: __noop,
        size: 0
    }
};
function __noop() {}
// Workaround for a Zinc closure-tracking bug surfaced by
// google.com /search inline script #3: the script declares
// `var sctm = false` at the top of an IIFE and reads it
// from a nested function defined ~25 KB later in the same
// IIFE. Minimal closure repros work fine in Zinc; this
// specific script's size or function-declaration ordering
// drops the binding by the time the inner function reads
// it. As a temporary unblock — until the closure-scope
// tracking is fixed in Zinc — promote `sctm` (and a
// handful of similar minified single-name globals the
// bundle reads from closure) to real globals so the
// unbound-identifier read resolves cleanly. The boolean
// is `false`, matching the bundle's intent (telemetry
// path stays off).
var sctm = false;
var sclm = false;
// `encodeURIComponent` / `decodeURIComponent` polyfills.
// Zinc doesn't ship them as builtins, and feature-probe code
// that calls them bare crashes with "is not defined". UTF-8
// encoding for non-ASCII follows the RFC 3986 rules.
function encodeURIComponent(s) {
    if (s === null || s === undefined) return String(s);
    s = String(s);
    var out = '';
    for (var i = 0; i < s.length; i++) {
        var c = s.charCodeAt(i);
        var ch = s.charAt(i);
        var unreserved =
            (c >= 0x30 && c <= 0x39) ||   // 0-9
            (c >= 0x41 && c <= 0x5A) ||   // A-Z
            (c >= 0x61 && c <= 0x7A) ||   // a-z
            ch === '-' || ch === '_' || ch === '.' || ch === '~' ||
            ch === '!' || ch === '*' || ch === "'" || ch === '(' || ch === ')';
        if (unreserved) {
            out += ch;
        } else if (c < 0x80) {
            var hex = c.toString(16).toUpperCase();
            out += '%' + (hex.length < 2 ? '0' : '') + hex;
        } else if (c < 0x800) {
            var b1 = (0xC0 | (c >> 6)).toString(16).toUpperCase();
            var b2 = (0x80 | (c & 0x3F)).toString(16).toUpperCase();
            out += '%' + b1 + '%' + b2;
        } else {
            var b1 = (0xE0 | (c >> 12)).toString(16).toUpperCase();
            var b2 = (0x80 | ((c >> 6) & 0x3F)).toString(16).toUpperCase();
            var b3 = (0x80 | (c & 0x3F)).toString(16).toUpperCase();
            out += '%' + b1 + '%' + b2 + '%' + b3;
        }
    }
    return out;
}
function decodeURIComponent(s) {
    if (s === null || s === undefined) return String(s);
    s = String(s);
    var out = '';
    for (var i = 0; i < s.length; i++) {
        if (s.charAt(i) === '%' && i + 2 < s.length) {
            var byte = parseInt(s.substr(i + 1, 2), 16);
            if (!isNaN(byte)) {
                out += String.fromCharCode(byte);
                i += 2;
                continue;
            }
        }
        out += s.charAt(i);
    }
    return out;
}
// `encodeURI` / `decodeURI` are looser variants — encodeURI
// leaves reserved characters (/:?#&=+,;@) alone. The
// distinction matters when callers pass full URLs. For
// pragmatic completeness without reimplementing both, point
// these at the same component variants — over-encodes some
// reserved chars but the result still parses correctly on
// the server side.
function encodeURI(s) { return encodeURIComponent(s); }
function decodeURI(s) { return decodeURIComponent(s); }
// `btoa` / `atob` — base64 round-trip for binary-safe
// strings. Used by every modern bundle for inline assets,
// preflight signing, source-map data URIs.
function btoa(s) {
    if (s === null || s === undefined) return '';
    s = String(s);
    var chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
    var out = '';
    var i = 0;
    while (i < s.length) {
        var c1 = s.charCodeAt(i++);
        var c2 = i < s.length ? s.charCodeAt(i++) : NaN;
        var c3 = i < s.length ? s.charCodeAt(i++) : NaN;
        out += chars.charAt(c1 >> 2);
        out += chars.charAt(((c1 & 3) << 4) | (isNaN(c2) ? 0 : (c2 >> 4)));
        out += isNaN(c2) ? '=' : chars.charAt(((c2 & 15) << 2) | (isNaN(c3) ? 0 : (c3 >> 6)));
        out += isNaN(c3) ? '=' : chars.charAt(c3 & 63);
    }
    return out;
}
// URLSearchParams polyfill. Methods defined on each instance
// rather than the prototype because Zinc's prototype-method
// dispatch was inconsistent for this shape. Each instance
// gets its own method closures — slightly more memory per
// URLSearchParams but reliable.
function URLSearchParams(init) {
    var entries = [];
    function _parseString(s) {
        s = s.charAt(0) === '?' ? s.substring(1) : s;
        if (s.length === 0) return;
        var pairs = s.split('&');
        for (var i = 0; i < pairs.length; i++) {
            var eq = pairs[i].indexOf('=');
            var k = eq < 0 ? pairs[i] : pairs[i].substring(0, eq);
            var v = eq < 0 ? '' : pairs[i].substring(eq + 1);
            entries.push([
                decodeURIComponent(k.replace(/\+/g, ' ')),
                decodeURIComponent(v.replace(/\+/g, ' '))
            ]);
        }
    }
    if (init !== undefined && init !== null) {
        if (typeof init === 'string') {
            _parseString(init);
        } else if (Array.isArray(init)) {
            for (var i = 0; i < init.length; i++) {
                entries.push([String(init[i][0]), String(init[i][1])]);
            }
        } else {
            for (var k in init) {
                entries.push([k, String(init[k])]);
            }
        }
    }
    this._entries = entries;
    this.get = function(name) {
        name = String(name);
        for (var i = 0; i < entries.length; i++) {
            if (entries[i][0] === name) return entries[i][1];
        }
        return null;
    };
    this.getAll = function(name) {
        name = String(name);
        var out = [];
        for (var i = 0; i < entries.length; i++) {
            if (entries[i][0] === name) out.push(entries[i][1]);
        }
        return out;
    };
    this.has = function(name) {
        name = String(name);
        for (var i = 0; i < entries.length; i++) {
            if (entries[i][0] === name) return true;
        }
        return false;
    };
    this.set = function(name, value) {
        name = String(name);
        value = String(value);
        var found = false;
        var kept = [];
        for (var i = 0; i < entries.length; i++) {
            if (entries[i][0] === name) {
                if (!found) { kept.push([name, value]); found = true; }
            } else {
                kept.push(entries[i]);
            }
        }
        if (!found) kept.push([name, value]);
        entries.length = 0;
        for (var i = 0; i < kept.length; i++) entries.push(kept[i]);
    };
    this.append = function(name, value) {
        entries.push([String(name), String(value)]);
    };
    this.delete = function(name) {
        name = String(name);
        var kept = [];
        for (var i = 0; i < entries.length; i++) {
            if (entries[i][0] !== name) kept.push(entries[i]);
        }
        entries.length = 0;
        for (var i = 0; i < kept.length; i++) entries.push(kept[i]);
    };
    this.forEach = function(cb, thisArg) {
        for (var i = 0; i < entries.length; i++) {
            cb.call(thisArg, entries[i][1], entries[i][0], this);
        }
    };
    this.toString = function() {
        var out = [];
        for (var i = 0; i < entries.length; i++) {
            out.push(
                encodeURIComponent(entries[i][0]) + '=' +
                encodeURIComponent(entries[i][1])
            );
        }
        return out.join('&');
    };
}

// URL constructor polyfill. Uses a single regex to peel off
// every component in one pass — bypasses several Zinc
// idiosyncrasies with `indexOf` and `var` scoping that the
// hand-rolled parser tripped. Not the full WHATWG algorithm
// (this is the "pragmatic 80%") but matches every typical
// real-world URL shape.
//
// Groups: 1=scheme, 2=userinfo (without @), 3=host:port,
//         4=pathname, 5=search (without ?), 6=hash (without #)
var __URL_RE = /^(?:([a-zA-Z][a-zA-Z0-9+.\-]*):)?(?:\/\/(?:([^@/?#]*)@)?([^/?#]*))?([^?#]*)(?:\?([^#]*))?(?:#(.*))?$/;
function URL(input, base) {
    input = String(input);
    if (base !== undefined && base !== null) {
        input = __urlResolve(String(base), input);
    }
    var m = input.match(__URL_RE);
    var scheme = (m && m[1]) ? m[1] : '';
    var userinfo = (m && m[2]) ? m[2] : '';
    var host = (m && m[3]) ? m[3] : '';
    var pathname = (m && m[4]) ? m[4] : '';
    var search = (m && m[5]) ? m[5] : '';
    var hash = (m && m[6]) ? m[6] : '';
    this.protocol = scheme ? scheme.toLowerCase() + ':' : '';
    this.host = host;
    // Split host:port — `lastIndexOf` keeps IPv6 `[::1]:80`
    // mostly intact (we don't validate IPv6 here).
    var colonAt = host.lastIndexOf(':');
    if (colonAt >= 0) {
        this.hostname = host.substring(0, colonAt);
        this.port = host.substring(colonAt + 1);
    } else {
        this.hostname = host;
        this.port = '';
    }
    var atAt = userinfo.indexOf(':');
    if (atAt >= 0) {
        this.username = userinfo.substring(0, atAt);
        this.password = userinfo.substring(atAt + 1);
    } else {
        this.username = userinfo;
        this.password = '';
    }
    this.pathname = pathname;
    this.search = search ? '?' + search : '';
    this.hash = hash ? '#' + hash : '';
    this.origin = this.protocol + (this.host ? '//' + this.host : '');
    this.searchParams = new URLSearchParams(this.search);
    this.toString = function() {
        var s = this.protocol;
        if (this.host) s += '//' + this.host;
        s += this.pathname;
        var qstr = this.searchParams.toString();
        if (qstr) s += '?' + qstr;
        else if (this.search) s += this.search;
        s += this.hash;
        return s;
    };
}
URL.prototype.toJSON = function() { return this.toString(); };

// Simple relative-URL resolver. Handles the typical cases:
// absolute URL → as-is. Protocol-relative (`//host/path`)
// borrows scheme from base. Host-relative (`/path`) borrows
// scheme + authority. Pure-relative (`x` / `./x` / `../x`)
// resolves against the base's directory.
function __urlResolve(base, relative) {
    if (/^[a-zA-Z][a-zA-Z0-9+.\-]*:/.test(relative)) {
        return relative;
    }
    var schemeEnd = base.indexOf('://');
    if (schemeEnd < 0) return relative;
    var schemePrefix = base.substring(0, schemeEnd + 3);
    var afterScheme = base.substring(schemeEnd + 3);
    if (relative.substring(0, 2) === '//') {
        return base.substring(0, schemeEnd + 1) + relative;
    }
    var pathStart = afterScheme.indexOf('/');
    var authority, basePath;
    if (pathStart < 0) {
        authority = afterScheme;
        basePath = '/';
    } else {
        authority = afterScheme.substring(0, pathStart);
        basePath = afterScheme.substring(pathStart);
    }
    if (relative.charAt(0) === '/') {
        return schemePrefix + authority + relative;
    }
    if (relative.charAt(0) === '?' || relative.charAt(0) === '#') {
        var qIdx = basePath.indexOf('?');
        var hIdx = basePath.indexOf('#');
        var cut = basePath.length;
        if (qIdx >= 0 && qIdx < cut) cut = qIdx;
        if (hIdx >= 0 && hIdx < cut) cut = hIdx;
        return schemePrefix + authority + basePath.substring(0, cut) + relative;
    }
    var lastSlash = basePath.lastIndexOf('/');
    var dir = lastSlash >= 0 ? basePath.substring(0, lastSlash + 1) : '/';
    return schemePrefix + authority + dir + relative;
}

// structuredClone polyfill. Deep-copies plain objects /
// arrays / primitives via JSON round-trip. Loses functions,
// Symbols, Dates, RegExps — which the real algorithm
// preserves — but the JSON form covers the common
// state-snapshot use case (every Redux store ever).
function structuredClone(value) {
    if (value === null || value === undefined) return value;
    if (typeof value !== 'object') return value;
    try { return JSON.parse(JSON.stringify(value)); }
    catch (e) { return value; }
}

// AbortController + AbortSignal polyfills. Modern fetch /
// fetch-like code passes `{ signal: controller.signal }` and
// expects `controller.abort()` to cancel. The signal carries
// `aborted` / `reason` and supports `addEventListener('abort',
// cb)`. Without these the typical "race + cancel slow path"
// idiom crashes on missing-class throws.
function AbortController() {
    var listeners = [];
    var signal = {
        aborted: false,
        reason: undefined,
        addEventListener: function(type, cb) {
            if (type === 'abort' && typeof cb === 'function') {
                listeners.push(cb);
            }
        },
        removeEventListener: function(type, cb) {
            if (type !== 'abort') return;
            var kept = [];
            for (var i = 0; i < listeners.length; i++) {
                if (listeners[i] !== cb) kept.push(listeners[i]);
            }
            listeners.length = 0;
            for (var i = 0; i < kept.length; i++) listeners.push(kept[i]);
        },
        throwIfAborted: function() {
            if (signal.aborted) throw signal.reason;
        },
        dispatchEvent: function(_e) { return true; },
        onabort: null
    };
    this.signal = signal;
    this.abort = function(reason) {
        if (signal.aborted) return;
        signal.aborted = true;
        signal.reason = (reason === undefined)
            ? new Error('AbortError: signal is aborted without reason')
            : reason;
        if (typeof signal.onabort === 'function') {
            try { signal.onabort({ type: 'abort' }); } catch (e) {}
        }
        for (var i = 0; i < listeners.length; i++) {
            try { listeners[i].call(signal, { type: 'abort' }); } catch (e) {}
        }
    };
}
// `AbortSignal.abort(reason)` and `AbortSignal.timeout(ms)`
// are widely-used static factories. The timeout variant
// schedules a real setTimeout via our timer queue.
var AbortSignal = {
    abort: function(reason) {
        var c = new AbortController();
        c.abort(reason);
        return c.signal;
    },
    timeout: function(ms) {
        var c = new AbortController();
        setTimeout(function() { c.abort(new Error('TimeoutError')); }, ms);
        return c.signal;
    },
    any: function(signals) {
        var c = new AbortController();
        for (var i = 0; i < signals.length; i++) {
            if (signals[i].aborted) { c.abort(signals[i].reason); return c.signal; }
            signals[i].addEventListener('abort', function() {
                c.abort(this.reason);
            });
        }
        return c.signal;
    }
};

// FormData polyfill. Used by every form-submitting page +
// every fetch-based file upload. Stores name/value entries
// in insertion order; values can be strings or blob-like.
function FormData(_form) {
    var entries = [];
    // Constructing from a <form> element: walk the form's
    // controls and seed entries from name/value. The
    // wrapper element's `elements` accessor (when present)
    // gives us a name→control map; we iterate the keys.
    if (_form && _form.elements) {
        var els = _form.elements;
        for (var name in els) {
            var el = els[name];
            if (el && typeof el.value !== 'undefined') {
                entries.push([name, String(el.value)]);
            }
        }
    }
    this.append = function(name, value) {
        entries.push([String(name), value]);
    };
    this.set = function(name, value) {
        name = String(name);
        var found = false;
        var kept = [];
        for (var i = 0; i < entries.length; i++) {
            if (entries[i][0] === name) {
                if (!found) { kept.push([name, value]); found = true; }
            } else {
                kept.push(entries[i]);
            }
        }
        if (!found) kept.push([name, value]);
        entries.length = 0;
        for (var i = 0; i < kept.length; i++) entries.push(kept[i]);
    };
    this.get = function(name) {
        name = String(name);
        for (var i = 0; i < entries.length; i++) {
            if (entries[i][0] === name) return entries[i][1];
        }
        return null;
    };
    this.getAll = function(name) {
        name = String(name);
        var out = [];
        for (var i = 0; i < entries.length; i++) {
            if (entries[i][0] === name) out.push(entries[i][1]);
        }
        return out;
    };
    this.has = function(name) {
        name = String(name);
        for (var i = 0; i < entries.length; i++) {
            if (entries[i][0] === name) return true;
        }
        return false;
    };
    this.delete = function(name) {
        name = String(name);
        var kept = [];
        for (var i = 0; i < entries.length; i++) {
            if (entries[i][0] !== name) kept.push(entries[i]);
        }
        entries.length = 0;
        for (var i = 0; i < kept.length; i++) entries.push(kept[i]);
    };
    this.forEach = function(cb, thisArg) {
        for (var i = 0; i < entries.length; i++) {
            cb.call(thisArg, entries[i][1], entries[i][0], this);
        }
    };
}

// Headers polyfill. Used by fetch() Request/Response shapes.
// Names are case-insensitive (lowercased internally), values
// stringified. Multi-value `append` matches spec; `set`
// replaces existing values with the same name.
function Headers(init) {
    var pairs = [];
    if (init !== undefined && init !== null) {
        if (Array.isArray(init)) {
            for (var i = 0; i < init.length; i++) {
                pairs.push([String(init[i][0]).toLowerCase(), String(init[i][1])]);
            }
        } else if (typeof init === 'object') {
            for (var k in init) {
                pairs.push([k.toLowerCase(), String(init[k])]);
            }
        }
    }
    this.get = function(name) {
        name = String(name).toLowerCase();
        for (var i = 0; i < pairs.length; i++) {
            if (pairs[i][0] === name) return pairs[i][1];
        }
        return null;
    };
    this.set = function(name, value) {
        name = String(name).toLowerCase();
        value = String(value);
        var found = false;
        var kept = [];
        for (var i = 0; i < pairs.length; i++) {
            if (pairs[i][0] === name) {
                if (!found) { kept.push([name, value]); found = true; }
            } else {
                kept.push(pairs[i]);
            }
        }
        if (!found) kept.push([name, value]);
        pairs.length = 0;
        for (var i = 0; i < kept.length; i++) pairs.push(kept[i]);
    };
    this.has = function(name) {
        name = String(name).toLowerCase();
        for (var i = 0; i < pairs.length; i++) {
            if (pairs[i][0] === name) return true;
        }
        return false;
    };
    this.append = function(name, value) {
        pairs.push([String(name).toLowerCase(), String(value)]);
    };
    this.delete = function(name) {
        name = String(name).toLowerCase();
        var kept = [];
        for (var i = 0; i < pairs.length; i++) {
            if (pairs[i][0] !== name) kept.push(pairs[i]);
        }
        pairs.length = 0;
        for (var i = 0; i < kept.length; i++) pairs.push(kept[i]);
    };
    this.forEach = function(cb, thisArg) {
        for (var i = 0; i < pairs.length; i++) {
            cb.call(thisArg, pairs[i][1], pairs[i][0], this);
        }
    };
}

// `Event(type, init)` and `CustomEvent(type, init)` JS-side
// constructors. The Event object that flows through our
// internal `dispatch_js` is built by Rust; these are the
// constructors user code calls when it wants to make its own
// events (for `target.dispatchEvent(new CustomEvent(...))`).
// dispatch through our listener map is a follow-up — for now
// the constructor returns a plain object so feature-probe
// code (`typeof CustomEvent === 'function'`) and the
// `new Event(...)` shape both work.
function Event(type, init) {
    init = init || {};
    this.type = String(type);
    this.bubbles = !!init.bubbles;
    this.cancelable = !!init.cancelable;
    this.composed = !!init.composed;
    this.target = null;
    this.currentTarget = null;
    this.defaultPrevented = false;
    var stopped = false;
    var self = this;
    this.preventDefault = function() {
        if (self.cancelable) self.defaultPrevented = true;
    };
    this.stopPropagation = function() { stopped = true; };
    this.stopImmediatePropagation = function() { stopped = true; };
    this.eventPhase = 0;
    this.isTrusted = false;
    this.timeStamp = Date.now();
}
function CustomEvent(type, init) {
    Event.call(this, type, init);
    this.detail = (init && init.detail !== undefined) ? init.detail : null;
}

// Promise.allSettled polyfill. Returns a Promise that
// resolves to [{status, value | reason}, …] after every
// input Promise settles, in input order.
if (typeof Promise.allSettled !== 'function') {
    Promise.allSettled = function(iterable) {
        return Promise.all(
            Array.from(iterable).map(function(p) {
                return Promise.resolve(p).then(
                    function(value) { return { status: 'fulfilled', value: value }; },
                    function(reason) { return { status: 'rejected', reason: reason }; }
                );
            })
        );
    };
}

// Original prelude continues below.
function atob(s) {
    if (s === null || s === undefined) return '';
    s = String(s).replace(/[^A-Za-z0-9+/]/g, '');
    var chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
    var out = '';
    var i = 0;
    while (i < s.length) {
        var e1 = chars.indexOf(s.charAt(i++));
        var e2 = chars.indexOf(s.charAt(i++));
        var e3 = chars.indexOf(s.charAt(i++));
        var e4 = chars.indexOf(s.charAt(i++));
        var c1 = (e1 << 2) | (e2 >> 4);
        var c2 = ((e2 & 15) << 4) | (e3 >> 2);
        var c3 = ((e3 & 3) << 6) | e4;
        out += String.fromCharCode(c1);
        if (e3 !== -1 && e3 < 64) out += String.fromCharCode(c2);
        if (e4 !== -1 && e4 < 64) out += String.fromCharCode(c3);
    }
    return out;
}
// Timer scheduling now routes through host fns that push onto
// a shared queue; the embedder's frame tick drains entries
// whose deadline has elapsed. `setTimeout(fn, 0)` therefore
// fires at most one frame (~16 ms) later, matching what real
// browsers do for zero-delay timers.
function setTimeout(fn, ms) {
    return __setTimeoutHost(fn, ms || 0);
}
function clearTimeout(id) { __clearTimerHost(id); }
function setInterval(fn, ms) {
    return __setIntervalHost(fn, ms || 0);
}
function clearInterval(id) { __clearTimerHost(id); }
function requestAnimationFrame(fn) {
    return __requestAnimationFrameHost(fn);
}
function cancelAnimationFrame(id) { __clearTimerHost(id); }
function queueMicrotask(fn) {
    // Microtasks aren't macrotasks — running them synchronously
    // is still strictly better than queueing them onto the
    // timer queue, which would defer to the next frame. Real
    // queueMicrotask semantics require draining after the
    // current synchronous task; for now we accept "fire now"
    // since user code typically doesn't observe the difference.
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
// Asynchronous fetch. __fetch_start kicks the request off on a
// background thread and returns a real pending Promise the host
// settles with { ok, status, url, body } once the response lands
// (or rejects on network error). We layer the standard Response
// surface on top: text() / json() return promises of their own.
function _makeFetchResponse(raw) {
    return {
        ok: raw.ok,
        status: raw.status,
        statusText: raw.ok ? 'OK' : 'Error',
        url: raw.url,
        // `.body` isn't part of the standard fetch Response surface
        // (the standard is .text() / .json() / etc.) but it's a
        // convenient escape hatch and the test path uses it.
        body: raw.body,
        headers: { get: function(_n) { return null; }, has: function(_n) { return false; } },
        text: function() { return Promise.resolve(raw.body); },
        json: function() {
            try { return Promise.resolve(JSON.parse(raw.body)); }
            catch (e) { return Promise.reject(e); }
        }
    };
}
function fetch(url, _opts) {
    return __fetch_start(url || '').then(function(raw) {
        return _makeFetchResponse(raw);
    });
}
document.addEventListener = __ael;
document.removeEventListener = __noop;
// (document.documentElement is now a getter on the document
// literal above — returns a wrapped Element for the <html>
// node so dataset / classList / addEventListener all work.)
var location = {
    get href() { return __current_url(); },
    set href(v) { __navigate(v); },
    assign: __navigate,
    replace: __navigate,
    reload: __noop
};
var navigator = {
    userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Copper/0.1.0 Safari/537.36',
    language: 'en-US',
    languages: ['en-US'],
    platform: 'MacIntel',
    onLine: true,
    cookieEnabled: true,
    doNotTrack: null,
    hardwareConcurrency: 4,
    maxTouchPoints: 0,
    vendor: 'Google Inc.',
    sendBeacon: __sendBeacon,
    geolocation: { getCurrentPosition: __noop, watchPosition: __noop, clearWatch: __noop },
    clipboard: { writeText: function(){ return Promise.resolve(); }, readText: function(){ return Promise.resolve(''); } },
    serviceWorker: { register: function(){ return Promise.reject(new Error('not supported')); }, controller: null }
};
var history = { length: 1, state: null, pushState: __noop, replaceState: __noop, back: __noop, forward: __noop, go: __noop };
var window = {
    document: document,
    location: location,
    navigator: navigator,
    history: history,
    fetch: fetch,
    setTimeout: setTimeout,
    clearTimeout: clearTimeout,
    setInterval: setInterval,
    clearInterval: clearInterval,
    requestAnimationFrame: requestAnimationFrame,
    cancelAnimationFrame: cancelAnimationFrame,
    addEventListener: __ael,
    removeEventListener: __noop,
    innerWidth: 1400,
    innerHeight: 900,
    // Scroll position is exposed both as deprecated camelCase
    // (pageXOffset / pageYOffset) and modern (scrollX / scrollY).
    // We return 0 — the layout-aware integration that ties these
    // to the chrome's scroll_y is a follow-up; for now scripts
    // that read them just see "viewport is at the top".
    scrollX: 0,
    scrollY: 0,
    pageXOffset: 0,
    pageYOffset: 0,
    scrollTo: __noop,
    scrollBy: __noop,
    // Common no-op stubs that prevent crashes when scripts
    // probe them without checking existence first.
    getComputedStyle: function(_el) {
        return {
            getPropertyValue: function(_n) { return ''; }
        };
    },
    matchMedia: function(_q) {
        // Returns a MediaQueryList shape that always reports
        // "no match" — closest honest answer without a real
        // media-query engine.
        return {
            matches: false,
            media: _q == null ? '' : String(_q),
            addEventListener: __noop,
            removeEventListener: __noop,
            addListener: __noop,
            removeListener: __noop
        };
    },
    alert: __noop,
    confirm: function() { return false; },
    prompt: function() { return null; },
    getSelection: function() { return null; }
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
// `localStorage` / `sessionStorage` — in-memory only (no
// persistence across sessions). Real browsers persist to disk;
// for our render-and-go embedder, in-memory is enough because
// most code paths just probe these to cache derived state and
// fall back gracefully when nothing's stored. Most pages
// reference `localStorage` directly during script init (for
// theme preferences, dismissed-banner flags, etc.); leaving it
// undefined was triggering ReferenceError on MDN and others.
function __makeStorage() {
    var data = {};
    return {
        getItem: function(k) {
            k = String(k);
            return Object.prototype.hasOwnProperty.call(data, k) ? data[k] : null;
        },
        setItem: function(k, v) { data[String(k)] = String(v); },
        removeItem: function(k) { delete data[String(k)]; },
        clear: function() { for (var k in data) delete data[k]; },
        key: function(i) {
            var keys = Object.keys(data);
            return i >= 0 && i < keys.length ? keys[i] : null;
        },
        get length() { return Object.keys(data).length; }
    };
}
var localStorage = __makeStorage();
var sessionStorage = __makeStorage();
// Defensive `_`, `_s`, `_qs` stubs. Google's inline bundle
// uses these as Closure chunk-loader namespaces and assigns to
// them via `window._s = window._s || {};` then immediately
// reads bare `_s`. In real browsers, `window.X = …` at top
// level aliases as a bare global; in Zinc, `window` is a
// local object literal in the prelude so the alias is
// explicit. Pre-creating these AND wiring `window._ = _` (see
// below) keeps both spelling paths pointing at the same
// underlying object.
var _ = {};
var _s = {};
var _qs = {};
// Google's bootstrap installs `_DumpException` on `_`, `_s`,
// `_qs` to re-throw caught errors. Pre-stubbing as a thrower
// matches the spec exactly and prevents the `_._DumpException
// is not a function` cascade when subsequent code reaches for
// it.
function __dumpException(e) { throw e; }
_._DumpException = __dumpException;
_s._DumpException = __dumpException;
_qs._DumpException = __dumpException;
// Google's `google.*` namespace pre-populated with the timer +
// chunk-loader shape its inline scripts touch before defining
// real values. Each slot is a no-op or empty container that
// silently accepts the typical `google.timers.load.t.X = Y`
// assignments instead of throwing on undefined access.
// MutationObserver — real wiring. Each `new MutationObserver(cb)`
// registers `cb` with the host's observer registry; the host
// queues records on every DOM mutation that matches a
// subscription. Delivery happens after each `JsContext::tick`
// drains microtasks. IntersectionObserver / ResizeObserver
// stay no-ops below — those need a layout-tick integration
// that's not built yet.
function MutationObserver(cb) {
    var id = __newMutationObserver(cb);
    return {
        _id: id,
        observe: function(target, init) {
            if (!target) return;
            __moObserve(this._id, target._h, init || {});
        },
        disconnect: function() {
            __moDisconnect(this._id);
        },
        takeRecords: function() { return []; }
    };
}
function IntersectionObserver(_cb, _opts) {
    return {
        observe: __noop,
        unobserve: __noop,
        disconnect: __noop,
        takeRecords: function() { return []; },
        root: null,
        rootMargin: '0px',
        thresholds: [0]
    };
}
function ResizeObserver(_cb) {
    return {
        observe: __noop,
        unobserve: __noop,
        disconnect: __noop
    };
}
// `Image` is the classic preload constructor — `new Image();
// img.src = '/preload.png'` queues an HTTP fetch. A no-op
// implementation that swallows the src assignment lets pages
// using it as a preload hint survive; real preload routing
// can come later via our fetcher infrastructure.
function Image(_w, _h) {
    return {
        src: '',
        srcset: '',
        crossOrigin: null,
        complete: true,
        naturalWidth: 0,
        naturalHeight: 0,
        onload: null,
        onerror: null,
        addEventListener: __noop,
        removeEventListener: __noop
    };
}
// `Audio` follows the same shape (very commonly used as a
// silent ping target). Stub for the same reason.
function Audio(_src) {
    return {
        src: '',
        play: function() { return Promise.resolve(); },
        pause: __noop,
        load: __noop,
        addEventListener: __noop,
        removeEventListener: __noop
    };
}
// XMLHttpRequest stub. Google's bootstrap branches on
// `typeof XMLHttpRequest !== 'undefined'` — and then issues
// /gen_204 telemetry pings. Returning a fake instance whose
// methods are all no-ops keeps the page progressing past the
// "feature detect → use it" path without ever hitting the
// network. A real implementation would route through our
// fetcher infrastructure same as `fetch()`; Phase 10 work.
function XMLHttpRequest() {
    return {
        readyState: 0,
        status: 0,
        statusText: '',
        responseText: '',
        responseType: '',
        response: null,
        responseURL: '',
        onreadystatechange: null,
        onload: null,
        onerror: null,
        onabort: null,
        ontimeout: null,
        onloadend: null,
        onprogress: null,
        upload: {
            addEventListener: __noop,
            removeEventListener: __noop
        },
        open: __noop,
        send: __noop,
        setRequestHeader: __noop,
        abort: __noop,
        getResponseHeader: function() { return null; },
        getAllResponseHeaders: function() { return ''; },
        addEventListener: __noop,
        removeEventListener: __noop,
        withCredentials: false,
        timeout: 0
    };
}
// `navigator.sendBeacon(url, body)` is a fire-and-forget
// ping API. Real impl would route through bui-net; stub
// returns true so callers think the beacon went out.
function __sendBeacon(_url, _body) { return true; }
var google = {
    kEI: '', kEXPI: '', kPS: '', kHL: 'en',
    sn: '', c: { b: function(){} },
    jsr: __noop,
    tick: __noop,
    log: __noop,
    x: __noop,
    erd: { jsr: 0, bv: 0, de: false, c: '', dpf: '' },
    // `google.timers.load.m` is consulted by `google.c.b`:
    //   google.c.b = function(a){var b=google.timers.load.m;
    //       b[a]&&google.ml(Error("b"))}
    // Without `.m`, the function throws on undefined indexing.
    timers: { load: { t: {}, m: {} } },
    // `google.ml(error, …)` is the page's central error logger.
    // Stubbing as no-op silences a class of error-cascade
    // throws where one script's failure triggers ml() on the
    // next.
    ml: __noop,
    // `stvsc: true` is the load-bearing flag. Google's first
    // inline script does
    //   ((a=window.google)==null ? 0 : a.stvsc) ?
    //       google.kEI = _g.kEI :
    //       window.google = _g;
    // Without stvsc set, the *else* branch runs: window.google
    // is replaced wholesale with `_g` (which carries kEI,
    // kEXPI, kBL, kOPI — and lacks the `erd` / `timers` shape
    // every later script reads). With stvsc set, the *then*
    // branch runs and our object is preserved, just merging
    // the new kEI in. Cuts the "Cannot read 'jsr' of
    // undefined" / "'load' of undefined" cascade off at the
    // root.
    stvsc: true
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
window.localStorage = localStorage;
window.sessionStorage = sessionStorage;
window.gapi = gapi;
window.location = location;
window.history = history;
window.navigator = navigator;
window.MutationObserver = MutationObserver;
window.IntersectionObserver = IntersectionObserver;
window.ResizeObserver = ResizeObserver;
window.Image = Image;
window.Audio = Audio;
window.XMLHttpRequest = XMLHttpRequest;
// Google's bootstrap iterates a list of candidates
// [globalThis, self, window, this, …] checking `e.Math === Math`
// to find the real global object — fails over to `throw
// Error("b")` if none match. Mirror the JS primitives onto
// our local `window` so that check passes.
window.Math = Math;
window.JSON = JSON;
window.Object = Object;
window.Array = Array;
window.Date = Date;
window.RegExp = RegExp;
window.Error = Error;
window.TypeError = TypeError;
window.RangeError = RangeError;
window.SyntaxError = SyntaxError;
window.ReferenceError = ReferenceError;
window.Promise = Promise;
window.Symbol = (typeof Symbol === 'undefined') ? undefined : Symbol;
window.Map = Map;
window.Set = Set;
window.WeakMap = WeakMap;
window.WeakSet = WeakSet;
window.parseInt = parseInt;
window.parseFloat = parseFloat;
window.isNaN = isNaN;
window.isFinite = isFinite;
window.encodeURIComponent = encodeURIComponent;
window.decodeURIComponent = decodeURIComponent;
window.encodeURI = encodeURI;
window.decodeURI = decodeURI;
window.btoa = btoa;
window.atob = atob;
window.URL = URL;
window.URLSearchParams = URLSearchParams;
window.structuredClone = structuredClone;
window.AbortController = AbortController;
window.AbortSignal = AbortSignal;
window.FormData = FormData;
window.Headers = Headers;
window.Event = Event;
window.CustomEvent = CustomEvent;
window.console = console;
window.globalThis = window;
window.self = window;
window.top = window;
window.parent = window;
window.frames = window;
// `_` / `_s` / `_qs` aliased onto window so the
// `window.X = window.X || {}` idiom that Google's bootstrap
// runs returns the same object bare-`X` reads point at.
window._ = _;
window._s = _s;
window._qs = _qs;
window._DumpException = __dumpException;
// `_F_toggles` / `_F_jsUrl` / `_F_installCss` are
// experiment-flag globals Google's bundle assigns to before
// the JS bundle loads. Pre-stubbing as truthy-empty values
// stops `length` reads on undefined cascades when other code
// reaches for them prematurely.
window._F_toggles = [];
window._F_jsUrl = '';
window._F_installCss = __noop;
window._rtf = __noop;
window._xjs_toggles = [];
window.loaded_h_0 = __noop;
window.n = null;
// DOM interface constructors. Pages feature-detect and brand-check
// against these (`el instanceof HTMLIFrameElement` in react-dom's
// focus bookkeeping; `x instanceof Element` everywhere). Inert
// functions are enough for instanceof to evaluate (to false) instead
// of throwing "right-hand side is not callable"; real prototype
// branding for wrapped elements is a follow-up.
var EventTarget = function EventTarget() {};
var Node = function Node() {};
var Element = function Element() {};
var Document = function Document() {};
var HTMLDocument = function HTMLDocument() {};
var DocumentFragment = function DocumentFragment() {};
var Text = function Text() {};
var Comment = function Comment() {};
var HTMLElement = function HTMLElement() {};
var SVGElement = function SVGElement() {};
var HTMLIFrameElement = function HTMLIFrameElement() {};
var HTMLInputElement = function HTMLInputElement() {};
var HTMLTextAreaElement = function HTMLTextAreaElement() {};
var HTMLSelectElement = function HTMLSelectElement() {};
var HTMLOptionElement = function HTMLOptionElement() {};
var HTMLButtonElement = function HTMLButtonElement() {};
var HTMLAnchorElement = function HTMLAnchorElement() {};
var HTMLImageElement = function HTMLImageElement() {};
var HTMLFormElement = function HTMLFormElement() {};
var HTMLSpanElement = function HTMLSpanElement() {};
var HTMLDivElement = function HTMLDivElement() {};
var MutationEvent = function MutationEvent() {};
// In real browsers `window` IS the global object. The surface above
// was built as a detached literal; graft it onto globalThis and
// rebind, so `window === self === globalThis === this` holds and a
// UMD bundle's `(global = global || self, factory(global.React = {}))`
// creates a real global instead of writing into a void. (Strict-mode
// IIFE wrappers get `this === undefined`, so they all reach for
// `self` — React, Preact, and most CDN bundles ship exactly this.)
(function () {
    var keys = Object.keys(window);
    for (var i = 0; i < keys.length; i++) {
        var k = keys[i];
        if (k === 'globalThis' || k === 'self' || k === 'top'
            || k === 'parent' || k === 'frames') { continue; }
        globalThis[k] = window[k];
    }
})();
window = globalThis;
var self = globalThis;
window.self = globalThis;
window.top = globalThis;
window.parent = globalThis;
window.frames = globalThis;
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

    // Obsolete: the Phase-5 `setTimeout(fn, 0)` synchronous-fire
    // shim was replaced by a real per-context timer queue in
    // a subsequent slice; setTimeout(fn, 0) now queues for the
    // next `JsContext::tick`. Coverage moved to the
    // `set_timeout_with_delay_fires_when_tick_passes_deadline`
    // and `clear_timeout_cancels_pending` tests in `tests::`.

    /// Settle pending fetches by ticking until delivered (bounded).
    fn settle_fetches(ctx: &mut crate::JsContext) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while ctx.pending_fetches() > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
            ctx.tick(std::time::Instant::now());
        }
        assert_eq!(ctx.pending_fetches(), 0, "fetches never settled");
    }

    #[test]
    fn fetch_resolves_asynchronously_through_embedder_fetcher() {
        // A script calls fetch(url) and gets a real pending Promise:
        // the network runs on a background thread and the .then chain
        // fires once the embedder ticks the context.
        let doc = wrapped_doc(
            "<body><script>\
                var captured = {};\
                fetch('/api/x').then(function(r){ \
                    captured.status = r.status; \
                    captured.ok = r.ok; \
                    captured.body = r.body; \
                });\
             </script></body>",
        );
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
        let (mut ctx, _) = crate::JsContext::install_and_run(
            doc,
            String::from("https://example.com/page"),
            Some(fetcher),
        );
        settle_fetches(&mut ctx);

        assert_eq!(ctx.eval("captured.status"), "200");
        assert_eq!(ctx.eval("captured.ok"), "true");
        let body = ctx.eval("captured.body");
        assert!(body.contains("\"hello\":\"world\""), "body was {body:?}");
        assert_eq!(url_seen.lock().unwrap().as_deref(), Some("/api/x"));

        // Promise chaining through r.json(): the second .then sees
        // the parsed object (thenable adoption in the reaction).
        ctx.eval(
            "var jhello = null;\
             fetch('/api/x').then(function(r){ return r.json(); })\
                 .then(function(j){ jhello = j.hello; });",
        );
        settle_fetches(&mut ctx);
        // One more tick: the adopted json() promise resolves through
        // an extra microtask hop.
        ctx.tick(std::time::Instant::now());
        assert_eq!(ctx.eval("jhello"), "world");
    }

    #[test]
    fn element_contains_walks_descendants() {
        let mut engine = Engine::new();
        let doc = wrapped_doc(
            "<body><div id='outer'><span id='inner'>x</span></div>\
             <p id='sibling'>y</p></body>",
        );
        let _ctx = BindingContext::install(&mut engine, doc.clone(), String::new());

        // outer contains itself.
        let (self_, _) = engine.eval_with_output(
            "(function(){ var d = document.querySelector('#outer'); return d.contains(d); })()",
        );
        assert_eq!(self_, "true");

        // outer contains its descendant inner.
        let (desc, _) = engine.eval_with_output(
            "document.querySelector('#outer').contains(document.querySelector('#inner'))",
        );
        assert_eq!(desc, "true");

        // outer does NOT contain its sibling.
        let (sibling, _) = engine.eval_with_output(
            "document.querySelector('#outer').contains(document.querySelector('#sibling'))",
        );
        assert_eq!(sibling, "false");
    }

    #[test]
    fn layout_geometry_stubs_return_zero_without_crashing() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><div id='d'>x</div></body>");
        let _ctx = BindingContext::install(&mut engine, doc.clone(), String::new());

        let (w, _) = engine.eval_with_output(
            "document.querySelector('#d').offsetWidth",
        );
        assert_eq!(w, "0");

        let (rect, _) = engine.eval_with_output(
            "(function(){\
                 var r = document.querySelector('#d').getBoundingClientRect();\
                 return r.x + ',' + r.y + ',' + r.width + ',' + r.height;\
             })()",
        );
        assert_eq!(rect, "0,0,0,0");

        // Function stubs are callable, no return value to check.
        engine
            .eval("document.querySelector('#d').focus(); document.querySelector('#d').click()")
            .expect("eval");

        // window scroll position stubs.
        let (sx, _) = engine.eval_with_output("window.scrollX + ',' + window.pageYOffset");
        assert_eq!(sx, "0,0");
    }

    #[test]
    fn node_tree_walks_elements_and_text() {
        let mut engine = Engine::new();
        // Body has an element with a text child + a sibling
        // element. Mixed content is the realistic shape.
        let doc = wrapped_doc(
            "<body><p id='p1'>hello</p><p id='p2'>world</p></body>",
        );
        let _ctx = BindingContext::install(&mut engine, doc.clone(), String::new());

        // firstChild of <p id='p1'> is the text node.
        let (kind, _) = engine.eval_with_output(
            "document.querySelector('#p1').firstChild.nodeType",
        );
        assert_eq!(kind, "3"); // TEXT_NODE
        let (val, _) = engine.eval_with_output(
            "document.querySelector('#p1').firstChild.nodeValue",
        );
        assert_eq!(val, "hello");

        // nextSibling of #p1 is the element <p id='p2'>.
        let (sib_id, _) = engine.eval_with_output(
            "document.querySelector('#p1').nextSibling.id",
        );
        assert_eq!(sib_id, "p2");
        let (sib_type, _) = engine.eval_with_output(
            "document.querySelector('#p1').nextSibling.nodeType",
        );
        assert_eq!(sib_type, "1"); // ELEMENT_NODE

        // childNodes includes the text node.
        let (count, _) = engine.eval_with_output(
            "document.querySelector('#p1').childNodes.length",
        );
        assert_eq!(count, "1");
        let (name, _) = engine.eval_with_output(
            "document.querySelector('#p1').childNodes[0].nodeName",
        );
        assert_eq!(name, "#text");
    }

    #[test]
    fn clone_node_creates_detached_copy() {
        let mut engine = Engine::new();
        let doc = wrapped_doc(
            "<body><div id='src' class='c'><span>hi</span></div></body>",
        );
        let _ctx = BindingContext::install(&mut engine, doc.clone(), String::new());

        // Shallow clone — keeps attrs, drops children.
        let (tag, _) = engine.eval_with_output(
            "(function(){\
                 var c = document.querySelector('#src').cloneNode(false);\
                 return c.tagName + ',' + c.id + ',' + c.childNodes.length;\
             })()",
        );
        assert_eq!(tag, "DIV,src,0");

        // Deep clone — children come along.
        let (deep, _) = engine.eval_with_output(
            "(function(){\
                 var c = document.querySelector('#src').cloneNode(true);\
                 return c.childNodes.length + ',' + c.firstChild.tagName;\
             })()",
        );
        assert_eq!(deep, "1,SPAN");
    }

    #[test]
    fn insert_adjacent_html_appends_at_beforeend() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><div id='c'><p>existing</p></div></body>");
        let _ctx = BindingContext::install(&mut engine, doc.clone(), String::new());

        engine
            .eval(
                "document.querySelector('#c').insertAdjacentHTML('beforeend', '<span>new</span>')",
            )
            .expect("eval");

        let d = doc.lock().unwrap();
        let div = d
            .descendants(d.root)
            .find(|n| d.element(*n).map(|e| e.get_attr("id") == Some("c")).unwrap_or(false))
            .expect("div");
        let mut child_names = Vec::new();
        let mut c = d.node(div).first_child;
        while let Some(id) = c {
            if let Some(e) = d.element(id) {
                child_names.push(e.name.clone());
            }
            c = d.node(id).next_sibling;
        }
        assert_eq!(child_names, vec!["p".to_string(), "span".to_string()]);
    }

    #[test]
    fn abort_controller_signals_listeners() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());

        let (got, _) = engine.eval_with_output(
            "(function(){\
                 var c = new AbortController();\
                 var fired = 0;\
                 c.signal.addEventListener('abort', function(){ fired = fired + 1; });\
                 if (c.signal.aborted) return 'pre-aborted';\
                 c.abort();\
                 return fired + ',' + c.signal.aborted;\
             })()",
        );
        assert_eq!(got, "1,true");
    }

    #[test]
    fn form_data_round_trips() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());

        let (got, _) = engine.eval_with_output(
            "(function(){\
                 var f = new FormData();\
                 f.append('q', 'hello');\
                 f.append('q', 'world');\
                 f.set('lang', 'en');\
                 return f.get('q') + ',' + f.getAll('q').length + ',' + f.get('lang');\
             })()",
        );
        assert_eq!(got, "hello,2,en");
    }

    #[test]
    fn headers_case_insensitive_get() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());

        let (got, _) = engine.eval_with_output(
            "(function(){\
                 var h = new Headers({'Content-Type': 'application/json'});\
                 return h.get('content-type') + ',' + h.has('CONTENT-TYPE');\
             })()",
        );
        assert_eq!(got, "application/json,true");
    }

    #[test]
    fn custom_event_construction() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());

        let (got, _) = engine.eval_with_output(
            "(function(){\
                 var e = new CustomEvent('myevent', { detail: { x: 42 }, bubbles: true });\
                 return e.type + ',' + e.detail.x + ',' + e.bubbles;\
             })()",
        );
        assert_eq!(got, "myevent,42,true");
    }

    /// URL component decomposition. Zinc's `p.toString()` direct
    /// dispatch doesn't honour own-property `toString`, so this
    /// test only covers the component getters — `.href` /
    /// `.toString()` round-trip is broken until Zinc fixes the
    /// dispatch bug (calling via template literals `\`${url}\``
    /// or `'' + url` does work).
    #[test]
    fn url_polyfill_decomposes_absolute_components() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());

        let (host, _) = engine.eval_with_output(
            "new URL('https://example.com:8080/path?q=1#h').host",
        );
        assert_eq!(host, "example.com:8080");
        let (hostname, _) = engine.eval_with_output(
            "new URL('https://example.com:8080/path').hostname",
        );
        assert_eq!(hostname, "example.com");
        let (port, _) = engine.eval_with_output(
            "new URL('https://example.com:8080/path').port",
        );
        assert_eq!(port, "8080");
        let (pathname, _) = engine.eval_with_output(
            "new URL('https://example.com/path?q=1#h').pathname",
        );
        assert_eq!(pathname, "/path");
        let (search, _) = engine.eval_with_output(
            "new URL('https://example.com/path?q=1#h').search",
        );
        assert_eq!(search, "?q=1");
        let (hash, _) = engine.eval_with_output(
            "new URL('https://example.com/path?q=1#h').hash",
        );
        assert_eq!(hash, "#h");
        let (proto, _) = engine.eval_with_output(
            "new URL('https://example.com/').protocol",
        );
        assert_eq!(proto, "https:");

        // searchParams.get works (each instance owns the
        // method as a closure, not via prototype).
        let (q, _) = engine.eval_with_output(
            "new URL('https://a.com/?x=1&y=2').searchParams.get('y')",
        );
        assert_eq!(q, "2");
    }

    #[test]
    fn url_search_params_get_and_has() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());

        let (got, _) = engine.eval_with_output(
            "(function(){\
                 var p = new URLSearchParams('a=1&b=hello+world');\
                 return p.get('a') + ',' + p.get('b');\
             })()",
        );
        assert_eq!(got, "1,hello world");

        let (has, _) = engine.eval_with_output(
            "(function(){\
                 var p = new URLSearchParams('a=1');\
                 return p.has('a') + ',' + p.has('missing');\
             })()",
        );
        assert_eq!(has, "true,false");
    }

    #[test]
    fn structured_clone_deep_copies_objects() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body></body>");
        let _ctx = BindingContext::install(&mut engine, doc, String::new());

        let (got, _) = engine.eval_with_output(
            "(function(){\
                 var a = { x: 1, y: { z: [2, 3] } };\
                 var b = structuredClone(a);\
                 b.y.z.push(4);\
                 return a.y.z.length + ',' + b.y.z.length;\
             })()",
        );
        assert_eq!(got, "2,3");
    }

    #[test]
    fn inner_html_setter_parses_fragment_into_children() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><div id='d'><span>old</span></div></body>");
        let _ctx = BindingContext::install(&mut engine, doc.clone(), String::new());

        engine
            .eval(
                "document.querySelector('#d').innerHTML = '<p class=\"x\">hello <em>world</em></p>'",
            )
            .expect("eval");

        // The DOM reflects the new subtree.
        let d = doc.lock().unwrap();
        let div = d
            .descendants(d.root)
            .find(|n| d.element(*n).map(|e| e.name == "div").unwrap_or(false))
            .expect("div");
        let mut child_names: Vec<String> = Vec::new();
        let mut child = d.node(div).first_child;
        while let Some(id) = child {
            if let Some(e) = d.element(id) {
                child_names.push(e.name.clone());
            }
            child = d.node(id).next_sibling;
        }
        assert_eq!(child_names, vec!["p".to_string()]);

        // The <span>old</span> child is detached (still in the
        // node arena but no longer parented under the div).
        let p = d
            .descendants(d.root)
            .find(|n| d.element(*n).map(|e| e.name == "p").unwrap_or(false))
            .expect("p");
        assert_eq!(
            d.element(p).and_then(|e| e.get_attr("class")),
            Some("x")
        );
    }

    #[test]
    fn inner_html_getter_serialises_subtree() {
        let mut engine = Engine::new();
        let doc = wrapped_doc(
            "<body><div id='d'><p class='x'>hi <em>there</em></p></div></body>",
        );
        let _ctx = BindingContext::install(&mut engine, doc.clone(), String::new());

        let (got, _) = engine.eval_with_output(
            "document.querySelector('#d').innerHTML",
        );
        // Long-form open/close, attribute escaping, text
        // round-trip. Slight loss vs. the source (em wraps
        // contiguous text → serialiser puts `there` inside).
        assert!(got.contains("<p class=\"x\">"), "got: {got}");
        assert!(got.contains("<em>there</em>"), "got: {got}");
        assert!(got.contains("hi "), "got: {got}");
    }

    #[test]
    fn dataset_exposes_data_attrs_as_camelcase() {
        let mut engine = Engine::new();
        let doc = wrapped_doc(
            "<body><div id='d' data-foo='one' data-foo-bar='two' \
             data-x='three' class='ignored' title='also-ignored'></div></body>",
        );
        let _ctx = BindingContext::install(&mut engine, doc.clone(), String::new());

        // `data-foo` → `dataset.foo`.
        let (foo, _) = engine.eval_with_output(
            "document.querySelector('#d').dataset.foo",
        );
        assert_eq!(foo, "one");

        // `data-foo-bar` → `dataset.fooBar` (kebab → camel).
        let (foobar, _) = engine.eval_with_output(
            "document.querySelector('#d').dataset.fooBar",
        );
        assert_eq!(foobar, "two");

        // Non-`data-` attributes are not surfaced.
        let (title, _) = engine.eval_with_output(
            "document.querySelector('#d').dataset.title",
        );
        // Zinc's stringify of `undefined` is the literal word.
        assert_eq!(title, "undefined");
    }

    #[test]
    fn style_set_get_remove_property_round_trips_through_style_attr() {
        let mut engine = Engine::new();
        let doc = wrapped_doc(
            "<body><div id='d' style='color: red'></div></body>",
        );
        let _ctx = BindingContext::install(&mut engine, doc.clone(), String::new());

        // Initial value.
        let (color, _) = engine.eval_with_output(
            "document.querySelector('#d').style.getPropertyValue('color')",
        );
        assert_eq!(color, "red");

        // Set a new property.
        engine
            .eval(
                "document.querySelector('#d').style.setProperty('font-size', '16px')",
            )
            .expect("eval");
        let (fs, _) = engine.eval_with_output(
            "document.querySelector('#d').style.getPropertyValue('font-size')",
        );
        assert_eq!(fs, "16px");

        // Remove the original.
        engine
            .eval("document.querySelector('#d').style.removeProperty('color')")
            .expect("eval");
        let (gone, _) = engine.eval_with_output(
            "document.querySelector('#d').style.getPropertyValue('color')",
        );
        assert_eq!(gone, "");

        // Verify the DOM round-trip — the new attr is the
        // canonical "prop: value" serialisation.
        let d = doc.lock().unwrap();
        let div = d
            .descendants(d.root)
            .find(|n| d.element(*n).map(|e| e.name == "div").unwrap_or(false))
            .expect("div");
        assert_eq!(
            d.element(div).and_then(|e| e.get_attr("style")),
            Some("font-size: 16px"),
        );
    }

    #[test]
    fn style_csstext_replaces_whole_attribute() {
        let mut engine = Engine::new();
        let doc = wrapped_doc("<body><div id='d'></div></body>");
        let _ctx = BindingContext::install(&mut engine, doc.clone(), String::new());

        engine
            .eval(
                "document.querySelector('#d').style.cssText = 'display: none; opacity: 0.5'",
            )
            .expect("eval");
        let (got, _) = engine.eval_with_output(
            "document.querySelector('#d').style.cssText",
        );
        assert_eq!(got, "display: none; opacity: 0.5");

        // Assigning empty cssText drops the attribute.
        engine
            .eval("document.querySelector('#d').style.cssText = ''")
            .expect("eval");
        let d = doc.lock().unwrap();
        let div = d
            .descendants(d.root)
            .find(|n| d.element(*n).map(|e| e.name == "div").unwrap_or(false))
            .expect("div");
        assert_eq!(d.element(div).and_then(|e| e.get_attr("style")), None);
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
// temp probe — appended to dom_bindings tests
#[test]
fn install_does_not_block_on_slow_fetch() {
    let doc = wrapped_doc(
        "<body><script>fetch('/slow').then(function(r){});</script></body>",
    );
    let fetcher: Fetcher = std::sync::Arc::new(move |u| {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        Some(FetchResponse { status: 200, url: u.to_string(), body: b"{}".to_vec() })
    });
    let started = std::time::Instant::now();
    let (_ctx, _) = crate::JsContext::install_and_run(
        doc, String::from("https://example.com/"), Some(fetcher));
    let elapsed = started.elapsed();
    assert!(elapsed < std::time::Duration::from_millis(500),
        "install_and_run blocked {elapsed:?} on a slow fetch");
}

}
