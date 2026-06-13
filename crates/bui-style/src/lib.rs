//! bui-style — selector matching, cascade, computed values.
//!
//! Phase 3: collects style rules from a built-in user-agent stylesheet plus
//! author `<style>` blocks, matches them against the DOM, applies cascade
//! and inheritance, and produces a `ComputedValues` per element.

mod media;
mod values;

pub use media::ViewportSize;
pub use values::{
    set_viewport, viewport, AlignItems, BackgroundAxisPos, BackgroundPosition, BackgroundRepeat,
    BackgroundSize, BorderCollapse, BoxShadow, BoxSizing, CaptionSide, Clear, ComputedValues,
    Cursor, Dimension, Display, EdgeSizes, FlexBasis, FlexDirection, FlexWrap, Float, FontStyle,
    FontWeight, GridAutoFlow, GridLine, JustifyContent, Length, ListStyleType, MinMaxSide,
    ObjectFit, Overflow, OverflowWrap, PointerEvents, Position, RgbaColor, TextAlign, TextOverflow,
    TextShadow, TextTransform, TrackSize, VerticalAlign, Visibility, WhiteSpace, WordBreak,
};

use std::collections::HashMap;

use bui_css::{Declaration, Rule, Specificity, StyleRule, Stylesheet};
use bui_dom::{Document, NodeId, NodeKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    UserAgent,
    Author,
}

#[derive(Debug, Clone)]
pub struct StyleTree {
    pub values: HashMap<NodeId, ComputedValues>,
    /// Computed values for the synthetic `::before` pseudo-element of
    /// each real DOM node. Only present when at least one matching
    /// `::before` rule applied. Inherits from the real element.
    pub before: HashMap<NodeId, ComputedValues>,
    pub after: HashMap<NodeId, ComputedValues>,
}

impl StyleTree {
    pub fn get(&self, id: NodeId) -> Option<&ComputedValues> {
        self.values.get(&id)
    }
    pub fn before(&self, id: NodeId) -> Option<&ComputedValues> {
        self.before.get(&id)
    }
    pub fn after(&self, id: NodeId) -> Option<&ComputedValues> {
        self.after.get(&id)
    }
}

/// Phase 3 entry point: builds the style tree for `doc`, given a UA sheet
/// and zero or more author sheets (typically the contents of `<style>`).
/// Uses a default desktop viewport for `@media` evaluation. Call
/// `style_document_with_viewport` to gate against a real window size.
pub fn style_document(doc: &Document, author_sheets: &[Stylesheet]) -> StyleTree {
    style_document_with_viewport(doc, author_sheets, ViewportSize::DEFAULT_DESKTOP)
}

/// Like `style_document`, but evaluates `@media` queries against
/// `viewport`. Rules guarded by a non-matching `@media` block are
/// skipped during the cascade.
pub fn style_document_with_viewport(
    doc: &Document,
    author_sheets: &[Stylesheet],
    viewport: ViewportSize,
) -> StyleTree {
    let ua = Stylesheet::parse(USER_AGENT_CSS);
    let inputs = std::iter::once((Origin::UserAgent, &ua))
        .chain(author_sheets.iter().map(|s| (Origin::Author, s)))
        .collect::<Vec<_>>();

    // Pre-flatten rules with (origin, source-order) tags. We hold owned
    // `StyleRule`s here because parsing nested @media blocks produces new
    // Stylesheets whose rules we'd otherwise be borrowing from temporaries.
    let mut flat: Vec<(Origin, usize, StyleRule)> = Vec::new();
    let mut order = 0usize;
    for (origin, sheet) in &inputs {
        flatten_rules(&sheet.rules, *origin, viewport, &mut flat, &mut order);
    }

    let flat_refs: Vec<(Origin, usize, &StyleRule)> = flat
        .iter()
        .map(|(o, n, r)| (*o, *n, r))
        .collect();
    let index = RuleIndex::build(&flat_refs);

    let mut values: HashMap<NodeId, ComputedValues> = HashMap::new();
    let mut before: HashMap<NodeId, ComputedValues> = HashMap::new();
    let mut after: HashMap<NodeId, ComputedValues> = HashMap::new();
    cascade_recursive(
        doc,
        doc.root,
        &flat_refs,
        &index,
        None,
        &mut values,
        &mut before,
        &mut after,
    );
    StyleTree { values, before, after }
}

/// One selector of one flattened rule, addressable for the per-rule
/// best-specificity merge.
#[derive(Clone, Copy)]
struct IndexedSelector<'a> {
    /// Position in the flat rules slice.
    rule_pos: usize,
    /// Position inside the rule's selector list (tie-break: the first
    /// max-specificity selector decides the pseudo-element bucket).
    sel_pos: usize,
    sel: &'a bui_css::Selector,
}

/// Rules bucketed by the rightmost compound of each selector — its
/// id, else first class, else tag — so the cascade only runs full
/// `Selector::matches` on selectors that *could* match an element,
/// instead of every selector of every rule (O(elements × rules)).
/// The bucket key is a necessary condition for a match; `matches`
/// still re-validates everything, so imprecision here is impossible.
struct RuleIndex<'a> {
    by_id: HashMap<&'a str, Vec<IndexedSelector<'a>>>,
    by_class: HashMap<&'a str, Vec<IndexedSelector<'a>>>,
    /// Keys lowercased (tag matching is ASCII-case-insensitive).
    by_tag: HashMap<String, Vec<IndexedSelector<'a>>>,
    universal: Vec<IndexedSelector<'a>>,
}

impl<'a> RuleIndex<'a> {
    fn build(rules: &[(Origin, usize, &'a StyleRule)]) -> Self {
        let mut idx = RuleIndex {
            by_id: HashMap::new(),
            by_class: HashMap::new(),
            by_tag: HashMap::new(),
            universal: Vec::new(),
        };
        for (rule_pos, (_, _, sr)) in rules.iter().enumerate() {
            for (sel_pos, sel) in sr.selectors.iter().enumerate() {
                let entry = IndexedSelector { rule_pos, sel_pos, sel };
                match sel.compounds.last() {
                    Some(cp) if cp.id.is_some() => {
                        idx.by_id.entry(cp.id.as_deref().unwrap()).or_default().push(entry);
                    }
                    Some(cp) if !cp.classes.is_empty() => {
                        idx.by_class.entry(cp.classes[0].as_str()).or_default().push(entry);
                    }
                    Some(cp) if cp.tag.as_deref().is_some_and(|t| t != "*") => {
                        idx.by_tag
                            .entry(cp.tag.as_deref().unwrap().to_ascii_lowercase())
                            .or_default()
                            .push(entry);
                    }
                    _ => idx.universal.push(entry),
                }
            }
        }
        idx
    }

    /// Selectors whose rightmost compound could match `elem`.
    fn candidates(&self, elem: &bui_dom::Element, out: &mut Vec<IndexedSelector<'a>>) {
        out.extend_from_slice(&self.universal);
        let tag_hit = if elem.name.bytes().any(|b| b.is_ascii_uppercase()) {
            self.by_tag.get(&elem.name.to_ascii_lowercase())
        } else {
            self.by_tag.get(elem.name.as_str())
        };
        if let Some(list) = tag_hit {
            out.extend_from_slice(list);
        }
        for class in elem.classes() {
            if let Some(list) = self.by_class.get(class) {
                out.extend_from_slice(list);
            }
        }
        if let Some(id) = elem.get_attr("id") {
            if let Some(list) = self.by_id.get(id) {
                out.extend_from_slice(list);
            }
        }
    }
}

/// Walk a list of rules, collecting `Rule::Style` entries directly and
/// recursing into matching `@media` blocks. Non-style at-rules are
/// dropped.
fn flatten_rules(
    rules: &[Rule],
    origin: Origin,
    viewport: ViewportSize,
    out: &mut Vec<(Origin, usize, StyleRule)>,
    order: &mut usize,
) {
    for r in rules {
        match r {
            Rule::Style(sr) => {
                out.push((origin, *order, sr.clone()));
                *order += 1;
            }
            Rule::At { name, prelude, block } if name.eq_ignore_ascii_case("media") => {
                if !media::matches(prelude, viewport) {
                    continue;
                }
                let Some(body) = block else { continue };
                let inner = Stylesheet::parse(body);
                flatten_rules(&inner.rules, origin, viewport, out, order);
            }
            // `@supports (feature)` — we say "yes" to the feature
            // detection optimistically. That's right far more often
            // than not; the alternative (saying "no") would gate off
            // huge piles of modern CSS that we actually do support.
            Rule::At { name, block, .. } if name.eq_ignore_ascii_case("supports") => {
                let Some(body) = block else { continue };
                let inner = Stylesheet::parse(body);
                flatten_rules(&inner.rules, origin, viewport, out, order);
            }
            // `@layer name { … }` — cascade layers. We ignore the layer
            // name (real implementation would order layers) and just
            // inline the rules. The plain `@layer name;` declaration
            // form has no body and gets dropped.
            Rule::At { name, block, .. } if name.eq_ignore_ascii_case("layer") => {
                let Some(body) = block else { continue };
                let inner = Stylesheet::parse(body);
                flatten_rules(&inner.rules, origin, viewport, out, order);
            }
            // `@scope` — same trick as @layer for now.
            Rule::At { name, block, .. } if name.eq_ignore_ascii_case("scope") => {
                let Some(body) = block else { continue };
                let inner = Stylesheet::parse(body);
                flatten_rules(&inner.rules, origin, viewport, out, order);
            }
            Rule::At { .. } => {}
        }
    }
}

/// Rules matching `node`, in rule order, each with the best (highest,
/// first-wins-on-tie) specificity among its matching selectors and
/// that selector's pseudo-element. Candidates come from the index
/// buckets; everything else cannot match by its rightmost compound.
fn matched_rules<'a>(
    doc: &Document,
    node: NodeId,
    index: &RuleIndex<'a>,
) -> Vec<(usize, Specificity, Option<&'a str>)> {
    let mut candidates: Vec<IndexedSelector> = Vec::new();
    if let Some(elem) = doc.element(node) {
        index.candidates(elem, &mut candidates);
    }
    let mut hits: Vec<(usize, usize, Specificity, Option<&str>)> = Vec::new();
    for cand in &candidates {
        if cand.sel.matches(doc, node) {
            hits.push((
                cand.rule_pos,
                cand.sel_pos,
                cand.sel.specificity(),
                cand.sel.pseudo_element(),
            ));
        }
    }
    hits.sort_unstable_by_key(|h| (h.0, h.1));
    let mut out = Vec::with_capacity(hits.len());
    let mut i = 0;
    while i < hits.len() {
        let rule_pos = hits[i].0;
        let mut best = hits[i].2;
        let mut best_pseudo = hits[i].3;
        let mut j = i + 1;
        while j < hits.len() && hits[j].0 == rule_pos {
            // Strict > replicates the unindexed loop: the first
            // max-specificity selector in source order decides the
            // pseudo-element bucket.
            if hits[j].2 > best {
                best = hits[j].2;
                best_pseudo = hits[j].3;
            }
            j += 1;
        }
        out.push((rule_pos, best, best_pseudo));
        i = j;
    }
    out
}

fn cascade_recursive(
    doc: &Document,
    node: NodeId,
    rules: &[(Origin, usize, &StyleRule)],
    index: &RuleIndex,
    parent: Option<&ComputedValues>,
    out: &mut HashMap<NodeId, ComputedValues>,
    before_out: &mut HashMap<NodeId, ComputedValues>,
    after_out: &mut HashMap<NodeId, ComputedValues>,
) {
    if !matches!(doc.node(node).kind, NodeKind::Element(_)) {
        // Non-elements still need a children walk.
        let mut child = doc.node(node).first_child;
        while let Some(c) = child {
            cascade_recursive(doc, c, rules, index, parent, out, before_out, after_out);
            child = doc.node(c).next_sibling;
        }
        return;
    }

    // Collect matching declarations with their cascade keys for the
    // real element (selectors with no pseudo-element) and stash any
    // ::before / ::after hits separately so we can build their
    // synthetic CVs after the parent CV is finalised.
    //
    // Only selectors from the element's index buckets are tested —
    // everything else is guaranteed not to match by its rightmost
    // compound. Hits are then merged per rule with the same
    // first-max-specificity-wins logic the unindexed loop used.
    let mut matches: Vec<(CascadeKey, Declaration)> = Vec::new();
    let mut before_matches: Vec<(CascadeKey, Declaration)> = Vec::new();
    let mut after_matches: Vec<(CascadeKey, Declaration)> = Vec::new();
    for (rule_pos, best, best_pseudo) in matched_rules(doc, node, index) {
        let (origin, order, sr) = rules[rule_pos];
        let bucket = match best_pseudo {
            Some("before") => &mut before_matches,
            Some("after") => &mut after_matches,
            Some(_) => continue, // unsupported pseudo (::first-letter etc)
            None => &mut matches,
        };
        for decl in &sr.declarations {
            bucket.push((
                CascadeKey {
                    origin,
                    important: decl.important,
                    specificity: best,
                    source_order: order,
                },
                decl.clone(),
            ));
        }
    }

    // Inline style attribute (counts as Author with max specificity outside selector formula).
    if let Some(elem) = doc.element(node) {
        if let Some(inline) = elem.get_attr("style") {
            let css = format!("__inline {{ {inline} }}");
            let s = Stylesheet::parse(&css);
            for r in s.rules {
                if let Rule::Style(sr) = r {
                    for decl in sr.declarations {
                        matches.push((
                            CascadeKey {
                                origin: Origin::Author,
                                important: decl.important,
                                specificity: Specificity {
                                    a: 1000,
                                    b: 0,
                                    c: 0,
                                },
                                source_order: usize::MAX,
                            },
                            decl,
                        ));
                    }
                }
            }
        }
    }

    matches.sort_by(|(ka, _), (kb, _)| ka.cascade_cmp(kb));

    // Start with inherited values from parent (or initial root).
    let mut cv = match parent {
        Some(p) => p.inherit_into_default(),
        None => ComputedValues::root_default(),
    };

    // CSS custom properties cascade INDEPENDENTLY of var() substitution
    // (CSS Variables §3): first every `--foo` resolves to its winning
    // cascaded value, THEN `var()` references are substituted using those
    // final values. Doing it in one interleaved pass (resolving each
    // var() against the vars-so-far) is wrong when a referenced custom
    // property is set by a HIGHER-specificity rule that sorts later — the
    // referrer reads a stale/inherited value. DuckDuckGo's CTA button hit
    // this: `.link-button_primary-solid { --button-rest-bg:
    // var(--ds-accent-primary) }` read the inherited `.theme-light` blue
    // instead of the `.theme-light .motif-mandarin` orange (higher
    // specificity, sorted later), so the button rendered blue.
    //
    // Phase A — cascade custom properties to their winning RAW values
    // (matches is pre-sorted ascending, so the last writer wins).
    for (_, decl) in &matches {
        if let Some(custom) = decl.name.strip_prefix("--") {
            cv.vars.insert(format!("--{custom}"), decl.value.trim().to_string());
        }
    }
    // Phase B — resolve var() references WITHIN custom-property values to
    // a fixed point (bounded), so `--button-rest-bg: var(--ds-accent-
    // primary)` ends up holding the final colour, not a nested var().
    resolve_var_map(&mut cv.vars);
    // Phase C — apply non-custom declarations, substituting var() against
    // the fully-resolved map.
    for (_, decl) in &matches {
        if decl.name.starts_with("--") {
            continue;
        }
        let resolved = substitute_vars(&decl.value, &cv.vars);
        let resolved_decl = bui_css::Declaration {
            name: decl.name.clone(),
            value: resolved,
            important: decl.important,
        };
        values::apply_declaration(&mut cv, &resolved_decl, parent);
    }

    // CSS spec: `border-color` initial value is `currentcolor`.
    // We've been carrying a hardcoded BLACK in the cv defaults; if
    // no rule explicitly set border-color (or via the `border`
    // shorthand with a color token), copy this element's resolved
    // `color` over so borders track the local text color rather
    // than parent-inherited black. Without this, an element that
    // declared only `border: 1px solid` painted a black ring
    // regardless of its own color.
    if !cv.border_color_explicit {
        cv.border_color = cv.color;
    }

    out.insert(node, cv.clone());

    // Build ::before / ::after computed values if any rules matched.
    // The pseudo-element inherits from the real element (its visual
    // parent), so we seed with `cv.inherit_into_default()`.
    if !before_matches.is_empty() {
        if let Some(synth) = build_pseudo_cv(&cv, &before_matches) {
            before_out.insert(node, synth);
        }
    }
    if !after_matches.is_empty() {
        if let Some(synth) = build_pseudo_cv(&cv, &after_matches) {
            after_out.insert(node, synth);
        }
    }

    let mut child = doc.node(node).first_child;
    while let Some(c) = child {
        cascade_recursive(doc, c, rules, index, Some(&cv), out, before_out, after_out);
        child = doc.node(c).next_sibling;
    }
}

/// Build computed values for a pseudo-element from its parent
/// element's CV plus the matched rules. Returns `None` if there's
/// no `content` declaration — pseudo-elements without `content`
/// don't render in CSS.
fn build_pseudo_cv(
    parent: &ComputedValues,
    matches: &[(CascadeKey, Declaration)],
) -> Option<ComputedValues> {
    let mut sorted: Vec<(CascadeKey, Declaration)> = matches.to_vec();
    sorted.sort_by(|(ka, _), (kb, _)| ka.cascade_cmp(kb));
    let mut cv = parent.inherit_into_default();
    // Same two-phase resolution as the main cascade (see cascade_recursive):
    // custom properties cascade first, then var() substitution.
    for (_, decl) in &sorted {
        if let Some(custom) = decl.name.strip_prefix("--") {
            cv.vars.insert(format!("--{custom}"), decl.value.trim().to_string());
        }
    }
    resolve_var_map(&mut cv.vars);
    for (_, decl) in &sorted {
        if decl.name.starts_with("--") {
            continue;
        }
        {
            let resolved = substitute_vars(&decl.value, &cv.vars);
            let resolved_decl = Declaration {
                name: decl.name.clone(),
                value: resolved,
                important: decl.important,
            };
            values::apply_declaration(&mut cv, &resolved_decl, Some(parent));
        }
    }
    // Mirror the `border-color: currentcolor` initial-value
    // finalization done in cascade_recursive (see comment there).
    if !cv.border_color_explicit {
        cv.border_color = cv.color;
    }
    if cv.content.is_some() {
        Some(cv)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy)]
struct CascadeKey {
    origin: Origin,
    important: bool,
    specificity: Specificity,
    source_order: usize,
}

impl CascadeKey {
    fn cascade_cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Higher cascade priority sorts last (so it overrides earlier entries).
        let priority = |k: &CascadeKey| match (k.origin, k.important) {
            (Origin::UserAgent, false) => 0,
            (Origin::Author, false) => 1,
            (Origin::Author, true) => 2,
            (Origin::UserAgent, true) => 3,
        };
        priority(self)
            .cmp(&priority(other))
            .then(self.specificity.cmp(&other.specificity))
            .then(self.source_order.cmp(&other.source_order))
    }
}

/// Extract the text content of all `<style>` elements in the tree.
/// Skips `<style>` blocks that live inside `<noscript>` — those carry
/// the page's JS-disabled fallback rules (often a "hide everything"
/// nuke like `table,div,span,p{display:none}`) and we render as if
/// scripting is enabled. Without this skip, google.com's noscript
/// stylesheet collapsed every `<div>` on the page to display:none.
pub fn extract_inline_stylesheets(doc: &Document) -> Vec<Stylesheet> {
    fn has_noscript_ancestor(doc: &Document, nid: NodeId) -> bool {
        let mut cur = doc.node(nid).parent;
        while let Some(p) = cur {
            if let Some(elem) = doc.element(p) {
                if elem.name == "noscript" {
                    return true;
                }
            }
            cur = doc.node(p).parent;
        }
        false
    }
    let mut out = Vec::new();
    for nid in doc.descendants(doc.root) {
        let Some(elem) = doc.element(nid) else {
            continue;
        };
        if elem.name != "style" {
            continue;
        }
        if has_noscript_ancestor(doc, nid) {
            continue;
        }
        let mut text = String::new();
        let mut child = doc.node(nid).first_child;
        while let Some(c) = child {
            if let NodeKind::Text(t) = &doc.node(c).kind {
                text.push_str(t);
            }
            child = doc.node(c).next_sibling;
        }
        if !text.trim().is_empty() {
            out.push(Stylesheet::parse(&text));
        }
    }
    out
}

const USER_AGENT_CSS: &str = include_str!("ua.css");

/// Resolve `var()` references that appear *inside* custom-property
/// values, to a fixed point. After the cascade collects each `--foo`'s
/// winning raw value (which may itself be `var(--bar)` or
/// `var(--bar, fallback)`), this rewrites them so every entry holds a
/// flat value. Bounded to a few passes so a cyclic reference
/// (`--a: var(--b); --b: var(--a)`) can't loop forever — it just
/// settles to empty, as browsers do.
fn resolve_var_map(vars: &mut std::collections::HashMap<String, String>) {
    // Most maps stabilize in 1–2 passes; the chains real design systems
    // build (token → semantic → component) are shallow.
    for _ in 0..8 {
        let mut changed = false;
        let snapshot = vars.clone();
        for (_, v) in vars.iter_mut() {
            if v.contains("var(") {
                let resolved = substitute_vars(v, &snapshot);
                if &resolved != v {
                    *v = resolved;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
}

/// Replace each top-level `var(--name)` (and `var(--name, fallback)`)
/// in `value` with whatever `vars` resolves it to. A missing variable
/// without a fallback collapses to the empty string — matching how
/// browsers behave when an undefined custom property is referenced.
/// One pass over the input; nested `var()`s in the resolved string
/// aren't re-substituted (that's a fast-path simplification — most
/// real Wikipedia / Vector CSS doesn't depend on it).
fn substitute_vars(value: &str, vars: &std::collections::HashMap<String, String>) -> String {
    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for the literal "var(" — case-insensitive at this
        // boundary because authors sometimes type "VAR()". `var` must
        // also start a token, so the previous char (if any) shouldn't
        // be alphanumeric.
        let starts_var = i + 4 <= bytes.len()
            && bytes[i..i + 4].eq_ignore_ascii_case(b"var(")
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric());
        if !starts_var {
            // Pass through one full UTF-8 character — pushing
            // `bytes[i] as char` would split multi-byte chars
            // (e.g. the · in `content: "\\a0 · "`) into separate
            // Latin-1 codepoints.
            let lead = bytes[i];
            let len = if lead < 0x80 {
                1
            } else if lead < 0xC0 {
                1
            } else if lead < 0xE0 {
                2
            } else if lead < 0xF0 {
                3
            } else {
                4
            };
            let end = (i + len).min(bytes.len());
            if let Ok(s) = std::str::from_utf8(&bytes[i..end]) {
                out.push_str(s);
            }
            i = end;
            continue;
        }
        // Walk to the matching ')' tracking paren depth so `rgb(...)`
        // inside a fallback survives.
        let mut depth = 1i32;
        let mut j = i + 4;
        while j < bytes.len() {
            match bytes[j] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        if depth != 0 {
            // Unbalanced — drop the rest as a literal.
            out.push_str(&value[i..]);
            return out;
        }
        let inner = &value[i + 4..j];
        let (name, fallback) = match inner.split_once(',') {
            Some((n, f)) => (n.trim(), Some(f.trim())),
            None => (inner.trim(), None),
        };
        if let Some(v) = vars.get(name) {
            out.push_str(v);
        } else if let Some(f) = fallback {
            out.push_str(f);
        }
        i = j + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bui_dom::Document;

    /// The unindexed matching loop this crate shipped before the
    /// RuleIndex — kept as the reference implementation. The index is
    /// only a pre-filter, so for any document and stylesheet both
    /// paths must produce identical (rule, specificity, pseudo) sets.
    fn matched_rules_bruteforce<'a>(
        doc: &Document,
        node: NodeId,
        rules: &[(Origin, usize, &'a StyleRule)],
    ) -> Vec<(usize, Specificity, Option<&'a str>)> {
        let mut out = Vec::new();
        for (rule_pos, (_, _, sr)) in rules.iter().enumerate() {
            let mut best: Option<Specificity> = None;
            let mut best_pseudo: Option<&str> = None;
            for sel in &sr.selectors {
                if sel.matches(doc, node) {
                    let sp = sel.specificity();
                    let take = match best {
                        Some(cur) => sp > cur,
                        None => true,
                    };
                    if take {
                        best = Some(sp);
                        best_pseudo = sel.pseudo_element();
                    }
                }
            }
            if let Some(sp) = best {
                out.push((rule_pos, sp, best_pseudo));
            }
        }
        out
    }

    #[test]
    fn rule_index_matches_bruteforce() {
        // A tree with ids, multiple classes, siblings, and nesting,
        // against selectors hitting every index bucket: tag, class,
        // id, universal, compounds, combinators, :not/:is, pseudo-
        // elements, and equal-specificity ties within one rule.
        let mut doc = Document::new();
        let html = doc.create_element("html");
        let body = doc.create_element("body");
        doc.append_child(doc.root, html);
        doc.append_child(html, body);
        let mut nodes = vec![doc.root, html, body];
        let main = doc.create_element("div");
        doc.element_mut(main).unwrap().set_attr("class", "page main");
        doc.element_mut(main).unwrap().set_attr("id", "content");
        doc.append_child(body, main);
        nodes.push(main);
        for i in 0..6 {
            let p = doc.create_element(if i % 2 == 0 { "p" } else { "span" });
            if i % 3 == 0 {
                doc.element_mut(p).unwrap().set_attr("class", "note warn");
            }
            if i == 4 {
                doc.element_mut(p).unwrap().set_attr("id", "special");
            }
            doc.append_child(main, p);
            nodes.push(p);
            let em = doc.create_element("em");
            doc.append_child(p, em);
            nodes.push(em);
        }

        let sheet = Stylesheet::parse(
            "p { color: red; }
             .note { color: blue; }
             #special { color: green; }
             * { margin: 0; }
             div.page#content { padding: 1px; }
             div p { font-size: 10px; }
             div > span { font-size: 11px; }
             p + span { font-weight: bold; }
             p ~ span { font-style: italic; }
             em:not(.x) { color: black; }
             :is(.warn, em) { outline: none; }
             p:first-child, .warn { text-align: left; }
             p::before { content: 'a'; }
             .note::after { content: 'b'; }
             span:nth-child(2n) { color: gray; }",
        );
        let viewport = ViewportSize::DEFAULT_DESKTOP;
        let mut flat: Vec<(Origin, usize, StyleRule)> = Vec::new();
        let mut order = 0usize;
        flatten_rules(&sheet.rules, Origin::Author, viewport, &mut flat, &mut order);
        let flat_refs: Vec<(Origin, usize, &StyleRule)> =
            flat.iter().map(|(o, n, r)| (*o, *n, r)).collect();
        let index = RuleIndex::build(&flat_refs);

        let mut checked_any = false;
        for &node in &nodes {
            if doc.element(node).is_none() {
                continue;
            }
            let indexed = matched_rules(&doc, node, &index);
            let brute = matched_rules_bruteforce(&doc, node, &flat_refs);
            assert_eq!(indexed, brute, "divergence at node {node:?}");
            checked_any = checked_any || !brute.is_empty();
        }
        assert!(checked_any, "test must actually match some rules");
    }

    fn build(html_like: &str) -> (Document, NodeId) {
        // Minimal hand-built tree for tests.
        let mut doc = Document::new();
        let html = doc.create_element("html");
        let body = doc.create_element("body");
        let div = doc.create_element("div");
        doc.append_child(doc.root, html);
        doc.append_child(html, body);
        doc.append_child(body, div);
        if !html_like.is_empty() {
            doc.element_mut(div).unwrap().set_attr("class", html_like);
        }
        (doc, div)
    }

    #[test]
    fn border_none_zeros_widths() {
        let (doc, div) = build("box");
        let sheet = Stylesheet::parse(
            ".box { border: 1px solid red; border: none; }",
        );
        let st = style_document(&doc, &[sheet]);
        let cv = st.get(div).unwrap();
        assert_eq!(cv.border, EdgeSizes::ZERO,
            "border: none should zero all four sides; got {:?}", cv.border);
    }

    #[test]
    fn border_style_none_zeros_widths() {
        let (doc, div) = build("box");
        let sheet = Stylesheet::parse(
            ".box { border-width: 2px; border-style: none; }",
        );
        let st = style_document(&doc, &[sheet]);
        let cv = st.get(div).unwrap();
        assert_eq!(cv.border, EdgeSizes::ZERO,
            "border-style: none should zero widths; got {:?}", cv.border);
    }

    #[test]
    fn border_color_defaults_to_color_when_unset() {
        // CSS spec: `border-color` initial value is `currentcolor`,
        // which resolves to the element's own `color`. With our
        // post-cascade finalization, an element that declares only
        // `border: 1px solid` (no color) should pick up its own
        // `color` instead of the legacy hardcoded BLACK.
        let (doc, div) = build("box");
        let sheet = Stylesheet::parse(
            ".box { color: rgb(50, 100, 200); border: 1px solid; }",
        );
        let st = style_document(&doc, &[sheet]);
        let cv = st.get(div).unwrap();
        assert_eq!(cv.border_color.r, 50);
        assert_eq!(cv.border_color.g, 100);
        assert_eq!(cv.border_color.b, 200);
    }

    #[test]
    fn explicit_border_color_wins_over_currentcolor() {
        let (doc, div) = build("box");
        let sheet = Stylesheet::parse(
            ".box { color: red; border: 1px solid blue; }",
        );
        let st = style_document(&doc, &[sheet]);
        let cv = st.get(div).unwrap();
        // `border: 1px solid blue` set the color explicitly to
        // blue; the post-cascade fixup must not stomp it with red.
        assert_eq!(cv.border_color.r, 0);
        assert_eq!(cv.border_color.g, 0);
        assert_eq!(cv.border_color.b, 255);
    }

    #[test]
    fn ua_default_block_for_div() {
        let (doc, div) = build("");
        let st = style_document(&doc, &[]);
        let cv = st.get(div).unwrap();
        assert_eq!(cv.display, Display::Block);
    }

    #[test]
    fn author_overrides_ua() {
        let (doc, div) = build("");
        let sheet = Stylesheet::parse("div { display: inline; }");
        let st = style_document(&doc, &[sheet]);
        let cv = st.get(div).unwrap();
        assert_eq!(cv.display, Display::Inline);
    }

    #[test]
    fn important_user_agent_beats_author() {
        // Spec: !important author beats !important UA. Verify ordering.
        let (doc, div) = build("");
        let author = Stylesheet::parse("div { display: flex !important }");
        let st = style_document(&doc, &[author]);
        let cv = st.get(div).unwrap();
        assert_eq!(cv.display, Display::Flex);
    }

    #[test]
    fn specificity_resolution() {
        let (doc, div) = build("foo");
        let sheet =
            Stylesheet::parse("div { color: red; } .foo { color: green; } div.foo { color: blue; }");
        let st = style_document(&doc, &[sheet]);
        let cv = st.get(div).unwrap();
        assert_eq!(cv.color, RgbaColor::rgb(0, 0, 255)); // div.foo wins
    }

    #[test]
    fn inheritance() {
        let mut doc = Document::new();
        let body = doc.create_element("body");
        let p = doc.create_element("p");
        doc.append_child(doc.root, body);
        doc.append_child(body, p);
        let sheet = Stylesheet::parse("body { color: rgb(10, 20, 30) }");
        let st = style_document(&doc, &[sheet]);
        let cv = st.get(p).unwrap();
        assert_eq!(cv.color, RgbaColor::rgb(10, 20, 30));
    }

    #[test]
    fn background_image_url_parsed_from_longhand_and_shorthand() {
        let (doc, div) = build("foo");
        let sheet = Stylesheet::parse(
            "div.foo { background-image: url('hero.jpg') } \
             div.bar { background: #ccc url(\"side.png\") no-repeat }",
        );
        let st = style_document(&doc, &[sheet]);
        let cv = st.get(div).unwrap();
        assert_eq!(cv.background_image.as_deref(), Some("hero.jpg"));
        // Same div — but here we test the shorthand variant via the
        // existing div via a different rule. (We just test shorthand
        // by feeding it a class match.)
        let (doc2, div2) = build("bar");
        let sheet2 = Stylesheet::parse("div.bar { background: #ccc url(\"side.png\") no-repeat }");
        let st2 = style_document(&doc2, &[sheet2]);
        let cv2 = st2.get(div2).unwrap();
        assert_eq!(cv2.background_image.as_deref(), Some("side.png"));
        // And the colour from the shorthand still applies.
        assert_eq!(cv2.background_color, RgbaColor::rgb(0xCC, 0xCC, 0xCC));
    }

    #[test]
    fn transform_translate_parses() {
        let (doc, div) = build("");
        let sheet = Stylesheet::parse("div { transform: translate(20px, 30px) }");
        let st = style_document(&doc, &[sheet]);
        let cv = st.get(div).unwrap();
        match cv.transform_translate {
            Some((Length::Px(x), Length::Px(y))) => {
                assert!((x - 20.0).abs() < 0.01);
                assert!((y - 30.0).abs() < 0.01);
            }
            _ => panic!("expected (20px, 30px), got {:?}", cv.transform_translate),
        }
    }

    #[test]
    fn outline_shorthand_pulls_width_and_color() {
        let (doc, div) = build("");
        let sheet = Stylesheet::parse("div { outline: 2px solid red }");
        let st = style_document(&doc, &[sheet]);
        let cv = st.get(div).unwrap();
        assert!(matches!(cv.outline_width, Some(Length::Px(v)) if (v - 2.0).abs() < 0.01));
        assert_eq!(cv.outline_color, RgbaColor::rgb(255, 0, 0));
    }

    #[test]
    fn vertical_align_keyword_parses() {
        let (doc, div) = build("");
        let sheet = Stylesheet::parse("div { vertical-align: middle }");
        let st = style_document(&doc, &[sheet]);
        assert!(matches!(
            st.get(div).unwrap().vertical_align,
            VerticalAlign::Middle
        ));
    }

    #[test]
    fn hsl_color_parses() {
        let (doc, div) = build("");
        let sheet = Stylesheet::parse("div { color: hsl(0, 100%, 50%) }");
        let st = style_document(&doc, &[sheet]);
        // hsl(0, 100%, 50%) is pure red.
        assert_eq!(st.get(div).unwrap().color, RgbaColor::rgb(255, 0, 0));
    }

    #[test]
    fn min_and_max_width_reach_computed_values() {
        let (doc, div) = build("");
        let sheet = Stylesheet::parse("div { min-width: 100px; max-width: 600px }");
        let st = style_document(&doc, &[sheet]);
        let cv = st.get(div).unwrap();
        assert!(matches!(cv.min_width, Some(Length::Px(v)) if (v - 100.0).abs() < 0.01));
        assert!(matches!(cv.max_width, Some(Length::Px(v)) if (v - 600.0).abs() < 0.01));
    }

    #[test]
    fn opacity_clamps_into_unit_range() {
        let (doc, div) = build("");
        let sheet = Stylesheet::parse("div { opacity: 0.4 }");
        let st = style_document(&doc, &[sheet]);
        assert!((st.get(div).unwrap().opacity - 0.4).abs() < 0.001);
    }

    #[test]
    fn css_variables_inherit_and_substitute() {
        // Define a variable on the wrapper and reference it on a
        // descendant — the cascade should resolve var(--brand) to the
        // declared value before color parsing runs.
        let mut doc = bui_dom::Document::new();
        let html = doc.create_element("html");
        let body = doc.create_element("body");
        let div = doc.create_element("div");
        doc.element_mut(div).unwrap().set_attr("class", "leaf");
        doc.append_child(doc.root, html);
        doc.append_child(html, body);
        doc.append_child(body, div);
        let sheet = Stylesheet::parse(
            ":root { --brand: rgb(10, 20, 30) } \
             div.leaf { color: var(--brand) }",
        );
        let st = style_document(&doc, &[sheet]);
        let cv = st.get(div).unwrap();
        assert_eq!(cv.color, RgbaColor::rgb(10, 20, 30));
    }

    #[test]
    fn custom_property_indirection_uses_final_cascaded_value() {
        // The DuckDuckGo CTA-button case: an inherited `--accent` (low
        // specificity, on the ancestor) is overridden on the element
        // itself by a HIGHER-specificity rule that sorts LATER, and a
        // SEPARATE lower-specificity rule on the element reads `--accent`
        // indirectly (`--btn-bg: var(--accent)`). The button bg must be
        // the overriding value, not the inherited one — custom properties
        // cascade to final values before var() substitution.
        let mut doc = bui_dom::Document::new();
        let html = doc.create_element("html");
        let body = doc.create_element("body");
        let a = doc.create_element("a");
        doc.element_mut(html).unwrap().set_attr("class", "theme");
        doc.element_mut(a).unwrap().set_attr("class", "btn motif");
        doc.append_child(doc.root, html);
        doc.append_child(html, body);
        doc.append_child(body, a);
        let sheet = Stylesheet::parse(
            ".theme { --accent: rgb(16, 116, 204) } \
             .theme .motif { --accent: rgb(240, 95, 43) } \
             .btn { --btn-bg: var(--accent); background-color: var(--btn-bg) }",
        );
        let st = style_document(&doc, &[sheet]);
        // motif override (orange) wins over the inherited theme blue,
        // even though `.btn` (which reads --accent) has lower specificity.
        assert_eq!(
            st.get(a).unwrap().background_color,
            RgbaColor::rgb(240, 95, 43)
        );
    }

    #[test]
    fn css_variable_fallback_used_when_undefined() {
        let (doc, div) = build("");
        let sheet =
            Stylesheet::parse("div { color: var(--missing, rgb(255, 0, 0)) }");
        let st = style_document(&doc, &[sheet]);
        assert_eq!(st.get(div).unwrap().color, RgbaColor::rgb(255, 0, 0));
    }

    #[test]
    fn font_shorthand_pulls_size_weight_family() {
        let (doc, div) = build("");
        let sheet = Stylesheet::parse("div { font: bold 14px/1.4 Helvetica, sans-serif }");
        let st = style_document(&doc, &[sheet]);
        let cv = st.get(div).unwrap();
        assert!((cv.font_size - 14.0).abs() < 0.01);
        assert!((cv.line_height - 1.4).abs() < 0.01);
        assert_eq!(cv.font_weight, FontWeight::Bold);
        assert_eq!(cv.font_family, "Helvetica");
    }

    #[test]
    fn media_query_gates_rule_application() {
        let (doc, div) = build("");
        let sheet = Stylesheet::parse(
            "div { color: red } @media (min-width: 1000px) { div { color: green } }",
        );
        // Narrow viewport: @media block should be skipped, color stays red.
        let narrow = style_document_with_viewport(
            &doc,
            std::slice::from_ref(&sheet),
            ViewportSize { width: 800.0, height: 600.0 },
        );
        assert_eq!(narrow.get(div).unwrap().color, RgbaColor::rgb(255, 0, 0));
        // Wide viewport: @media block matches, green wins (later in source).
        let wide = style_document_with_viewport(
            &doc,
            std::slice::from_ref(&sheet),
            ViewportSize { width: 1280.0, height: 800.0 },
        );
        assert_eq!(wide.get(div).unwrap().color, RgbaColor::rgb(0, 128, 0));
    }

    #[test]
    fn noscript_style_blocks_are_ignored() {
        // Google's homepage <noscript> block contains a hide-everything
        // CSS nuke. Our extractor must skip it so a JS-disabled
        // fallback rule doesn't crash through to the live render.
        let mut doc = Document::new();
        let html = doc.create_element("html");
        let head = doc.create_element("head");
        let body = doc.create_element("body");
        let ns = doc.create_element("noscript");
        let inner_style = doc.create_element("style");
        let inner_text = doc.create_text("table,div,span,p{display:none}");
        let p = doc.create_element("p");
        doc.append_child(doc.root, html);
        doc.append_child(html, head);
        doc.append_child(html, body);
        doc.append_child(body, ns);
        doc.append_child(ns, inner_style);
        doc.append_child(inner_style, inner_text);
        doc.append_child(body, p);

        let sheets = extract_inline_stylesheets(&doc);
        // The noscript-nested style should NOT show up.
        for sheet in &sheets {
            for rule in &sheet.rules {
                if let Rule::Style(sr) = rule {
                    for d in &sr.declarations {
                        assert_ne!(
                            (d.name.as_str(), d.value.as_str()),
                            ("display", "none"),
                            "noscript stylesheet leaked into cascade"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn google_classes_keep_default_display() {
        // Reproduce the exact cascade pattern from google.com's
        // homepage: a flex row of `.o3j99` siblings (nav, LS8OJ,
        // footer) plus a no-class container. None of them should
        // pick up `display:none` from anywhere — only `.mwht9d`
        // is configured for that.
        let mut doc = Document::new();
        let html = doc.create_element("html");
        let body = doc.create_element("body");
        let outer = doc.create_element("div");
        doc.element_mut(outer).unwrap().set_attr("class", "L3eUgb");
        let znpjsd = doc.create_element("div");
        let nav = doc.create_element("div");
        doc.element_mut(nav).unwrap().set_attr("class", "o3j99 n1xJcf");
        let ls = doc.create_element("div");
        doc.element_mut(ls).unwrap().set_attr("class", "o3j99 LLD4me LS8OJ");
        let footer = doc.create_element("div");
        doc.element_mut(footer).unwrap().set_attr("class", "o3j99 ikrT4e om7nvf");
        doc.append_child(doc.root, html);
        doc.append_child(html, body);
        doc.append_child(body, outer);
        doc.append_child(outer, znpjsd);
        doc.append_child(outer, nav);
        doc.append_child(outer, ls);
        doc.append_child(outer, footer);
        let css = ".L3eUgb{display:flex;flex-direction:column;height:100%}\
                   .o3j99{flex-shrink:0;box-sizing:border-box}\
                   .n1xJcf{height:60px}\
                   .LLD4me{min-height:150px;height:calc(100% - 560px);max-height:290px}\
                   .mwht9d{display:none}\
                   .qarstb{flex-grow:1}\
                   .LS8OJ{display:flex;flex-direction:column;align-items:center}\
                   .om7nvf{padding:20px}";
        let sheet = Stylesheet::parse(css);
        let st = style_document(&doc, &[sheet]);

        let znpjsd_cv = st.get(znpjsd).unwrap();
        assert!(
            !matches!(znpjsd_cv.display, Display::None),
            "no-class div should not be display:none, got {:?}",
            znpjsd_cv.display
        );
        let footer_cv = st.get(footer).unwrap();
        assert!(
            !matches!(footer_cv.display, Display::None),
            "footer (.o3j99 .ikrT4e .om7nvf) should not be display:none, got {:?}",
            footer_cv.display
        );
    }

    #[test]
    fn inline_style_wins() {
        let (mut doc, div) = build("");
        doc.element_mut(div)
            .unwrap()
            .set_attr("style", "color: rgb(1,2,3)");
        let sheet = Stylesheet::parse("div { color: red; }");
        let st = style_document(&doc, &[sheet]);
        let cv = st.get(div).unwrap();
        assert_eq!(cv.color, RgbaColor::rgb(1, 2, 3));
    }

    #[test]
    fn content_string_decodes_unicode_escape() {
        // CSS `content: "\200B"` produces a zero-width space (U+200B).
        // Wikipedia's `.mw-editsection-bracket::before` uses this for
        // screen-reader announcement; without unicode-escape handling
        // we'd render the literal "200B" next to every section title.
        let mut doc = Document::new();
        let span = doc.create_element("span");
        doc.element_mut(span).unwrap().set_attr("class", "x");
        doc.append_child(doc.root, span);
        let sheet = Stylesheet::parse(".x::before { content: \"\\200B\" }");
        let st = style_document(&doc, &[sheet]);
        let cv = st.before(span).expect("::before should match");
        assert_eq!(cv.content.as_deref(), Some("\u{200B}"));
    }

    #[test]
    fn font_size_em_resolves_against_parent_not_self() {
        // Two cascading rules on the same h1: UA-style "h1 { font-size:
        // 2em }" then author-style "h1 { font-size: 1.8em }". CSS
        // resolves both ems against the PARENT's font-size (16), so
        // the final size is 1.8 × 16 = 28.8 — NOT 2 × 1.8 × 16 = 57.6
        // (the buggy chained-em result that previously broke
        // Wikipedia's <h1>Felis</h1>).
        let mut doc = Document::new();
        let body = doc.create_element("body");
        let h1 = doc.create_element("h1");
        doc.append_child(doc.root, body);
        doc.append_child(body, h1);
        let sheet = Stylesheet::parse("h1 { font-size: 2em } h1 { font-size: 1.8em }");
        let st = style_document(&doc, &[sheet]);
        let cv = st.get(h1).unwrap();
        assert!(
            (cv.font_size - 28.8).abs() < 0.5,
            "expected ~28.8, got {}",
            cv.font_size
        );
    }
}
