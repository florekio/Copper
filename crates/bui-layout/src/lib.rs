//! bui-layout — box tree, block + inline layout.
//!
//! Phase 4 subset:
//!   * Block formatting context: vertical stack, content width = parent
//!     content width minus margin/border/padding, height = sum of children.
//!   * Inline formatting context: text runs are word-wrapped using a
//!     monospace metric (`char_width = font_size * 0.55`). Each line in the
//!     wrapped run becomes its own line box.
//!   * `display: none` skipped, void elements ignored, anonymous block
//!     boxes for stray inline content directly inside an element with mixed
//!     children.
//!
//! Out of scope (later phases): floats, flexbox (Phase 7), grid (Phase 11),
//! tables (Phase 7+), real text shaping (Phase 6), images (Phase 8).

pub mod svg;

use std::collections::HashMap;

use bui_dom::{Document, NodeId, NodeKind};
use bui_paint::{Color, DisplayList, PaintCommand, Rect};
use bui_style::{
    AlignItems, ComputedValues, Dimension, Display, EdgeSizes, FlexBasis, FlexDirection,
    GridLine, JustifyContent, Length, MinMaxSide, RgbaColor, StyleTree, TrackSize,
};

pub use svg::SvgEntry;

/// Per-element image metadata, used to give `<img>` a real intrinsic size
/// and a stable cache key for the renderer's texture lookup.
#[derive(Debug, Clone)]
pub struct ImageEntry {
    pub width: f32,
    pub height: f32,
    pub key: String,
}

pub type ImageRegistry = HashMap<NodeId, ImageEntry>;

/// Per-element SVG payload, used when the source the renderer would
/// have rasterised turned out to be a vector image. Populated by the
/// fetch loop in the binary; consumed by the build path when it sees
/// an `<img>` whose `NodeId` is registered here.
pub type SvgRegistry = HashMap<NodeId, svg::SvgEntry>;

#[derive(Debug, Clone, Copy)]
pub struct Frame {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Frame {
    pub const ZERO: Self = Self {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
    };
}

#[derive(Debug)]
pub enum BoxKind {
    Block,
    Anonymous,
    InlineText(String),
    InlineImage(ImageEntry),
    /// `<input>` or `<button>` — replaced inline element with a single
    /// label string that paints inside its own bordered rect.
    InlineControl(ControlEntry),
    /// `<svg>` — replaced inline element with a vector graphic. The
    /// payload carries the parsed shape list and viewBox; the renderer
    /// scales each shape to fit the laid-out frame.
    InlineSvg(SvgEntry),
    /// `<br>` — forced line break. Has no painted geometry; the
    /// inline layout commits the current line and starts a new one
    /// when it encounters this kind.
    InlineBreak,
    /// `display: inline-block` — a block-formatting context that
    /// participates inline. We build the whole block subtree up
    /// front; layout_inline lays it out at shrink-to-fit width when
    /// it encounters one and places it as a `LineItem::InlineBlock`.
    InlineBlockHost(Box<LayoutBox>),
}

#[derive(Debug, Clone)]
pub struct ControlEntry {
    pub label: String,
    pub kind: ControlKind,
    /// True when `label` came from `placeholder` (no real value typed
    /// yet). Paint draws this in a muted color to mirror browser
    /// `::placeholder` styling. Always `false` for non-Input kinds.
    pub is_placeholder: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlKind {
    /// `<input>` with type=text/search/email/url/tel/password (and the
    /// implied default). `value` attribute is the label.
    Input,
    /// `<button>`, or `<input type="submit"|"button"|"reset">`. For
    /// `<button>` the label is its rendered text content; for the
    /// submit-style inputs it's `value` (or the type-default fallback).
    Button,
    /// `<input type="checkbox">` — small square indicator. Painted
    /// with a check glyph when `checked`.
    Checkbox { checked: bool },
    /// `<input type="radio">` — small circle indicator. Painted
    /// filled when `checked`.
    Radio { checked: bool },
}

#[derive(Debug)]
pub struct LayoutBox {
    pub node: Option<NodeId>,
    pub style: ComputedValues,
    pub kind: BoxKind,
    pub children: Vec<LayoutBox>,
    pub frame: Frame,
    /// Wrapped lines for inline content inside a block. Populated during layout.
    pub lines: Vec<LineBox>,
    /// `<li>` marker. Set during tree build so paint can stamp a
    /// "•" / "1." / "a." in the parent list's padding-left strip.
    pub list_marker: Option<String>,
    /// `colspan` from the source `<td>` / `<th>`. 1 for non-cells
    /// and for cells without an explicit colspan attribute.
    pub colspan: u32,
    /// `rowspan` from the source `<td>` / `<th>`. 1 for non-cells.
    pub rowspan: u32,
    /// On a `<table>` LayoutBox: pre-extracted column widths from
    /// `<col>` / `<colgroup>` children, in source column order.
    /// `None` means no `<col>` declared a width for that column;
    /// that column's width falls back to the first-row cell's
    /// declared width or auto-share. Empty for non-tables.
    pub col_widths: Vec<Option<f32>>,
}

#[derive(Debug)]
pub struct LineBox {
    pub frame: Frame,
    pub items: Vec<LineItem>,
}

#[derive(Debug)]
pub enum LineItem {
    Text(TextRun),
    Image {
        frame: Frame,
        key: String,
        /// Originating DOM node — used by `hit_test` so clicks on
        /// images / replaced inline boxes resolve to the correct
        /// element (e.g. an `<img>` inside an `<a>`).
        node: Option<NodeId>,
        vertical_align: bui_style::VerticalAlign,
        /// Intrinsic image dimensions in CSS pixels — needed by the
        /// paint pass to compute object-fit cropping vs. stretching.
        intrinsic: (f32, f32),
        object_fit: bui_style::ObjectFit,
    },
    /// `<input>` / `<button>` — paints its own background, border, and
    /// the label text inside.
    Control {
        frame: Frame,
        label: String,
        kind: ControlKind,
        style: ComputedValues,
        node: Option<NodeId>,
        /// True when `label` is the input's `placeholder` rather
        /// than a user-typed / declared value. Painted muted.
        is_placeholder: bool,
    },
    /// `<svg>` — replaced inline graphic. Renderer maps the entry's
    /// view-box to `frame` and emits one paint command per shape.
    Svg {
        frame: Frame,
        entry: SvgEntry,
        node: Option<NodeId>,
        vertical_align: bui_style::VerticalAlign,
    },
    /// A laid-out `display: inline-block` subtree. `host` is its full
    /// box tree (already positioned in absolute coords by the inline
    /// layout); `frame` is its outer rect. Paint walks the host
    /// recursively just like any block.
    InlineBlock {
        frame: Frame,
        host: Box<LayoutBox>,
        node: Option<NodeId>,
        vertical_align: bui_style::VerticalAlign,
    },
}

#[derive(Debug)]
pub struct TextRun {
    pub text: String,
    pub style: ComputedValues,
    pub frame: Frame,
    /// Originating DOM text node — used by `hit_test` so clicks on
    /// rendered glyphs resolve to the right element after climbing the
    /// parent chain via `enclosing_anchor`.
    pub node: Option<NodeId>,
}

/// Build the layout tree starting from a DOM node (typically `<body>` or
/// the document root) using a precomputed style tree.
pub fn build(doc: &Document, style: &StyleTree, root: NodeId) -> LayoutBox {
    build_with_images(doc, style, &ImageRegistry::new(), &SvgRegistry::new(), root)
}

/// Same as `build`, plus an `ImageRegistry` so `<img>` elements with a
/// loaded resource render with their intrinsic dimensions and a paint
/// reference.
pub fn build_with_images(
    doc: &Document,
    style: &StyleTree,
    images: &ImageRegistry,
    svgs: &SvgRegistry,
    root: NodeId,
) -> LayoutBox {
    build_block(doc, style, images, svgs, root)
}

fn build_block(
    doc: &Document,
    style: &StyleTree,
    images: &ImageRegistry,
    svgs: &SvgRegistry,
    node: NodeId,
) -> LayoutBox {
    let cv = style.get(node).cloned().unwrap_or_else(ComputedValues::root_default);
    let list_marker = if matches!(cv.display, Display::ListItem) {
        compute_list_marker(doc, style, node)
    } else {
        None
    };
    let col_widths = if doc.element(node).map(|e| e.name == "table").unwrap_or(false) {
        extract_col_widths(doc, node)
    } else {
        Vec::new()
    };
    // Block-level replaced elements: `<img display: block>` (or flex/grid
    // item) still has image content to paint, but the inline-flow
    // branches that normally pick up InlineImage / InlineSvg don't run
    // because `display: block` routes through here. Wrap the loaded
    // resource in an Anonymous → InlineImage / InlineSvg subtree so
    // line layout sizes the Block to the image and paints it.
    // Without this, Wikipedia's `.mw-logo-wordmark { display: block }`
    // collapsed to an empty 140×22 rectangle with no logo visible.
    let img_replaced: Option<BoxKind> = doc.element(node).and_then(|e| {
        if e.name != "img" {
            return None;
        }
        if let Some(entry) = svgs.get(&node) {
            return Some(BoxKind::InlineSvg(entry.clone()));
        }
        if let Some(entry) = images.get(&node) {
            let mut entry = entry.clone();
            if let Some(w) = e.get_attr("width").and_then(|s| s.parse::<f32>().ok()) {
                entry.width = w;
            }
            if let Some(h) = e.get_attr("height").and_then(|s| s.parse::<f32>().ok()) {
                entry.height = h;
            }
            return Some(BoxKind::InlineImage(entry));
        }
        None
    });
    let mut bx = LayoutBox {
        node: Some(node),
        style: cv.clone(),
        kind: BoxKind::Block,
        children: Vec::new(),
        frame: Frame::ZERO,
        lines: Vec::new(),
        list_marker,
        colspan: read_cell_span(doc, node, "colspan"),
        rowspan: read_cell_span(doc, node, "rowspan"),
        col_widths,
    };
    if let Some(replaced_kind) = img_replaced {
        // Synth an Anonymous IFC wrapper holding the replaced leaf so
        // existing inline-layout handles sizing + paint.
        let mut anon_style = cv.clone();
        anon_style.padding = bui_style::EdgeSizes::ZERO;
        anon_style.margin = bui_style::EdgeSizes::ZERO;
        anon_style.border = bui_style::EdgeSizes::ZERO;
        anon_style.display = Display::Block;
        let leaf = LayoutBox {
            node: Some(node),
            style: cv,
            kind: replaced_kind,
            children: Vec::new(),
            frame: Frame::ZERO,
            lines: Vec::new(),
            list_marker: None,
            colspan: 1, rowspan: 1, col_widths: Vec::new(),
        };
        bx.children.push(LayoutBox {
            node: None,
            style: anon_style,
            kind: BoxKind::Anonymous,
            children: vec![leaf],
            frame: Frame::ZERO,
            lines: Vec::new(),
            list_marker: None,
            colspan: 1, rowspan: 1, col_widths: Vec::new(),
        });
        return bx;
    }
    collect_children(doc, style, images, svgs, node, &mut bx);
    bx
}

/// Pull a `colspan` / `rowspan` attribute off a `<td>` / `<th>`.
/// Returns 1 for non-cell elements and for missing / malformed
/// attributes — keeps the call sites unconditional.
/// Walk a `<table>`'s direct `<col>` / `<colgroup>` children and
/// return per-column widths in source column order. `<col span="N">`
/// repeats its declared width across N columns. Widths come from
/// either the HTML `width` attribute (presentational) or any inline
/// `style` declaration. Columns without an explicit width get
/// `None` (the table's column resolver falls back to first-row
/// cell widths or auto-share for those).
fn extract_col_widths(doc: &Document, table: NodeId) -> Vec<Option<f32>> {
    fn parse_width(s: &str) -> Option<f32> {
        let t = s.trim();
        // Accept "120", "120px", "120.5"; reject "30%" (we don't
        // resolve percent here without a containing-block width).
        let stripped = t.strip_suffix("px").unwrap_or(t);
        stripped.parse::<f32>().ok().filter(|n| *n >= 0.0 && n.is_finite())
    }
    fn handle_col(doc: &Document, col: NodeId, out: &mut Vec<Option<f32>>) {
        let Some(elem) = doc.element(col) else { return };
        let span: usize = elem
            .get_attr("span")
            .and_then(|s| s.parse::<u32>().ok())
            .map(|n| n.max(1) as usize)
            .unwrap_or(1);
        let w = elem.get_attr("width").and_then(parse_width);
        for _ in 0..span {
            out.push(w);
        }
    }
    let mut out: Vec<Option<f32>> = Vec::new();
    let mut child = doc.node(table).first_child;
    while let Some(c) = child {
        if let Some(elem) = doc.element(c) {
            match elem.name.as_str() {
                "col" => handle_col(doc, c, &mut out),
                "colgroup" => {
                    // <colgroup> may have its own width attribute
                    // (applies to all its <col> children that don't
                    // override) or it may simply contain <col>s.
                    let group_w = elem.get_attr("width").and_then(parse_width);
                    let group_span: usize = elem
                        .get_attr("span")
                        .and_then(|s| s.parse::<u32>().ok())
                        .map(|n| n.max(1) as usize)
                        .unwrap_or(0);
                    let mut had_col = false;
                    let mut gc = doc.node(c).first_child;
                    while let Some(g) = gc {
                        if let Some(ge) = doc.element(g) {
                            if ge.name == "col" {
                                had_col = true;
                                // Inherit the colgroup's width if the
                                // <col> didn't declare its own.
                                let gspan: usize = ge
                                    .get_attr("span")
                                    .and_then(|s| s.parse::<u32>().ok())
                                    .map(|n| n.max(1) as usize)
                                    .unwrap_or(1);
                                let w = ge
                                    .get_attr("width")
                                    .and_then(parse_width)
                                    .or(group_w);
                                for _ in 0..gspan {
                                    out.push(w);
                                }
                            }
                        }
                        gc = doc.node(g).next_sibling;
                    }
                    // <colgroup span="N" width="..."> with no inner
                    // <col> children: declare N columns directly.
                    if !had_col && group_span > 0 {
                        for _ in 0..group_span {
                            out.push(group_w);
                        }
                    }
                }
                _ => {}
            }
        }
        child = doc.node(c).next_sibling;
    }
    out
}

fn read_cell_span(doc: &Document, node: NodeId, attr: &str) -> u32 {
    doc.element(node)
        .and_then(|e| {
            if matches!(e.name.as_str(), "td" | "th") {
                e.get_attr(attr).and_then(|s| s.parse::<u32>().ok())
            } else {
                None
            }
        })
        .map(|n| n.max(1))
        .unwrap_or(1)
}

/// Compute the marker label for a `display: list-item` node. The
/// marker glyph follows the parent list's `list-style-type` (or the
/// item's own, since the property inherits). Item index counts only
/// element siblings inside the list ancestor.
fn compute_list_marker(
    doc: &Document,
    style: &StyleTree,
    node: NodeId,
) -> Option<String> {
    let parent_id = doc.node(node).parent?;
    let parent = doc.element(parent_id)?;
    if !matches!(parent.name.as_str(), "ul" | "ol" | "menu" | "dl") {
        return None;
    }
    // Count only same-tag element siblings before us. A stray <p> /
    // <style> / non-li element inside a <ul> shouldn't shift the
    // numbering of the actual list items — what `<ol>` cares about
    // is the count of `<li>`s, not the count of any element child.
    let target_name = doc.element(node).map(|e| e.name.clone());
    let mut idx = 1usize;
    let mut cur = doc.node(parent_id).first_child;
    while let Some(c) = cur {
        if c == node {
            break;
        }
        if let NodeKind::Element(_) = &doc.node(c).kind {
            if let (Some(want), Some(got)) = (
                target_name.as_deref(),
                doc.element(c).map(|e| e.name.as_str()),
            ) {
                if want == got {
                    idx += 1;
                }
            }
        }
        cur = doc.node(c).next_sibling;
    }
    let lst = style
        .get(node)
        .map(|cv| cv.list_style_type)
        .unwrap_or(bui_style::ListStyleType::Disc);
    Some(format_list_marker(lst, idx))
}

fn format_list_marker(t: bui_style::ListStyleType, idx: usize) -> String {
    match t {
        bui_style::ListStyleType::None => String::new(),
        bui_style::ListStyleType::Disc => "•".to_string(),
        bui_style::ListStyleType::Circle => "◦".to_string(),
        bui_style::ListStyleType::Square => "▪".to_string(),
        bui_style::ListStyleType::Decimal => format!("{idx}."),
        bui_style::ListStyleType::DecimalLeadingZero => format!("{idx:02}."),
        bui_style::ListStyleType::LowerAlpha => {
            format!("{}.", to_alpha(idx, false))
        }
        bui_style::ListStyleType::UpperAlpha => {
            format!("{}.", to_alpha(idx, true))
        }
        bui_style::ListStyleType::LowerRoman => format!("{}.", to_roman(idx, false)),
        bui_style::ListStyleType::UpperRoman => format!("{}.", to_roman(idx, true)),
    }
}

fn to_alpha(mut n: usize, upper: bool) -> String {
    if n == 0 {
        return String::new();
    }
    let base = if upper { b'A' } else { b'a' };
    let mut chars = Vec::new();
    while n > 0 {
        n -= 1;
        chars.push((base + (n % 26) as u8) as char);
        n /= 26;
    }
    chars.iter().rev().collect()
}

fn to_roman(mut n: usize, upper: bool) -> String {
    static PAIRS: &[(usize, &str)] = &[
        (1000, "m"), (900, "cm"), (500, "d"), (400, "cd"),
        (100, "c"), (90, "xc"), (50, "l"), (40, "xl"),
        (10, "x"), (9, "ix"), (5, "v"), (4, "iv"), (1, "i"),
    ];
    let mut s = String::new();
    for &(value, sym) in PAIRS {
        while n >= value {
            s.push_str(sym);
            n -= value;
        }
    }
    if upper { s.to_ascii_uppercase() } else { s }
}

/// One unit of pending inline content. Either a real DOM node we'll
/// recurse into, or a pre-built `LayoutBox` for a synthesized
/// pseudo-element (`::before` / `::after`).
enum PendingInline {
    Dom(NodeId),
    Synth(LayoutBox),
}

/// Flatten `display: contents` wrappers in the DOM-child sequence
/// of `parent`. A wrapper element produces no box; its own DOM
/// children participate in the parent's flow as if the wrapper
/// weren't there. Cascade still applies to descendants normally
/// because they have their own entry in the StyleTree. Recurses so
/// nested `display: contents` wrappers also flatten.
///
/// Inline-style elements like `<span>` already behave this way via
/// `collect_inline`'s transparent recursion, so we apply this only
/// when needed at the block-flow boundary.
fn effective_children(
    doc: &Document,
    style: &StyleTree,
    parent: NodeId,
) -> Vec<NodeId> {
    let mut out: Vec<NodeId> = Vec::new();
    let mut child = doc.node(parent).first_child;
    while let Some(c) = child {
        let next = doc.node(c).next_sibling;
        if doc.element(c).is_some() {
            let cv = style
                .get(c)
                .cloned()
                .unwrap_or_else(ComputedValues::root_default);
            if matches!(cv.display, Display::Contents) {
                out.extend(effective_children(doc, style, c));
                child = next;
                continue;
            }
        }
        out.push(c);
        child = next;
    }
    out
}

/// True when an anonymous block contains only whitespace inline
/// content. Used by flex / grid item collection to drop the phantom
/// items that source whitespace produces between block siblings.
fn anon_is_whitespace_only(bx: &LayoutBox) -> bool {
    if !matches!(bx.kind, BoxKind::Anonymous) {
        return false;
    }
    bx.children.iter().all(|c| match &c.kind {
        BoxKind::InlineText(t) => t.chars().all(|ch| ch.is_whitespace()),
        BoxKind::InlineBreak => true,
        _ => false,
    })
}

fn collect_children(
    doc: &Document,
    style: &StyleTree,
    images: &ImageRegistry,
    svgs: &SvgRegistry,
    parent: NodeId,
    parent_box: &mut LayoutBox,
) {
    // Walk DOM children with `display: contents` wrappers expanded
    // so their grandchildren participate as direct flow items here.
    let effective = effective_children(doc, style, parent);
    let mut iter = effective.into_iter();
    let mut pending_inline: Vec<PendingInline> = Vec::new();

    // CSS `::before` content goes at the very start of the element's
    // children (before any DOM child, before any inline content). We
    // build it as a synthetic InlineText box so it joins the inline
    // flow naturally.
    if let Some(synth) = build_pseudo_synth(style, parent, true) {
        pending_inline.push(PendingInline::Synth(synth));
    }

    fn flush(
        parent_box: &mut LayoutBox,
        pending: &mut Vec<PendingInline>,
        doc: &Document,
        style: &StyleTree,
        images: &ImageRegistry,
    svgs: &SvgRegistry,
    ) {
        if pending.is_empty() {
            return;
        }
        let drained: Vec<PendingInline> = std::mem::take(pending);
        // Synthetic anonymous wrappers inherit typography (font,
        // color, line-height, …) but NOT geometry — padding, margin,
        // and border belong to the parent's box. Without this strip,
        // a `<a>` with `padding: 12px 0 7px` ended up with 12+7=19 px
        // of padding on every nested anonymous wrapper, ballooning
        // the height (Wikipedia's tab `<li>` came out 73 px tall on
        // a single 17 px line of "Article" because four nested
        // anonymous wrappers each re-applied the same padding).
        let mut anon_style = parent_box.style.clone();
        anon_style.padding = bui_style::EdgeSizes::ZERO;
        anon_style.margin = bui_style::EdgeSizes::ZERO;
        anon_style.border = bui_style::EdgeSizes::ZERO;
        anon_style.display = Display::Block;
        let mut anon = LayoutBox {
            node: None,
            style: anon_style,
            kind: BoxKind::Anonymous,
            children: Vec::new(),
            frame: Frame::ZERO,
            lines: Vec::new(),
            list_marker: None,
            colspan: 1, rowspan: 1, col_widths: Vec::new(),
        };
        for item in drained {
            match item {
                PendingInline::Dom(id) => collect_inline(doc, style, images, svgs, id, &mut anon),
                PendingInline::Synth(b) => anon.children.push(b),
            }
        }
        // Don't push a wrapper that wraps nothing but whitespace text
        // between block siblings — every newline in the source HTML
        // would otherwise produce an empty Anonymous box that adds
        // nothing visible but takes layout time and dump space.
        if anon.children.is_empty() || anon_is_whitespace_only(&anon) {
            return;
        }
        parent_box.children.push(anon);
    }

    // CSS Flexbox / Grid: every in-flow ELEMENT child of a flex / grid
    // container is its own item, regardless of its own `display`. Only
    // contiguous text-runs are wrapped in an anonymous flex / grid item.
    // Without this, two `<a class=MV3Tnb>` (display: inline-block) and
    // a `<div class=LX3sZb>` siblings inside a flex container all got
    // merged into one anonymous wrapper, which then laid out as a
    // single block — pushing each child onto its own line because
    // `<div>` flex-grow:1 expanded to fill the container.
    let parent_is_flex_or_grid = matches!(
        parent_box.style.display,
        Display::Flex | Display::Grid
    );
    while let Some(c) = iter.next() {
        match &doc.node(c).kind {
            NodeKind::Element(_) => {
                let cv = style.get(c).cloned().unwrap_or_else(ComputedValues::root_default);
                // `Contents` was already expanded by effective_children;
                // any leftover here means the wrapper itself slipped
                // through (defensive). `None` is a hard skip.
                if matches!(cv.display, Display::None | Display::Contents) {
                    continue;
                }
                // <textarea> is a REPLACED element — author CSS can
                // declare `display: flex` (Google's search box uses
                // exactly this) but in real browsers the inner DOM
                // is hidden and a UA-managed widget paints in its
                // box. Route through inline-control path so the
                // page-level input buffer renders / accepts focus /
                // typing. Other form controls (input/button/select)
                // already work via their default inline-block UA
                // display; only textarea needs the short-circuit
                // because its content model includes child text
                // that would otherwise become rendered children.
                if doc.element(c).map(|e| e.name == "textarea").unwrap_or(false) {
                    pending_inline.push(PendingInline::Dom(c));
                    continue;
                }
                // Per CSS Display §6, `position: absolute | fixed`
                // forces an element to be block-level regardless of its
                // declared `display`. Without this, Wikipedia's
                // `<input type="checkbox" class="vector-dropdown-checkbox">`
                // (style: `position: absolute; opacity: 0`) joined the
                // inline flow and added a 19-px line item to the
                // header, pushing the entire `vector-header` from
                // ~51 px to 88 px. The absolute box now becomes a
                // direct block child and is removed from flow by
                // layout_block's `out_of_flow` skip.
                let is_out_of_flow = matches!(
                    cv.position,
                    bui_style::Position::Absolute | bui_style::Position::Fixed,
                );
                // `<input>` is a replaced element, like <textarea> above.
                // Author CSS can set `display: flex` on it (DuckDuckGo's
                // homepage does: `.searchInput { display: flex }`) but a
                // real browser never lays an <input> out as a flex/grid
                // container — it paints a UA widget in the box. Without
                // this short-circuit, build_block turned the input into an
                // empty flex container and its value/placeholder text was
                // dropped (DDG's search bar rendered as a blank pill).
                // Route visible, in-flow inputs through the inline-control
                // path. Out-of-flow inputs (e.g. Wikipedia's `position:
                // absolute; opacity: 0` dropdown checkbox) still fall
                // through to the block path so they're removed from flow.
                let is_visible_input = doc
                    .element(c)
                    .map(|e| {
                        e.name == "input"
                            && !matches!(
                                e.get_attr("type").map(|s| s.to_ascii_lowercase()).as_deref(),
                                Some("hidden") | Some("file") | Some("image")
                            )
                    })
                    .unwrap_or(false);
                if is_visible_input && !is_out_of_flow {
                    pending_inline.push(PendingInline::Dom(c));
                    continue;
                }
                let is_block = parent_is_flex_or_grid || is_out_of_flow || matches!(
                    cv.display,
                    Display::Block
                        | Display::ListItem
                        | Display::Flex
                        | Display::Grid
                        | Display::Table
                        | Display::TableRow
                        | Display::TableCell
                );
                if is_block {
                    flush(parent_box, &mut pending_inline, doc, style, images, svgs);
                    let bx = build_block(doc, style, images, svgs, c);
                    parent_box.children.push(bx);
                } else {
                    pending_inline.push(PendingInline::Dom(c));
                }
            }
            NodeKind::Text(_) => {
                pending_inline.push(PendingInline::Dom(c));
            }
            _ => {}
        }
    }
    if let Some(synth) = build_pseudo_synth(style, parent, false) {
        pending_inline.push(PendingInline::Synth(synth));
    }
    flush(parent_box, &mut pending_inline, doc, style, images, svgs);
}

/// Build a synthetic inline LayoutBox for a `::before` (when
/// `is_before` is true) or `::after` pseudo-element. Returns `None`
/// if the cascade didn't produce a `content` value for this slot.
fn build_pseudo_synth(
    style: &StyleTree,
    node: NodeId,
    is_before: bool,
) -> Option<LayoutBox> {
    let cv = if is_before {
        style.before(node)?
    } else {
        style.after(node)?
    };
    let text = cv.content.clone()?;
    Some(LayoutBox {
        node: None,
        style: cv.clone(),
        kind: BoxKind::InlineText(text),
        children: Vec::new(),
        frame: Frame::ZERO,
        lines: Vec::new(),
        list_marker: None,
        colspan: 1, rowspan: 1, col_widths: Vec::new(),
    })
}

fn collect_inline(
    doc: &Document,
    style: &StyleTree,
    images: &ImageRegistry,
    svgs: &SvgRegistry,
    node: NodeId,
    parent: &mut LayoutBox,
) {
    match &doc.node(node).kind {
        NodeKind::Text(t) => {
            // Whitespace collapse per CSS Normal: any run of whitespace becomes a single space.
            let collapsed = collapse_whitespace(t);
            if collapsed.is_empty() {
                return;
            }
            let cv = style.get(node).cloned().unwrap_or_else(|| parent.style.clone());
            parent.children.push(LayoutBox {
                node: Some(node),
                style: cv,
                kind: BoxKind::InlineText(collapsed),
                children: Vec::new(),
                frame: Frame::ZERO,
                lines: Vec::new(), list_marker: None, colspan: 1, rowspan: 1, col_widths: Vec::new(),
            });
        }
        NodeKind::Element(_) => {
            let cv = style.get(node).cloned().unwrap_or_else(|| parent.style.clone());
            if matches!(cv.display, Display::None) {
                return;
            }
            // <img> with a loaded resource: replaced inline box.
            if let Some(elem) = doc.element(node) {
                if elem.name == "img" {
                    // Vector first — when the fetched resource turned
                    // out to be SVG, we route it through the inline-SVG
                    // paint path so it scales crisply.
                    if let Some(entry) = svgs.get(&node) {
                        parent.children.push(LayoutBox {
                            node: Some(node),
                            style: cv.clone(),
                            kind: BoxKind::InlineSvg(entry.clone()),
                            children: Vec::new(),
                            frame: Frame::ZERO,
                            lines: Vec::new(), list_marker: None, colspan: 1, rowspan: 1, col_widths: Vec::new(),
                        });
                        return;
                    }
                    if let Some(entry) = images.get(&node) {
                        // HTML `width` / `height` attributes override
                        // the decoded image's intrinsic dims. Authors
                        // routinely declare these so the box reserves
                        // the right space before the image arrives;
                        // CSS width / height (consumed downstream)
                        // still takes precedence.
                        let mut entry = entry.clone();
                        if let Some(w) = elem.get_attr("width").and_then(|s| s.parse::<f32>().ok()) {
                            entry.width = w;
                        }
                        if let Some(h) = elem.get_attr("height").and_then(|s| s.parse::<f32>().ok()) {
                            entry.height = h;
                        }
                        parent.children.push(LayoutBox {
                            node: Some(node),
                            style: cv.clone(),
                            kind: BoxKind::InlineImage(entry),
                            children: Vec::new(),
                            frame: Frame::ZERO,
                            lines: Vec::new(), list_marker: None, colspan: 1, rowspan: 1, col_widths: Vec::new(),
                        });
                    }
                    return;
                }
                if let Some(entry) = control_entry(doc, node) {
                    parent.children.push(LayoutBox {
                        node: Some(node),
                        style: cv.clone(),
                        kind: BoxKind::InlineControl(entry),
                        children: Vec::new(),
                        frame: Frame::ZERO,
                        lines: Vec::new(), list_marker: None, colspan: 1, rowspan: 1, col_widths: Vec::new(),
                    });
                    return;
                }
                if elem.name == "svg" {
                    let host_color = bui_paint::Color::rgba(
                        cv.color.r,
                        cv.color.g,
                        cv.color.b,
                        cv.color.a,
                    );
                    if let Some(entry) = svg::parse_svg_with_color(doc, node, host_color) {
                        parent.children.push(LayoutBox {
                            node: Some(node),
                            style: cv.clone(),
                            kind: BoxKind::InlineSvg(entry),
                            children: Vec::new(),
                            frame: Frame::ZERO,
                            lines: Vec::new(), list_marker: None, colspan: 1, rowspan: 1, col_widths: Vec::new(),
                        });
                    }
                    return;
                }
                if elem.name == "br" {
                    parent.children.push(LayoutBox {
                        node: Some(node),
                        style: cv.clone(),
                        kind: BoxKind::InlineBreak,
                        children: Vec::new(),
                        frame: Frame::ZERO,
                        lines: Vec::new(),
                        list_marker: None,
                        colspan: 1, rowspan: 1, col_widths: Vec::new(),
                    });
                    return;
                }
                if matches!(cv.display, Display::InlineBlock) {
                    // Build the inline-block as a full block subtree.
                    // We give it `display: Block` for the build pass so
                    // collect_children groups its descendants the same
                    // way it would for a normal `<div>`. Layout flips
                    // the subtree's root back to inline-block-aware
                    // sizing later.
                    let mut sub_style = cv.clone();
                    sub_style.display = Display::Block;
                    let mut host = LayoutBox {
                        node: Some(node),
                        style: sub_style,
                        kind: BoxKind::Block,
                        children: Vec::new(),
                        frame: Frame::ZERO,
                        lines: Vec::new(),
                        list_marker: None,
                        colspan: 1, rowspan: 1, col_widths: Vec::new(),
                    };
                    collect_children(doc, style, images, svgs, node, &mut host);
                    parent.children.push(LayoutBox {
                        node: Some(node),
                        style: cv.clone(),
                        kind: BoxKind::InlineBlockHost(Box::new(host)),
                        children: Vec::new(),
                        frame: Frame::ZERO,
                        lines: Vec::new(),
                        list_marker: None,
                        colspan: 1, rowspan: 1, col_widths: Vec::new(),
                    });
                    return;
                }
            }
            // For other inline elements, treat as transparent: recurse into
            // children with the inline element's style applied.
            if let Some(b) = build_pseudo_synth(style, node, true) {
                parent.children.push(b);
            }
            let mut child = doc.node(node).first_child;
            while let Some(c) = child {
                collect_inline_with_style(doc, style, images, svgs, c, parent, &cv);
                child = doc.node(c).next_sibling;
            }
            if let Some(a) = build_pseudo_synth(style, node, false) {
                parent.children.push(a);
            }
        }
        _ => {}
    }
}

fn collect_inline_with_style(
    doc: &Document,
    style: &StyleTree,
    images: &ImageRegistry,
    svgs: &SvgRegistry,
    node: NodeId,
    parent: &mut LayoutBox,
    inline_style: &ComputedValues,
) {
    match &doc.node(node).kind {
        NodeKind::Text(t) => {
            let collapsed = collapse_whitespace(t);
            if collapsed.is_empty() {
                return;
            }
            parent.children.push(LayoutBox {
                node: Some(node),
                style: inline_style.clone(),
                kind: BoxKind::InlineText(collapsed),
                children: Vec::new(),
                frame: Frame::ZERO,
                lines: Vec::new(), list_marker: None, colspan: 1, rowspan: 1, col_widths: Vec::new(),
            });
        }
        NodeKind::Element(_) => {
            let cv = style.get(node).cloned().unwrap_or_else(|| inline_style.clone());
            if matches!(cv.display, Display::None) {
                return;
            }
            if let Some(elem) = doc.element(node) {
                if elem.name == "img" {
                    // Vector first — when the fetched resource turned
                    // out to be SVG, we route it through the inline-SVG
                    // paint path so it scales crisply.
                    if let Some(entry) = svgs.get(&node) {
                        parent.children.push(LayoutBox {
                            node: Some(node),
                            style: cv.clone(),
                            kind: BoxKind::InlineSvg(entry.clone()),
                            children: Vec::new(),
                            frame: Frame::ZERO,
                            lines: Vec::new(), list_marker: None, colspan: 1, rowspan: 1, col_widths: Vec::new(),
                        });
                        return;
                    }
                    if let Some(entry) = images.get(&node) {
                        // HTML `width` / `height` attributes override
                        // the decoded image's intrinsic dims. Authors
                        // routinely declare these so the box reserves
                        // the right space before the image arrives;
                        // CSS width / height (consumed downstream)
                        // still takes precedence.
                        let mut entry = entry.clone();
                        if let Some(w) = elem.get_attr("width").and_then(|s| s.parse::<f32>().ok()) {
                            entry.width = w;
                        }
                        if let Some(h) = elem.get_attr("height").and_then(|s| s.parse::<f32>().ok()) {
                            entry.height = h;
                        }
                        parent.children.push(LayoutBox {
                            node: Some(node),
                            style: cv.clone(),
                            kind: BoxKind::InlineImage(entry),
                            children: Vec::new(),
                            frame: Frame::ZERO,
                            lines: Vec::new(), list_marker: None, colspan: 1, rowspan: 1, col_widths: Vec::new(),
                        });
                    }
                    return;
                }
                if let Some(entry) = control_entry(doc, node) {
                    parent.children.push(LayoutBox {
                        node: Some(node),
                        style: cv.clone(),
                        kind: BoxKind::InlineControl(entry),
                        children: Vec::new(),
                        frame: Frame::ZERO,
                        lines: Vec::new(), list_marker: None, colspan: 1, rowspan: 1, col_widths: Vec::new(),
                    });
                    return;
                }
                if elem.name == "svg" {
                    let host_color = bui_paint::Color::rgba(
                        cv.color.r,
                        cv.color.g,
                        cv.color.b,
                        cv.color.a,
                    );
                    if let Some(entry) = svg::parse_svg_with_color(doc, node, host_color) {
                        parent.children.push(LayoutBox {
                            node: Some(node),
                            style: cv.clone(),
                            kind: BoxKind::InlineSvg(entry),
                            children: Vec::new(),
                            frame: Frame::ZERO,
                            lines: Vec::new(), list_marker: None, colspan: 1, rowspan: 1, col_widths: Vec::new(),
                        });
                    }
                    return;
                }
                if elem.name == "br" {
                    parent.children.push(LayoutBox {
                        node: Some(node),
                        style: cv.clone(),
                        kind: BoxKind::InlineBreak,
                        children: Vec::new(),
                        frame: Frame::ZERO,
                        lines: Vec::new(),
                        list_marker: None,
                        colspan: 1, rowspan: 1, col_widths: Vec::new(),
                    });
                    return;
                }
                if matches!(cv.display, Display::InlineBlock) {
                    // Build the inline-block as a full block subtree.
                    // We give it `display: Block` for the build pass so
                    // collect_children groups its descendants the same
                    // way it would for a normal `<div>`. Layout flips
                    // the subtree's root back to inline-block-aware
                    // sizing later.
                    let mut sub_style = cv.clone();
                    sub_style.display = Display::Block;
                    let mut host = LayoutBox {
                        node: Some(node),
                        style: sub_style,
                        kind: BoxKind::Block,
                        children: Vec::new(),
                        frame: Frame::ZERO,
                        lines: Vec::new(),
                        list_marker: None,
                        colspan: 1, rowspan: 1, col_widths: Vec::new(),
                    };
                    collect_children(doc, style, images, svgs, node, &mut host);
                    parent.children.push(LayoutBox {
                        node: Some(node),
                        style: cv.clone(),
                        kind: BoxKind::InlineBlockHost(Box::new(host)),
                        children: Vec::new(),
                        frame: Frame::ZERO,
                        lines: Vec::new(),
                        list_marker: None,
                        colspan: 1, rowspan: 1, col_widths: Vec::new(),
                    });
                    return;
                }
            }
            if let Some(b) = build_pseudo_synth(style, node, true) {
                parent.children.push(b);
            }
            let mut child = doc.node(node).first_child;
            while let Some(c) = child {
                collect_inline_with_style(doc, style, images, svgs, c, parent, &cv);
                child = doc.node(c).next_sibling;
            }
            if let Some(a) = build_pseudo_synth(style, node, false) {
                parent.children.push(a);
            }
        }
        _ => {}
    }
}

/// Synthesize a `ControlEntry` for `<input>` and `<button>` — both render
/// as a single bordered rect with a label string inside (the input's
/// `value`, or the button's text content). Returns `None` for any other
/// element so the caller can fall through to normal inline handling.
fn control_entry(doc: &Document, node: NodeId) -> Option<ControlEntry> {
    let elem = doc.element(node)?;
    match elem.name.as_str() {
        "input" => {
            let ty = elem
                .get_attr("type")
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_else(|| "text".to_string());
            // Skip non-visual inputs (hidden, file, image, ...). Anything
            // we don't render visually we leave out of layout entirely.
            let kind = match ty.as_str() {
                "submit" | "button" | "reset" => ControlKind::Button,
                "checkbox" => ControlKind::Checkbox {
                    checked: elem.get_attr("checked").is_some(),
                },
                "radio" => ControlKind::Radio {
                    checked: elem.get_attr("checked").is_some(),
                },
                "hidden" | "file" | "image" => return None,
                _ => ControlKind::Input,
            };
            // Default labels mirror the visible browser defaults so a bare
            // `<input type=submit>` still says "Submit".
            let default_label = match ty.as_str() {
                "submit" => "Submit",
                "reset" => "Reset",
                _ => "",
            };
            let value = elem.get_attr("value").map(|s| s.to_string());
            let placeholder = if matches!(kind, ControlKind::Input) {
                elem.get_attr("placeholder").map(|s| s.to_string())
            } else {
                None
            };
            let (label, is_placeholder) = match (value.as_deref(), placeholder) {
                (Some(v), _) if !v.is_empty() => (v.to_string(), false),
                (_, Some(p)) => (p, true),
                _ => (default_label.to_string(), false),
            };
            Some(ControlEntry { label, kind, is_placeholder })
        }
        "button" => {
            let mut buf = String::new();
            collect_text(doc, node, &mut buf);
            let label = buf.trim().to_string();
            Some(ControlEntry {
                label,
                kind: ControlKind::Button,
                is_placeholder: false,
            })
        }
        "select" => {
            // Find the selected <option> (or fall back to the first
            // one). Render the select as a button-shaped control
            // displaying that option's text. We don't open a real
            // dropdown — a static rendering of the current selection
            // is what most static-page snapshots expect anyway.
            let mut selected: Option<String> = None;
            let mut first: Option<String> = None;
            walk_options(doc, node, &mut |elem, n| {
                if elem.name != "option" {
                    return;
                }
                let mut buf = String::new();
                collect_text(doc, n, &mut buf);
                let label = buf.trim().to_string();
                if first.is_none() && !label.is_empty() {
                    first = Some(label.clone());
                }
                if elem.get_attr("selected").is_some() {
                    selected = Some(label);
                }
            });
            let label = selected.or(first).unwrap_or_default();
            Some(ControlEntry {
                label,
                kind: ControlKind::Button,
                is_placeholder: false,
            })
        }
        "textarea" => {
            // Treat <textarea>'s text content as the label of an
            // input-shaped control. We prefer the `value` attribute
            // when set (the browser's main loop stamps the user's
            // typed buffer into `value` before each re-layout, so
            // typed text shows up live). Fall back to child text
            // content for textareas that pre-fill via DOM children,
            // then to the `placeholder` attribute. No multi-line
            // shaping yet.
            let typed = elem
                .get_attr("value")
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    let mut buf = String::new();
                    collect_text(doc, node, &mut buf);
                    let t = buf.trim().to_string();
                    if t.is_empty() { None } else { Some(t) }
                });
            let placeholder = elem.get_attr("placeholder").map(|s| s.to_string());
            let (label, is_placeholder) = match (typed, placeholder) {
                (Some(t), _) => (t, false),
                (None, Some(p)) => (p, true),
                (None, None) => (String::new(), false),
            };
            Some(ControlEntry {
                label,
                kind: ControlKind::Input,
                is_placeholder,
            })
        }
        _ => None,
    }
}

/// Visit every descendant element of `parent`, calling `cb` for each.
/// Used by `<select>` to walk its `<option>` children — we'd rather
/// not duplicate the descendants iterator here.
fn walk_options<F>(doc: &Document, parent: NodeId, cb: &mut F)
where
    F: FnMut(&bui_dom::Element, NodeId),
{
    let mut child = doc.node(parent).first_child;
    while let Some(c) = child {
        if let Some(elem) = doc.element(c) {
            cb(elem, c);
            walk_options(doc, c, cb);
        }
        child = doc.node(c).next_sibling;
    }
}

fn collect_text(doc: &Document, node: NodeId, out: &mut String) {
    let mut child = doc.node(node).first_child;
    while let Some(c) = child {
        match &doc.node(c).kind {
            NodeKind::Text(t) => out.push_str(t),
            NodeKind::Element(_) => collect_text(doc, c, out),
            _ => {}
        }
        child = doc.node(c).next_sibling;
    }
}

/// CSS Normal whitespace collapse for a single text node. A run of
/// whitespace becomes a single space; empty input stays empty. We
/// keep the leading/trailing space (if any) — line-edge stripping
/// happens in `layout_inline`, where we have line context. Eating
/// edge whitespace here would lose the space that should sit between
/// `<a>X</a> <a>Y</a>` where the " " is its own text node.
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_was_ws {
                out.push(' ');
                last_was_ws = true;
            }
        } else {
            out.push(c);
            last_was_ws = false;
        }
    }
    out
}

/// Lay out the box tree at `(x, y)` within an `available_width` viewport.
/// Runs the normal-flow pass first, then a fixup pass that resolves
/// `position: relative` / `absolute` / `fixed` against the appropriate
/// containing block.
pub fn layout(root: &mut LayoutBox, x: f32, y: f32, available_width: f32) {
    layout_block(root, x, y, available_width, None);
    let cb = root.frame;
    apply_positioning(root, cb, cb);
}

/// Resolve `position: relative` / `absolute` / `fixed` after normal flow.
///
/// `abs_cb` is the containing block for `absolute` descendants — the
/// nearest positioned ancestor's frame, or the root frame if none. `fix_cb`
/// is the containing block for `fixed` descendants — we use the root
/// frame as a viewport stand-in since `layout` doesn't know the real
/// viewport height.
///
/// We recurse into children FIRST, then apply this box's own shift, so
/// when a `position: relative` parent shifts, descendants ride along
/// with it via `shift_subtree`.
fn apply_positioning(bx: &mut LayoutBox, abs_cb: Frame, fix_cb: Frame) {
    let pos = bx.style.position;
    // For descendants: if this box itself is positioned, it becomes the
    // containing block for absolute descendants.
    let child_abs_cb = if matches!(
        pos,
        bui_style::Position::Relative
            | bui_style::Position::Absolute
            | bui_style::Position::Fixed
            | bui_style::Position::Sticky,
    ) {
        bx.frame
    } else {
        abs_cb
    };
    for child in &mut bx.children {
        apply_positioning(child, child_abs_cb, fix_cb);
    }

    match pos {
        bui_style::Position::Static => {}
        bui_style::Position::Relative => {
            let (dx, dy) = compute_relative_shift(&bx.style, abs_cb);
            shift_subtree(bx, dx, dy);
        }
        bui_style::Position::Sticky => {
            // Sticky boxes flow at their natural position during
            // layout; the paint pass emits PushStickyGroup /
            // PopStickyGroup, and the scroll-shift loop in the host
            // applies the per-group clamp. No shift here.
        }
        bui_style::Position::Absolute | bui_style::Position::Fixed => {
            let cb = if matches!(pos, bui_style::Position::Fixed) {
                fix_cb
            } else {
                abs_cb
            };
            let (new_x, new_y) = resolve_absolute_origin(bx, cb);
            let dx = new_x - bx.frame.x;
            let dy = new_y - bx.frame.y;
            shift_subtree(bx, dx, dy);
        }
    }

    // CSS `transform: translate(...)` is purely visual — applied
    // *after* box-model layout. We piggy-back on shift_subtree so the
    // whole subtree moves with us (the box's children inherit the
    // translation, just like under `position: relative`).
    if let Some((tx, ty)) = bx.style.transform_translate {
        let dx = resolve_length(tx, bx.style.font_size, bx.frame.width);
        let dy = resolve_length(ty, bx.style.font_size, bx.frame.height.max(1.0));
        shift_subtree(bx, dx, dy);
    }
}

/// `position: relative` shifts by `(left, top)`, falling back to the
/// negation of `(right, bottom)` if those are the only specified edges.
/// CSS resolves left/right percentages against the containing block's
/// width, top/bottom against its height.
fn compute_relative_shift(style: &ComputedValues, cb: Frame) -> (f32, f32) {
    let dx = style
        .left
        .map(|l| resolve_length(l, style.font_size, cb.width))
        .or_else(|| {
            style
                .right
                .map(|r| -resolve_length(r, style.font_size, cb.width))
        })
        .unwrap_or(0.0);
    let dy = style
        .top
        .map(|t| resolve_length(t, style.font_size, cb.height.max(1.0)))
        .or_else(|| {
            style
                .bottom
                .map(|b| -resolve_length(b, style.font_size, cb.height.max(1.0)))
        })
        .unwrap_or(0.0);
    (dx, dy)
}

/// Compute `(x, y)` for an absolutely / fixed positioned box from its
/// containing block. Honours `left`/`right` (and `top`/`bottom`) with
/// `left` (and `top`) winning when both are present, matching how every
/// real browser handles the conflict.
fn resolve_absolute_origin(bx: &LayoutBox, cb: Frame) -> (f32, f32) {
    let s = &bx.style;
    let cb_h = cb.height.max(1.0);
    let new_x = if let Some(l) = s.left {
        cb.x + resolve_length(l, s.font_size, cb.width)
    } else if let Some(r) = s.right {
        cb.x + cb.width - bx.frame.width - resolve_length(r, s.font_size, cb.width)
    } else {
        bx.frame.x
    };
    let new_y = if let Some(t) = s.top {
        cb.y + resolve_length(t, s.font_size, cb_h)
    } else if let Some(b) = s.bottom {
        cb.y + cb.height - bx.frame.height - resolve_length(b, s.font_size, cb_h)
    } else {
        bx.frame.y
    };
    (new_x, new_y)
}

/// Translate every coordinate in `bx` and its descendants by `(dx, dy)`.
/// Used when a positioned box's final origin differs from the location
/// it was laid out at; everything below it (block frames, line boxes,
/// text runs, images, controls) rides along.
/// Approximate the maximum-content width a box would need if its
/// inline runs were laid out without wrapping. Walks every descendant
/// LineBox, sums each line's item widths (so multi-run lines are
/// counted as a single unwrapped line), and returns the max across
/// all lines plus the box's own padding / border. Used by float
/// shrink-to-fit so a `<li>Article</li>` doesn't claim 160 px when
/// its content is ~40 px wide.
fn max_content_width(bx: &LayoutBox) -> f32 {
    let mut best = 0.0_f32;
    walk_max_content(bx, &mut best);
    let p = resolve_edges(&bx.style.padding, bx.style.font_size, 0.0);
    let b = resolve_edges(&bx.style.border, bx.style.font_size, 0.0);
    best + p.left + p.right + b.left + b.right
}

/// Estimate a box's max-content width by measuring InlineText leaves
/// directly with the shared font. Unlike `max_content_width`, this
/// works BEFORE layout — useful for sizing flex items whose lines
/// haven't been built yet. Walks the box tree summing text widths
/// per inline run; returns the max sum across runs plus this box's
/// own padding / border.
fn estimate_max_content_width(bx: &LayoutBox) -> f32 {
    let font = bui_text::shared_font();
    let mut best = 0.0_f32;
    estimate_walk(bx, font, &mut best);
    let p = resolve_edges(&bx.style.padding, bx.style.font_size, 0.0);
    let b = resolve_edges(&bx.style.border, bx.style.font_size, 0.0);
    best + p.left + p.right + b.left + b.right
}

fn estimate_walk(bx: &LayoutBox, font: &bui_text::Font, best: &mut f32) {
    // Sum direct InlineText / Image / Svg / Control leaf widths so a
    // run of inline siblings (e.g., "Über Google" inside an <a>) gets
    // counted as one unwrapped line.
    let mut run_w = 0.0_f32;
    for child in &bx.children {
        match &child.kind {
            BoxKind::InlineText(s) => {
                let w = font.measure_text_with_spacing(s, child.style.font_size, child.style.letter_spacing);
                run_w += w;
            }
            BoxKind::InlineImage(e) => {
                let w = match child.style.width {
                    Dimension::Length(l) => l.resolve(child.style.font_size, 16.0, 0.0),
                    _ => e.width,
                };
                run_w += w;
            }
            BoxKind::InlineSvg(e) => {
                let w = match child.style.width {
                    Dimension::Length(l) => l.resolve(child.style.font_size, 16.0, 0.0),
                    _ => e.width,
                };
                run_w += w;
            }
            BoxKind::InlineControl(e) => {
                let w = font.measure_text(&e.label, child.style.font_size).max(child.style.font_size);
                run_w += w;
            }
            BoxKind::InlineBlockHost(host) => {
                run_w += estimate_max_content_width(host);
            }
            BoxKind::Anonymous | BoxKind::Block => {
                // Floats sit alongside the inline run, not below it —
                // their intrinsic width contributes to the run total
                // instead of starting a new run. Without this branch
                // Wikipedia's `a.mw-logo` (flex item whose only child
                // is a `float: left` span containing two 140-px imgs)
                // estimated to 0 and the flex row painted the logo
                // on top of the search box.
                let floated = matches!(
                    child.style.float,
                    bui_style::Float::Left | bui_style::Float::Right
                );
                if floated {
                    run_w += intrinsic_max_width(child);
                    continue;
                }
                // Block-level subtree: recurse and treat its inner
                // result as a separate run.
                if run_w > *best {
                    *best = run_w;
                }
                run_w = 0.0;
                estimate_walk(child, font, best);
            }
            BoxKind::InlineBreak => {
                if run_w > *best {
                    *best = run_w;
                }
                run_w = 0.0;
            }
        }
    }
    if run_w > *best {
        *best = run_w;
    }
}

fn walk_max_content(bx: &LayoutBox, best: &mut f32) {
    for line in &bx.lines {
        let mut sum = 0.0_f32;
        for item in &line.items {
            sum += match item {
                LineItem::Text(r) => r.frame.width,
                LineItem::Image { frame, .. } => frame.width,
                LineItem::Control { frame, .. } => frame.width,
                LineItem::Svg { frame, .. } => frame.width,
                LineItem::InlineBlock { frame, .. } => frame.width,
            };
        }
        if sum > *best {
            *best = sum;
        }
    }
    for child in &bx.children {
        // For non-anonymous block children, include their own frame
        // width — they might be inline-block-like or floated.
        if !matches!(child.kind, BoxKind::Anonymous) {
            if child.frame.width > *best {
                *best = child.frame.width;
            }
        }
        walk_max_content(child, best);
    }
}

fn shift_subtree(bx: &mut LayoutBox, dx: f32, dy: f32) {
    if dx == 0.0 && dy == 0.0 {
        return;
    }
    bx.frame.x += dx;
    bx.frame.y += dy;
    for line in &mut bx.lines {
        line.frame.x += dx;
        line.frame.y += dy;
        for item in &mut line.items {
            match item {
                LineItem::Text(run) => {
                    run.frame.x += dx;
                    run.frame.y += dy;
                }
                LineItem::Image { frame, .. } => {
                    frame.x += dx;
                    frame.y += dy;
                }
                LineItem::Control { frame, .. } => {
                    frame.x += dx;
                    frame.y += dy;
                }
                LineItem::Svg { frame, .. } => {
                    frame.x += dx;
                    frame.y += dy;
                }
                LineItem::InlineBlock { frame, host, .. } => {
                    frame.x += dx;
                    frame.y += dy;
                    shift_subtree(host, dx, dy);
                }
            }
        }
    }
    for child in &mut bx.children {
        shift_subtree(child, dx, dy);
    }
}

/// CSS Flexbox (subset of L1).
///
/// What's implemented:
///   * `flex-direction: row` and `column` (row-reverse / column-reverse via
///     reordering the items list).
///   * `flex-wrap: wrap` (and wrap-reverse) — items group into rows when
///     their bases exceed the main-axis size; rows stack on the cross
///     axis with `row-gap` between them. Each row distributes its own
///     free space via grow / shrink.
///   * Per-item `flex-grow`, `flex-shrink`, `flex-basis` (auto = 0
///     contribution; length supported). `flex` shorthand parses upstream.
///   * `justify-content`: flex-start / flex-end / center / space-between /
///     space-around / space-evenly.
///   * `align-items`: stretch (default) / flex-start / flex-end / center.
///     Baseline falls back to flex-start.
///
/// What's deferred:
///   * Min/max-width/height clamping during shrink resolution.
///   * Aspect-ratio + intrinsic-size content measurement. We treat
///     `flex-basis: auto` items with `flex-grow > 0` as basis = 0 (they
///     get all their size from the grow distribution); items with
///     `flex-grow == 0` and basis = auto retain a small content width
///     determined by the explicit width style or 0.
///   * `align-content` for distributing extra cross-axis space across
///     wrapped rows. We stack rows tight with row_gap between them.
fn layout_flex(bx: &mut LayoutBox, x: f32, y: f32, container_w: f32, container_h: Option<f32>) {
    let cv = bx.style.clone();
    let m = resolve_edges(&cv.margin, cv.font_size, container_w);
    let p = resolve_edges(&cv.padding, cv.font_size, container_w);
    let b = resolve_edges(&cv.border, cv.font_size, container_w);

    let content_w =
        (container_w - m.left - m.right - p.left - p.right - b.left - b.right).max(0.0);
    let outer_x = x;
    let outer_y = y;
    let content_x = outer_x + m.left + b.left + p.left;
    let content_y = outer_y + m.top + b.top + p.top;

    let direction = cv.flex_direction;
    let row = matches!(direction, FlexDirection::Row | FlexDirection::RowReverse);
    let reversed = matches!(
        direction,
        FlexDirection::RowReverse | FlexDirection::ColumnReverse
    );

    // Prepare children: every in-flow direct child becomes a flex item.
    // Inline text / image children get wrapped in an anonymous box so they
    // can be measured + sized like blocks. Whitespace-only anonymous
    // blocks are skipped — they represent source whitespace between
    // block siblings and shouldn't take up a full flex item slot
    // (real browsers do this filter via "blockification").
    //
    // Out-of-flow children (`position: absolute` / `fixed`) are NOT
    // flex items — per CSS Flexbox §4 they're skipped during
    // size resolution. We still need them in the tree so
    // apply_positioning can shift them later, so they get laid out
    // separately at the container origin and stitched in alongside
    // the flex items at the end.
    let mut children = std::mem::take(&mut bx.children);
    let mut items: Vec<LayoutBox> = Vec::with_capacity(children.len());
    let mut out_of_flow: Vec<LayoutBox> = Vec::new();
    for child in children.drain(..) {
        let is_out_of_flow = matches!(
            child.style.position,
            bui_style::Position::Absolute | bui_style::Position::Fixed
        );
        if is_out_of_flow {
            out_of_flow.push(child);
            continue;
        }
        match child.kind {
            BoxKind::Block => items.push(child),
            BoxKind::Anonymous => {
                if anon_is_whitespace_only(&child) {
                    continue;
                }
                items.push(child);
            }
            BoxKind::InlineText(_)
            | BoxKind::InlineImage(_)
            | BoxKind::InlineControl(_)
            | BoxKind::InlineSvg(_)
            | BoxKind::InlineBreak
            | BoxKind::InlineBlockHost(_) => {
                // Wrap inline content in an anonymous BLOCK (not a flex
                // container). We can't clone the flex container's style
                // verbatim — its `display: flex` would re-enter
                // `layout_flex` on this wrapper, which would wrap its
                // single inline child in *another* flex anon, which
                // would wrap again, until the stack overflows.
                let mut wrapper_style = cv.clone();
                wrapper_style.display = Display::Block;
                items.push(LayoutBox {
                    node: None,
                    style: wrapper_style,
                    kind: BoxKind::Anonymous,
                    children: vec![child],
                    frame: Frame::ZERO,
                    lines: Vec::new(), list_marker: None, colspan: 1, rowspan: 1, col_widths: Vec::new(),
                });
            }
        }
    }
    // CSS Flexbox §5.4 — order-modified document order: items lay out
    // sorted by their `order` (ascending, stable so equal orders keep
    // document order). DuckDuckGo's CTA cards put `order:-1` on the
    // last card to swap it ahead of the first. Sort BEFORE the
    // reverse so row-reverse then mirrors the order-modified sequence.
    if items.iter().any(|it| it.style.order != 0) {
        items.sort_by_key(|it| it.style.order);
    }
    if reversed {
        items.reverse();
    }

    // Main-axis size of the container.
    let mut main_size = if row { content_w } else { content_height_hint(&cv, container_w, container_h) };
    // Apply max-height clamp upfront so we can hand a tight basis
    // to column-flex children. Without this, Google's
    // `.kKvsb { max-height: 230px }` parent kept main_size at 0 (its
    // height: auto resolved to 0 in content_height_hint), then grew
    // to 1304 (content_w) inside the child's
    // `height: calc(100% - 200px)` resolution because the child saw
    // container_w as its percent basis. Clamping main_size by
    // max-height here gives the child a 230 basis to subtract 200
    // from, yielding the spec-correct 30 px spacer.
    let mut main_size_was_explicit = row || main_size > 0.0;
    if !row {
        if let Some(mh) = cv.max_height {
            let basis = container_h.unwrap_or_else(|| {
                let (_, vh) = bui_style::viewport();
                vh
            });
            let lim = mh.resolve(cv.font_size, 16.0, basis);
            if lim > 0.0 && (main_size <= 0.0 || lim < main_size) {
                main_size = lim;
                main_size_was_explicit = true;
            }
        }
    }
    // Track whether main_size is fixed (must not grow from row_extent
    // even if children overflow — CSS lets content overflow the
    // flex container's main axis). For row flex, the container's
    // width is always fixed by the parent's content_w, so its main
    // axis never grows. For column flex, main_size grows only when
    // there was no CSS hint (height auto). Without this guard, a
    // row-flex container whose children overflowed (Google's
    // RNNXgb at 1352 inside its 688-max parent) would expand its
    // own main_size to 1352, blowing out the page width.
    //
    // (main_size_was_explicit was set above when applying the
    // upfront max-height clamp; declared mutable so the clamp can
    // flip it to true.)

    // Step 1: hypothetical main size per item (the "flex base size"
    // before grow/shrink). For row, that's the item's computed width or
    // flex-basis or 0 if both are auto and grow > 0.
    let n = items.len();
    let mut bases: Vec<f32> = Vec::with_capacity(n);
    let mut grows: Vec<f32> = Vec::with_capacity(n);
    let mut shrinks: Vec<f32> = Vec::with_capacity(n);
    // Per-item main-axis auto margins. CSS Flexbox §8.1: positive
    // free space on the main axis is consumed by auto margins
    // BEFORE justify-content runs. We collect (start_auto, end_auto)
    // tuples so the per-row loop can distribute leftover among them.
    let mut auto_main_start: Vec<bool> = Vec::with_capacity(n);
    let mut auto_main_end: Vec<bool> = Vec::with_capacity(n);
    for item in &items {
        if row {
            auto_main_start.push(item.style.margin_left_auto);
            auto_main_end.push(item.style.margin_right_auto);
        } else {
            auto_main_start.push(item.style.margin_top_auto);
            auto_main_end.push(item.style.margin_bottom_auto);
        }
    }
    for item in &items {
        let style = &item.style;
        let mut basis = match style.flex_basis {
            FlexBasis::Length(l) => l.resolve(style.font_size, 16.0, main_size),
            FlexBasis::Auto => match (row, style.width, style.height) {
                (true, Dimension::Length(l), _) => l.resolve(style.font_size, 16.0, main_size),
                (false, _, Dimension::Length(l)) => l.resolve(style.font_size, 16.0, main_size),
                _ => 0.0,
            },
        };
        // CSS Flexbox §7.1: an item's flex base size is floored by
        // its `min-width` (or `min-height` for column flex). Without
        // this, an item with `min-width: 256px; flex-basis: auto`
        // and no width gets basis=0, then flex-shrink can crush it
        // below its min. Wikipedia's `.cdx-text-input { min-width:
        // 256px }` inside the search form needs this so the input
        // wrapper claims at least 256 px before grow / shrink runs.
        if row {
            if let Some(mn) = style.min_width {
                let raw = mn.resolve(style.font_size, 16.0, main_size).max(0.0);
                if raw > basis {
                    basis = raw;
                }
            }
        } else if let Some(mn) = style.min_height {
            let raw = mn.resolve(style.font_size, 16.0, main_size).max(0.0);
            if raw > basis {
                basis = raw;
            }
        }
        bases.push(basis.max(0.0));
        grows.push(style.flex_grow);
        shrinks.push(style.flex_shrink);
    }
    let main_axis_gap = if row { cv.column_gap } else { cv.row_gap };
    let cross_axis_gap = if row { cv.row_gap } else { cv.column_gap };

    // Step 2: split items into rows. With wrap off, everything goes
    // on a single row; with wrap on, we walk items accumulating
    // bases + main-axis gap until the next item would push past
    // main_size, then start a new row. Single-item rows are allowed
    // even when the item itself is wider than main_size — the row
    // just overflows visibly, matching browser behaviour.
    let wrap_enabled = matches!(cv.flex_wrap, bui_style::FlexWrap::Wrap | bui_style::FlexWrap::WrapReverse);
    let mut row_starts: Vec<usize> = vec![0];
    if wrap_enabled {
        let mut acc = 0.0f32;
        for i in 0..n {
            let row_start = *row_starts.last().unwrap();
            let prefix_gap = if i > row_start { main_axis_gap } else { 0.0 };
            if i > row_start && acc + prefix_gap + bases[i] > main_size + 0.5 {
                row_starts.push(i);
                acc = bases[i];
            } else {
                acc += prefix_gap + bases[i];
            }
        }
    }
    row_starts.push(n);

    // Step 3 + 4: per-row free-space distribution and main-axis
    // layout. Each row owns a slice of `items` that we consume into
    // `bx.children` in source order (so post-loop iteration matches).
    let cross_origin = if row { content_y } else { content_x };
    let mut cross_cursor = cross_origin;
    let mut row_max_cross: Vec<f32> = Vec::new();
    let mut row_n: Vec<usize> = Vec::new();
    // Precompute max-content widths for each item before consuming the
    // items vec into items_iter — needed by the mixed-grow row sizing.
    // estimate_max_content_width walks raw inline content (text /
    // images / svg / inline-block-host) so it works BEFORE any layout
    // pass has populated `lines`.
    let item_max_content: Vec<f32> = items.iter().map(|it| estimate_max_content_width(it)).collect();
    let mut items_iter = items.into_iter();
    for w in row_starts.windows(2) {
        let start = w[0];
        let end = w[1];
        let m = end - start;
        if m == 0 {
            continue;
        }
        let row_bases: Vec<f32> = bases[start..end].to_vec();
        let row_grows: Vec<f32> = grows[start..end].to_vec();
        let row_shrinks: Vec<f32> = shrinks[start..end].to_vec();
        let total_basis: f32 = row_bases.iter().sum();
        let free = main_size - total_basis;
        let mut sizes: Vec<f32> = row_bases.clone();
        if free > 0.0 {
            // Items with basis:0 and grow:0 would otherwise collapse
            // to zero — CSS would resolve them via max-content but we
            // don't have intrinsic sizing. The fair-share fallback
            // helps row flex (Wikipedia's left/right toolbars), but
            // hurts column flex when many empty/auxiliary divs are
            // siblings of a few visible ones (Google's L3eUgb has
            // half a dozen empty <div>s that we'd over-allocate to
            // 200 px each, pushing real content way down). Apply
            // fair-share only to ROW flex; for column flex, leave
            // basis:0/grow:0 items at zero height so they shrink to
            // their content (which is already 0 for the empty divs).
            if row {
                // CSS behavior: items with auto basis and no grow get
                // their max-content size. We approximate that with
                // estimate_max_content_width — walks raw inline text /
                // image / svg leaves before layout populates `lines`.
                // The estimate undercounts (missing glyph kerning,
                // letter-spacing rounding, inter-anonymous gaps), so
                // we add a 20% margin so content doesn't wrap when it
                // would fit at full CSS-spec sizing.
                //
                // Items whose estimate is 0 (no measurable content —
                // empty wrappers, icon-only buttons whose icon is
                // CSS-sized later) fall back to a fair share of the
                // main axis so they aren't crushed to nothing
                // (Wikipedia's left/right toolbar buttons).
                let total_est: f32 = item_max_content[start..end].iter().sum();
                let measurable: usize = (start..end)
                    .filter(|&i| item_max_content[i] > 0.0)
                    .count();
                let unmeasured: usize = m - measurable;
                let fair_share = if unmeasured > 0 {
                    let est_used = total_est * 1.2 + 8.0 * measurable as f32;
                    let leftover = (main_size - est_used).max(0.0);
                    leftover / unmeasured as f32
                } else {
                    0.0
                };
                for i in 0..m {
                    if row_bases[i] < 0.5 && row_grows[i] == 0.0 {
                        let est = item_max_content[start + i];
                        if est > 0.0 {
                            sizes[i] = est * 1.2 + 8.0;
                        } else if fair_share > 0.0 {
                            sizes[i] = fair_share;
                        }
                    }
                }
            }
            let total_grow: f32 = row_grows.iter().sum();
            if total_grow > 0.0 {
                let used: f32 = sizes.iter().sum();
                let remaining = (main_size - used).max(0.0);
                for i in 0..m {
                    sizes[i] += remaining * (row_grows[i] / total_grow);
                }
            }
        } else if free < 0.0 {
            let total_weight: f32 = row_shrinks
                .iter()
                .zip(row_bases.iter())
                .map(|(s, b)| s * b)
                .sum();
            if total_weight > 0.0 {
                for i in 0..m {
                    let weight = row_shrinks[i] * row_bases[i];
                    let delta = free * (weight / total_weight);
                    sizes[i] = (sizes[i] + delta).max(0.0);
                }
            }
        }
        let total_fixed_gap = if m > 1 {
            main_axis_gap * (m as f32 - 1.0)
        } else {
            0.0
        };
        let used_main: f32 = sizes.iter().sum::<f32>() + total_fixed_gap;
        let leftover = (main_size - used_main).max(0.0);
        // Auto margins consume positive main-axis free space BEFORE
        // justify-content has a say. Count auto margins in this row;
        // each gets `leftover / total_autos`. justify-content is
        // ignored when any auto margin is present (CSS Flexbox §8.1).
        let auto_starts_in_row: Vec<bool> = auto_main_start[start..end].to_vec();
        let auto_ends_in_row: Vec<bool> = auto_main_end[start..end].to_vec();
        let total_autos: usize = auto_starts_in_row.iter().filter(|&&b| b).count()
            + auto_ends_in_row.iter().filter(|&&b| b).count();
        // Auto-margin distribution happens in two phases: we lay out
        // items first (so we can see actual rendered sizes — a flex
        // item with `height: 100%` has basis = main_size leaving
        // zero pre-layout leftover, but its actual frame.height
        // after layout may be smaller, and that real leftover is
        // what auto margins should absorb). After the per-item loop
        // we walk back through the placed children, compute the
        // real leftover from min/max actual bounds, and shift each
        // item by its share of the leftover. (CSS Flexbox §8.1.)
        let _ = leftover;
        let (start_offset, justify_gap) = if total_autos > 0 {
            // justify-content is overridden — auto margins absorb
            // the leftover; we distribute it post-layout below.
            (0.0, 0.0)
        } else {
            match cv.justify_content {
                JustifyContent::FlexStart => (0.0, 0.0),
                JustifyContent::FlexEnd => (leftover, 0.0),
                JustifyContent::Center => (leftover * 0.5, 0.0),
                JustifyContent::SpaceBetween => {
                    if m > 1 {
                        (0.0, leftover / (m as f32 - 1.0))
                    } else {
                        (leftover * 0.5, 0.0)
                    }
                }
                JustifyContent::SpaceAround => {
                    let g = if m > 0 { leftover / m as f32 } else { 0.0 };
                    (g * 0.5, g)
                }
                JustifyContent::SpaceEvenly => {
                    let g = if m > 0 { leftover / (m as f32 + 1.0) } else { 0.0 };
                    (g, g)
                }
            }
        };
        let gap_between = main_axis_gap + justify_gap;
        let main_origin = if row { content_x } else { content_y };
        let mut cursor = main_origin + start_offset;
        let row_main_origin = cursor;
        let mut row_cross: f32 = 0.0;
        // Track the index where this row's children land in
        // bx.children so the post-loop auto-margin shift can find
        // them. row_first_idx is the index of the first child added
        // for this row.
        let row_first_idx = bx.children.len();
        for i in 0..m {
            let mut child = items_iter.next().unwrap();
            let main_pixel = sizes[i];
            // CSS Values 4 §6.6.1: a child's percent-on-height resolves
            // against the parent's content HEIGHT. The flex axis matters:
            //   * Column flex: the main axis IS height, so the
            //     (max-height-clamped) main_size is the right basis.
            //   * Row flex: the main axis is WIDTH — main_size is the
            //     container's width and must NOT be used as the height
            //     basis (doing so made `height:100%` resolve against the
            //     width, stretching DuckDuckGo's header into a square and
            //     shoving the hero off-screen). The height basis is the
            //     container's own height = `container_h`, which is `None`
            //     when our height is indefinite — and per spec
            //     `height:%` against an indefinite parent is just `auto`.
            let child_basis: Option<f32> = if row {
                container_h
            } else if main_size_was_explicit && main_size > 0.0 {
                Some(main_size)
            } else {
                container_h
            };
            if row {
                layout_block(&mut child, cursor, cross_cursor, main_pixel, child_basis);
            } else {
                layout_block(&mut child, cross_cursor, cursor, content_w, child_basis);
            }
            let child_cross = if row { child.frame.height } else { child.frame.width };
            row_cross = row_cross.max(child_cross);
            // For column flex, the child's actual height may exceed
            // our planned `sizes[i]` (it's an inline-block / table /
            // text-content child whose height comes from content,
            // not the flex assignment). Without this adjustment the
            // next sibling overlaps the current one. For column
            // flex, the actual rendered height is also typically
            // SMALLER than sizes[i] when the item has no
            // height-defining content — the difference is what auto
            // margins absorb in the post-pass below. We pick the
            // cursor-advance based on whether main_size came from CSS:
            //   - explicit main_size (e.g., L3eUgb's height: 100%):
            //     advance by sizes[i] exactly, even if the child's
            //     actual size exceeds it. CSS lets content overflow;
            //     the parent's allocated tracks must add up to the
            //     declared height so leftover-distribution / footer-
            //     anchoring math stays consistent.
            //   - auto main_size (no CSS height): advance by the
            //     larger of planned and actual so a sibling whose
            //     real height grew (top-nav with wrapping content)
            //     doesn't get overlapped.
            let _actual_main = if row { child.frame.width } else { child.frame.height };
            let advance = if main_size_was_explicit {
                main_pixel
            } else {
                main_pixel.max(_actual_main)
            };
            cursor += advance + gap_between;
            bx.children.push(child);
        }
        // Auto-margin shift: walk the just-placed row, compute the
        // real leftover from main_size minus the items' actual
        // bounding span, and shift each item by its share of the
        // leftover (cumulative, so each item's left/top margin
        // pushes everything after it). This is the CSS Flexbox §8.1
        // behaviour: auto margins absorb positive free space.
        if total_autos > 0 {
            let mut min_main = f32::INFINITY;
            let mut max_main = f32::NEG_INFINITY;
            for c in &bx.children[row_first_idx..] {
                let s = if row { c.frame.x } else { c.frame.y };
                let e = if row {
                    c.frame.x + c.frame.width
                } else {
                    c.frame.y + c.frame.height
                };
                if s < min_main { min_main = s; }
                if e > max_main { max_main = e; }
            }
            let actual_used = if min_main.is_finite() {
                max_main - min_main
            } else {
                0.0
            };
            let real_leftover = (main_size - actual_used).max(0.0);
            if real_leftover > 0.0 {
                let share = real_leftover / total_autos as f32;
                let mut accumulated = 0.0_f32;
                for (i, c) in bx.children[row_first_idx..].iter_mut().enumerate() {
                    if auto_starts_in_row[i] {
                        accumulated += share;
                    }
                    if accumulated > 0.0 {
                        if row {
                            shift_subtree(c, accumulated, 0.0);
                        } else {
                            shift_subtree(c, 0.0, accumulated);
                        }
                    }
                    if auto_ends_in_row[i] {
                        accumulated += share;
                    }
                }
            }
        }
        row_max_cross.push(row_cross);
        row_n.push(m);
        cross_cursor += row_cross + cross_axis_gap;
        // Track the actual main-axis extent used by this row's items
        // so the final frame_h (column flex) can grow to enclose
        // content even when no explicit main_size was set. Without
        // this, a column flex with `height: auto` collapsed to 0 +
        // padding even though its children were placed at increasing
        // y positions.
        let row_extent = (cursor - row_main_origin).max(0.0);
        if row_extent > main_size && !main_size_was_explicit {
            main_size = row_extent;
        }
    }

    // Step 5: align-items per row. Each row's items get re-shifted on
    // the cross axis based on that row's max_cross.
    let mut child_idx = 0usize;
    for (row_i, &row_count) in row_n.iter().enumerate() {
        let max_cross = row_max_cross[row_i];
        for _ in 0..row_count {
            let child = &mut bx.children[child_idx];
            let child_cross = if row { child.frame.height } else { child.frame.width };
            let cross_offset = match cv.align_items {
                AlignItems::FlexStart | AlignItems::Baseline => 0.0,
                AlignItems::FlexEnd => max_cross - child_cross,
                AlignItems::Center => (max_cross - child_cross) * 0.5,
                AlignItems::Stretch => 0.0,
            };
            if cross_offset.abs() > 0.0 {
                // Shift the WHOLE subtree, not just the item's frame —
                // descendants need to follow or they stay at their
                // original (now-wrong) position. Without this, an
                // align-items: center on a flex container leaves its
                // items' inline content above the item's new top.
                if row {
                    shift_subtree(child, 0.0, cross_offset);
                } else {
                    shift_subtree(child, cross_offset, 0.0);
                }
            }
            if matches!(cv.align_items, AlignItems::Stretch) {
                if row && child_cross < max_cross {
                    child.frame.height = max_cross;
                } else if !row && child_cross < max_cross {
                    child.frame.width = max_cross;
                }
            }
            child_idx += 1;
        }
    }

    let total_cross: f32 = row_max_cross.iter().sum::<f32>()
        + if row_max_cross.len() > 1 {
            cross_axis_gap * (row_max_cross.len() as f32 - 1.0)
        } else {
            0.0
        };
    let mut frame_w = if row {
        main_size + p.left + p.right + b.left + b.right
    } else {
        total_cross + p.left + p.right + b.left + b.right
    };
    let mut frame_h = if row {
        total_cross + p.top + p.bottom + b.top + b.bottom
    } else {
        main_size + p.top + p.bottom + b.top + b.bottom
    };
    // CSS min-height / min-width clamp the flex container's frame.
    // Google's `.RNNXgb { min-height: 50px }` was producing a 40px
    // frame because the inline children stretched to 38px and the
    // 1px border padded out to 40 — without the min-height, the
    // search bar's rounded pill looked squat. Apply mins post-flex
    // so the cross-axis floor is honoured even when items stretch.
    if let Some(mh) = cv.min_height {
        let v = mh.resolve(cv.font_size, 16.0, container_w);
        if v > frame_h {
            frame_h = v;
        }
    }
    if let Some(mw) = cv.min_width {
        let v = mw.resolve(cv.font_size, 16.0, container_w);
        if v > frame_w {
            frame_w = v;
        }
    }
    bx.frame = Frame {
        x: outer_x + m.left,
        y: outer_y + m.top,
        width: frame_w,
        height: frame_h,
    };

    // Out-of-flow children: lay them out hypothetically at the
    // container's content origin so they have a frame; the post-pass
    // apply_positioning shifts them to their containing block's
    // top/left/right/bottom offsets. They're appended AFTER in-flow
    // items so paint order matches source order roughly.
    for mut child in out_of_flow.drain(..) {
        layout_block(&mut child, content_x, content_y, content_w, container_h);
        bx.children.push(child);
    }
}

/// Resolve the rendered size of a replaced inline element (`<img>` /
/// inline SVG) from its CSS width/height, intrinsic size, and the
/// containing block. CSS rules honored:
///   * px lengths resolve directly; percent WIDTH against `container_w`,
///     percent HEIGHT against `container_h` (the containing block's
///     content height) — NOT the width. When height is a percent and no
///     definite container height exists, it computes back to `auto`.
///   * `auto` on one axis preserves the intrinsic aspect ratio from the
///     other; `auto` on both uses the intrinsic size.
///   * `max-width` / `max-height` clamp the result (percent against the
///     matching container axis), preserving aspect ratio.
/// This is what keeps DuckDuckGo's header wordmark (`width:auto;
/// height:100%` against a 32px-tall `<a>`) from exploding to viewport
/// height when percent-height fell through to the width basis.
fn resolve_replaced_size(
    cv: &ComputedValues,
    intrinsic_w: f32,
    intrinsic_h: f32,
    container_w: f32,
    container_h: Option<f32>,
) -> (f32, f32) {
    let aspect = if intrinsic_h > 0.0 { intrinsic_w / intrinsic_h } else { 1.0 };
    // A percent height is only definite when the container height is.
    let height_definite = |l: &Length| -> Option<f32> {
        match l {
            Length::Percent(p) => container_h.map(|ch| ch * p / 100.0),
            other => Some(other.resolve(cv.font_size, 16.0, container_w)),
        }
    };
    let w_spec = match cv.width {
        Dimension::Length(l) => Some(l.resolve(cv.font_size, 16.0, container_w).max(0.0)),
        Dimension::Auto => None,
    };
    let h_spec = match cv.height {
        Dimension::Length(l) => height_definite(&l).map(|v| v.max(0.0)),
        Dimension::Auto => None,
    };
    let (mut w, mut h) = match (w_spec, h_spec) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => (w, if aspect > 0.0 { w / aspect } else { intrinsic_h }),
        (None, Some(h)) => (h * aspect, h),
        (None, None) => (intrinsic_w, intrinsic_h),
    };
    // max-width / max-height clamp, preserving aspect ratio.
    if let Some(mw) = cv.max_width {
        let lim = match mw {
            Length::Percent(p) => container_w * p / 100.0,
            other => other.resolve(cv.font_size, 16.0, container_w),
        };
        if lim > 0.0 && w > lim {
            h *= lim / w;
            w = lim;
        }
    }
    if let Some(mh) = cv.max_height {
        let lim = match mh {
            Length::Percent(p) => container_h.map(|ch| ch * p / 100.0),
            other => Some(other.resolve(cv.font_size, 16.0, container_w)),
        };
        if let Some(lim) = lim {
            if lim > 0.0 && h > lim {
                w *= lim / h;
                h = lim;
            }
        }
    }
    (w.max(1.0), h.max(1.0))
}

fn content_height_hint(cv: &ComputedValues, basis: f32, container_h: Option<f32>) -> f32 {
    // For ANY length on height — including calc(100% - Npx) — the
    // percent basis is the parent's resolved (and max-height-clamped)
    // height when known, falling back to viewport height otherwise.
    // Without this, Google's `.LLD4me { height: calc(100% - 560px) }`
    // evaluated 100% of 1400 (the width) instead of 900 (vh) and
    // produced an LS8OJ container 840px tall in a 900px viewport.
    // Browsers do the same: percent-on-height resolves to the
    // containing block's height, which for the html/body root is
    // the viewport height.
    let height_basis = container_h.unwrap_or_else(|| {
        let (_, vh) = bui_style::viewport();
        if vh > 0.0 { vh } else { basis }
    });
    // Plain `height: <percent>` against an indefinite parent (no definite
    // ancestor height plumbed in) computes back to `auto` per CSS Values 4
    // §6.6.1 — NOT the viewport. Resolving it to vh stretched
    // DuckDuckGo's `height:100%` header (inside auto-height ancestors)
    // into a full viewport-tall box. `calc(100% ± px)` keeps the vh
    // fallback (Google's `.LLD4me`), and the min-height floor below
    // still applies either way.
    let plain_percent = matches!(cv.height, Dimension::Length(bui_style::Length::Percent(_)));
    let h = match cv.height {
        Dimension::Length(_) if plain_percent && container_h.is_none() => 0.0,
        Dimension::Length(l) => l.resolve(cv.font_size, 16.0, height_basis),
        Dimension::Auto => 0.0,
    };
    // `min-height` provides a floor for the main-axis size of a
    // column flex container — `min-height: 100vh` is the canonical
    // "this container is at least one viewport tall" pattern that
    // Google's L3eUgb uses to anchor its footer to the bottom of
    // the screen and let the logo+search bar share the leftover.
    if let Some(mh) = cv.min_height {
        // vh / vw resolve against the viewport, not basis.
        let mn = mh.resolve(cv.font_size, 16.0, basis);
        if mn > h {
            return mn;
        }
    }
    h
}

/// CSS Grid Level 1 — the subset that real-world stylesheets actually
/// hit on the pages we care about (Vector-2022, GitHub, Stack
/// Overflow, etc.). Implemented:
///
///   * `display: grid` triggers grid layout.
///   * `grid-template-columns` / `grid-template-rows` with `<length>`,
///     `<percentage>`, `<fr>`, `auto`, `minmax()`, `repeat()` (no
///     `auto-fit` / `auto-fill` track-count yet — those repeat once).
///   * `gap` / `row-gap` / `column-gap`.
///   * `grid-column` / `grid-row` shorthand: integer line numbers and
///     `span <n>`. Auto on either side falls into the row-major
///     auto-placement walk.
///   * `grid-auto-rows` / `grid-auto-columns` for implicit tracks
///     past the explicit grid.
///
/// Track sizing is intentionally cheap: fixed tracks resolve up-front,
/// `fr` tracks split the residue, and `auto` tracks share whatever's
/// left when no `fr` is present. We never run a min-/max-content
/// measurement pass — items always lay out at their column rect's
/// width. That's enough for grids that are "scaffolding" (nav bars,
/// page shells); a content-sized grid (`grid-template-columns:
/// auto auto`) still works but the column widths will be the equal
/// share, not each column's intrinsic size.
fn layout_grid(bx: &mut LayoutBox, x: f32, y: f32, container_w: f32, container_h: Option<f32>) {
    let cv = bx.style.clone();
    let _ = container_h;
    let m = resolve_edges(&cv.margin, cv.font_size, container_w);
    let p = resolve_edges(&cv.padding, cv.font_size, container_w);
    let b = resolve_edges(&cv.border, cv.font_size, container_w);

    let content_w = match cv.width {
        Dimension::Auto => (container_w - m.left - m.right - p.left - p.right - b.left - b.right)
            .max(0.0),
        Dimension::Length(l) => {
            let raw = l.resolve(cv.font_size, 16.0, container_w);
            if matches!(cv.box_sizing, bui_style::BoxSizing::BorderBox) {
                (raw - p.left - p.right - b.left - b.right).max(0.0)
            } else {
                raw
            }
        }
    };
    let outer_x = x;
    let outer_y = y;
    let content_x = outer_x + m.left + b.left + p.left;
    let content_y = outer_y + m.top + b.top + p.top;

    // Wrap inline children in anonymous block boxes so each grid item
    // is something `layout_block` can lay out at a fixed width. Reset
    // the wrapper's display to Block — keeping `grid` would re-enter
    // layout_grid on a single-text-child wrapper and chase its tail.
    let mut children = std::mem::take(&mut bx.children);
    let mut items: Vec<LayoutBox> = Vec::with_capacity(children.len());
    for child in children.drain(..) {
        match child.kind {
            BoxKind::Block => items.push(child),
            BoxKind::Anonymous => {
                if anon_is_whitespace_only(&child) {
                    continue;
                }
                items.push(child);
            }
            BoxKind::InlineText(_)
            | BoxKind::InlineImage(_)
            | BoxKind::InlineControl(_)
            | BoxKind::InlineSvg(_)
            | BoxKind::InlineBreak
            | BoxKind::InlineBlockHost(_) => {
                let mut wrapper_style = cv.clone();
                wrapper_style.display = Display::Block;
                wrapper_style.grid_template_columns = Vec::new();
                wrapper_style.grid_template_rows = Vec::new();
                wrapper_style.grid_column_start = GridLine::Auto;
                wrapper_style.grid_column_end = GridLine::Auto;
                wrapper_style.grid_row_start = GridLine::Auto;
                wrapper_style.grid_row_end = GridLine::Auto;
                items.push(LayoutBox {
                    node: None,
                    style: wrapper_style,
                    kind: BoxKind::Anonymous,
                    children: vec![child],
                    frame: Frame::ZERO,
                    lines: Vec::new(),
                    list_marker: None,
                    colspan: 1, rowspan: 1, col_widths: Vec::new(),
                });
            }
        }
    }

    let n = items.len();
    let col_gap = cv.column_gap;
    let row_gap = cv.row_gap;
    let explicit_cols = cv.grid_template_columns.len();
    let cols = explicit_cols.max(1);

    // ---- 1. Auto-place each item ----
    struct Placed {
        row: usize,
        col: usize,
        col_span: usize,
        row_span: usize,
    }
    let mut placed: Vec<Placed> = Vec::with_capacity(n);
    let mut occupied: Vec<Vec<bool>> = vec![vec![false; cols]];
    let mut cursor_row = 0usize;
    let mut cursor_col = 0usize;

    for item in &items {
        let s = &item.style;

        // `grid-area: foo` shorthand sets all 4 sides to Named("foo").
        // Look it up against the parent's `grid-template-areas` to
        // recover an explicit (row, col, row_span, col_span). If no
        // such area exists, fall through to per-axis line resolution.
        let area_placement = grid_area_placement(s, &cv.grid_template_areas);

        let col_start_resolved = resolve_grid_line(
            &s.grid_column_start,
            &cv.grid_template_column_line_names,
        );
        let col_end_resolved = resolve_grid_line(
            &s.grid_column_end,
            &cv.grid_template_column_line_names,
        );
        let row_start_resolved = resolve_grid_line(
            &s.grid_row_start,
            &cv.grid_template_row_line_names,
        );
        let row_end_resolved = resolve_grid_line(
            &s.grid_row_end,
            &cv.grid_template_row_line_names,
        );
        let (col_span, row_span, explicit_col, explicit_row) = if let Some(a) = &area_placement {
            (a.col_span, a.row_span, Some(a.col), Some(a.row))
        } else {
            let col_span = grid_span(&col_start_resolved, &col_end_resolved).min(cols).max(1);
            let row_span = grid_span(&row_start_resolved, &row_end_resolved).max(1);
            let explicit_col = match &col_start_resolved {
                GridLine::Line(a) if *a >= 1 => Some((*a as usize - 1).min(cols.saturating_sub(1))),
                _ => None,
            };
            let explicit_row = match &row_start_resolved {
                GridLine::Line(a) if *a >= 1 => Some(*a as usize - 1),
                _ => None,
            };
            (col_span, row_span, explicit_col, explicit_row)
        };

        let (row, col) = if let (Some(r), Some(c)) = (explicit_row, explicit_col) {
            (r, c)
        } else {
            // Row-major auto-placement: walk forward from the cursor
            // until we find a span_r × span_c block with no occupied
            // cell. Explicit-on-one-axis-only goes through the same
            // walk but locks that axis.
            let (r, c) = find_free_slot(
                &mut occupied,
                cursor_row,
                cursor_col,
                cols,
                row_span,
                col_span,
                explicit_row,
                explicit_col,
            );
            (r, c)
        };

        // Ensure occupancy grid is tall enough.
        while occupied.len() < row + row_span {
            occupied.push(vec![false; cols]);
        }
        for dr in 0..row_span {
            for dc in 0..col_span {
                if col + dc < cols {
                    occupied[row + dr][col + dc] = true;
                }
            }
        }
        // Advance auto cursor past this placement.
        cursor_row = row;
        cursor_col = col + col_span;
        if cursor_col >= cols {
            cursor_col = 0;
            cursor_row += 1;
        }

        placed.push(Placed { row, col, col_span, row_span });
    }
    let row_count = occupied.len().max(1);

    // ---- 2. Resolve column widths ----
    let total_col_gap = if cols > 1 { col_gap * (cols - 1) as f32 } else { 0.0 };
    let track_avail_w = (content_w - total_col_gap).max(0.0);
    let col_tracks: Vec<TrackSize> = (0..cols)
        .map(|i| {
            cv.grid_template_columns
                .get(i)
                .copied()
                .unwrap_or(cv.grid_auto_columns)
        })
        .collect();
    let col_widths = resolve_track_axis(&col_tracks, track_avail_w, cv.font_size, content_w);

    // ---- 3. Each item's content-box width = sum(spanned cols + gaps) ----
    let mut item_widths: Vec<f32> = Vec::with_capacity(n);
    for pl in &placed {
        let mut w = 0.0;
        for c in pl.col..pl.col + pl.col_span {
            if c < cols {
                w += col_widths[c];
            }
        }
        if pl.col_span > 1 {
            w += col_gap * (pl.col_span - 1) as f32;
        }
        item_widths.push(w);
    }

    // ---- 4. Lay each item out once to capture its natural height ----
    let mut item_heights: Vec<f32> = Vec::with_capacity(n);
    for (i, item) in items.iter_mut().enumerate() {
        layout_block(item, 0.0, 0.0, item_widths[i], None);
        item_heights.push(item.frame.height);
    }

    // ---- 5. Resolve row heights ----
    let explicit_rows = cv.grid_template_rows.len();
    let row_tracks: Vec<TrackSize> = (0..row_count)
        .map(|i| {
            cv.grid_template_rows
                .get(i)
                .copied()
                .unwrap_or(cv.grid_auto_rows)
        })
        .collect();
    let mut row_heights = vec![0.0_f32; row_count];
    for (i, t) in row_tracks.iter().enumerate() {
        if let TrackSize::Length(l) = t {
            row_heights[i] = l.resolve(cv.font_size, 16.0, container_w).max(0.0);
        }
        if let TrackSize::MinMax(_, MinMaxSide::Length(l)) = t {
            row_heights[i] = l.resolve(cv.font_size, 16.0, container_w).max(0.0);
        }
    }
    // Auto/fr rows grow to fit items. Each item's height is split
    // among the auto rows it spans, after subtracting fixed rows.
    for (idx, pl) in placed.iter().enumerate() {
        let mut fixed = 0.0;
        let mut auto_rows: Vec<usize> = Vec::new();
        for r in pl.row..pl.row + pl.row_span {
            if r >= row_count {
                break;
            }
            match row_tracks[r] {
                TrackSize::Length(_) | TrackSize::MinMax(_, MinMaxSide::Length(_)) => {
                    fixed += row_heights[r]
                }
                _ => auto_rows.push(r),
            }
        }
        if pl.row_span > 1 {
            fixed += row_gap * (pl.row_span - 1) as f32;
        }
        let remain = (item_heights[idx] - fixed).max(0.0);
        if !auto_rows.is_empty() {
            let share = remain / auto_rows.len() as f32;
            for r in auto_rows {
                if row_heights[r] < share {
                    row_heights[r] = share;
                }
            }
        }
    }
    let _ = explicit_rows;

    // ---- 6. Compute track origins + final placement ----
    let mut col_x: Vec<f32> = Vec::with_capacity(cols + 1);
    {
        let mut cx = content_x;
        col_x.push(cx);
        for cw in &col_widths {
            cx += cw + col_gap;
            col_x.push(cx);
        }
    }
    let mut row_y: Vec<f32> = Vec::with_capacity(row_count + 1);
    {
        let mut ry = content_y;
        row_y.push(ry);
        for rh in &row_heights {
            ry += rh + row_gap;
            row_y.push(ry);
        }
    }

    for (i, item) in items.iter_mut().enumerate() {
        let pl = &placed[i];
        let cell_x = col_x[pl.col];
        let cell_y = row_y[pl.row];
        // Translate the already-laid-out subtree to its final cell
        // origin. We can't just re-run `layout_block` here: its
        // inline-flow path is destructive — `layout_inline` drains
        // the box's children into `bx.lines` on the first call, so
        // a second call would re-flow against an empty child list
        // and zero out anonymous-block heights (visible as h1 with
        // height=0 inside a grid). Since the hypothetical pass laid
        // each item out at (0, 0), a uniform shift by (cell_x, cell_y)
        // produces the same frames the second `layout_block` would
        // have, without revisiting the inline content.
        shift_subtree(item, cell_x, cell_y);
    }

    bx.children = items;

    // Container content height = bottom of last row (minus the
    // trailing gap we added in the running sum).
    let grid_h = if row_count == 0 {
        0.0
    } else {
        row_y[row_count] - content_y - row_gap
    };
    let final_content_h = match cv.height {
        Dimension::Length(l) => {
            let raw = l.resolve(cv.font_size, 16.0, container_w);
            if matches!(cv.box_sizing, bui_style::BoxSizing::BorderBox) {
                (raw - p.top - p.bottom - b.top - b.bottom).max(0.0)
            } else {
                raw
            }
        }
        Dimension::Auto => grid_h,
    };

    bx.frame = Frame {
        x: outer_x + m.left,
        y: outer_y + m.top,
        width: content_w + p.left + p.right + b.left + b.right,
        height: final_content_h + p.top + p.bottom + b.top + b.bottom,
    };
}

struct AreaPlacement {
    row: usize,
    col: usize,
    row_span: usize,
    col_span: usize,
}

/// If a child's grid-line declarations are the `grid-area: foo`
/// shorthand (all four sides set to `Named("foo")`), look the name up
/// in the parent grid's `grid-template-areas`. Returns the explicit
/// row/col origin and span; `None` otherwise. Only the "all four sides
/// match" case routes through here — anything else uses per-axis
/// line-name resolution.
fn grid_area_placement(
    cv: &ComputedValues,
    areas: &[Vec<String>],
) -> Option<AreaPlacement> {
    if areas.is_empty() {
        return None;
    }
    let name = match (&cv.grid_column_start, &cv.grid_column_end, &cv.grid_row_start, &cv.grid_row_end) {
        (GridLine::Named(a), GridLine::Named(b), GridLine::Named(c), GridLine::Named(d))
            if a == b && b == c && c == d =>
        {
            a.clone()
        }
        _ => return None,
    };
    let mut min_row = usize::MAX;
    let mut max_row = 0usize;
    let mut min_col = usize::MAX;
    let mut max_col = 0usize;
    for (r, row) in areas.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            if cell == &name {
                if r < min_row {
                    min_row = r;
                }
                if r > max_row {
                    max_row = r;
                }
                if c < min_col {
                    min_col = c;
                }
                if c > max_col {
                    max_col = c;
                }
            }
        }
    }
    if min_row == usize::MAX {
        return None;
    }
    Some(AreaPlacement {
        row: min_row,
        col: min_col,
        row_span: max_row - min_row + 1,
        col_span: max_col - min_col + 1,
    })
}

fn grid_span(start: &GridLine, end: &GridLine) -> usize {
    match (start, end) {
        (GridLine::Line(a), GridLine::Line(bb)) if bb > a => (bb - a) as usize,
        (GridLine::Span(n), _) | (_, GridLine::Span(n)) => *n as usize,
        _ => 1,
    }
}

/// Resolve a `GridLine::Named(...)` against the parent grid's
/// line-name table. Returns either an integer `Line(n)` (1-based) or
/// the original line variant when no resolution applies.
fn resolve_grid_line(line: &GridLine, line_names: &[Vec<String>]) -> GridLine {
    if let GridLine::Named(name) = line {
        if let Some(idx) = line_names
            .iter()
            .position(|names| names.iter().any(|n| n == name))
        {
            return GridLine::Line((idx + 1) as i32);
        }
    }
    line.clone()
}

/// Find a row-major free slot of `row_span × col_span` cells starting
/// no earlier than `(start_row, start_col)`. `lock_row` / `lock_col`
/// pin the search to a specific row or column when the author has
/// given an explicit `grid-row-start` / `grid-column-start`.
fn find_free_slot(
    occupied: &mut Vec<Vec<bool>>,
    start_row: usize,
    start_col: usize,
    cols: usize,
    row_span: usize,
    col_span: usize,
    lock_row: Option<usize>,
    lock_col: Option<usize>,
) -> (usize, usize) {
    let mut r = lock_row.unwrap_or(start_row);
    let mut c = lock_col.unwrap_or(start_col);
    loop {
        // Grow the occupancy grid to cover the candidate rows.
        while occupied.len() < r + row_span {
            occupied.push(vec![false; cols]);
        }
        if c + col_span <= cols {
            let mut all_free = true;
            'check: for dr in 0..row_span {
                for dc in 0..col_span {
                    if occupied[r + dr][c + dc] {
                        all_free = false;
                        break 'check;
                    }
                }
            }
            if all_free {
                return (r, c);
            }
        }
        // Advance.
        if let Some(_) = lock_col {
            // Column locked → only rows advance.
            r += 1;
        } else if let Some(_) = lock_row {
            // Row locked → only columns; if we run off the right
            // edge there's no valid placement on that row, so
            // overflow into a new column past the grid (col == cols).
            // We allow that — the cell will paint outside.
            if c + 1 + col_span > cols {
                return (r, c); // overflow case
            }
            c += 1;
        } else {
            c += 1;
            if c + col_span > cols {
                c = 0;
                r += 1;
            }
        }
    }
}

/// Resolve a single axis (columns or rows) of track sizes against the
/// available space. See `layout_grid`'s doc comment for the exact
/// algorithm; in short: fixed → exact, fr → share residue, auto →
/// share whatever fr left untouched (or the whole residue when there
/// are no fr tracks).
fn resolve_track_axis(tracks: &[TrackSize], avail: f32, font_size: f32, percent_basis: f32) -> Vec<f32> {
    let n = tracks.len();
    let mut sizes = vec![0.0_f32; n];
    let mut fixed = 0.0_f32;
    let mut fr_total = 0.0_f32;
    let mut auto_count = 0usize;

    for (i, t) in tracks.iter().enumerate() {
        match t {
            TrackSize::Length(l) => {
                sizes[i] = l.resolve(font_size, 16.0, percent_basis).max(0.0);
                fixed += sizes[i];
            }
            TrackSize::Fr(f) => fr_total += f.max(0.0),
            TrackSize::Auto => auto_count += 1,
            TrackSize::MinMax(_, b) => match b {
                MinMaxSide::Length(l) => {
                    sizes[i] = l.resolve(font_size, 16.0, percent_basis).max(0.0);
                    fixed += sizes[i];
                }
                MinMaxSide::Fr(f) => fr_total += f.max(0.0),
                MinMaxSide::Auto => auto_count += 1,
            },
        }
    }

    let remaining = (avail - fixed).max(0.0);
    if fr_total > 0.0 {
        for (i, t) in tracks.iter().enumerate() {
            let fr = match t {
                TrackSize::Fr(f) => *f,
                TrackSize::MinMax(_, MinMaxSide::Fr(f)) => *f,
                _ => 0.0,
            };
            if fr > 0.0 {
                sizes[i] = remaining * fr / fr_total;
            }
        }
    } else if auto_count > 0 {
        let share = remaining / auto_count as f32;
        for (i, t) in tracks.iter().enumerate() {
            let is_auto = matches!(
                t,
                TrackSize::Auto | TrackSize::MinMax(_, MinMaxSide::Auto)
            );
            if is_auto {
                sizes[i] = share;
            }
        }
    }
    sizes
}

/// Auto-table layout (subset of CSS Tables L3).
///
/// Steps:
///   1. Walk the subtree to find every descendant box with
///      `display: table-row` and the cells inside it. `<tbody>` /
///      `<thead>` / `<tfoot>` (UA-styled `display: block`) are
///      transparent, so a `<table>` with a `<tbody>` wrapper still
///      finds its rows.
///   2. Column count = max cells in any row. Width is split evenly
///      among columns inside the content rect — `width: Npx` and
///      `width: N%` on individual cells aren't honoured yet.
///   3. Rows lay out top-to-bottom; each row picks the tallest cell
///      and stretches every cell in the row to that height so cell
///      backgrounds align.
///
/// Wrapper boxes (`tbody` etc.) get a frame that encloses their rows
/// so background painting and hit-testing on them still work.
fn layout_table(bx: &mut LayoutBox, x: f32, y: f32, container_w: f32, container_h: Option<f32>) {
    let _ = container_h;
    let cv = bx.style.clone();
    let m = resolve_edges(&cv.margin, cv.font_size, container_w);
    let p = resolve_edges(&cv.padding, cv.font_size, container_w);
    let b = resolve_edges(&cv.border, cv.font_size, container_w);
    // Honour an explicit `width` so an infobox sized at 240px doesn't
    // sprawl to fill the viewport. Auto = fill the available width
    // minus the table's own box-model edges.
    let content_w = match cv.width {
        Dimension::Length(len) => len.resolve(cv.font_size, 16.0, container_w),
        Dimension::Auto => (container_w - m.left - m.right - p.left - p.right - b.left - b.right)
            .max(0.0),
    };
    let content_x = x + m.left + b.left + p.left;
    let content_y = y + m.top + b.top + p.top;

    let max_cols = max_cells_in_any_row(&bx.children).max(1);

    // caption-side handling is deferred — identifying the
    // <caption> child requires either threading the Document into
    // layout or a dedicated BoxKind, neither of which is bounded
    // for this commit. The cv.caption_side value parses correctly
    // and we'll wire reordering when we surface the caption shape
    // through the layout tree.

    let gap_x = if matches!(cv.border_collapse, bui_style::BorderCollapse::Collapse) {
        0.0
    } else {
        cv.border_spacing_x
            .resolve(cv.font_size, 16.0, content_w)
            .max(0.0)
    };
    let gap_y = if matches!(cv.border_collapse, bui_style::BorderCollapse::Collapse) {
        0.0
    } else {
        cv.border_spacing_y
            .resolve(cv.font_size, 16.0, content_w)
            .max(0.0)
    };

    // border-collapse: collapse — drop the right border of every cell
    // except the last in its row, and drop the bottom border of every
    // cell except cells in the last row. The neighbouring cell's
    // left/top border now stands in for both, so the page sees one
    // line where it used to see two stacked.
    if matches!(cv.border_collapse, bui_style::BorderCollapse::Collapse) {
        collapse_cell_borders(&mut bx.children, max_cols);
    }

    // Per-column widths. We seed from declared cell widths in the
    // first row (covering the typical Wikipedia infobox shape:
    // narrow label column + wide value column). Anything left as
    // `auto` shares the remaining width equally. Inter-column gaps
    // (`border-spacing-x`) are subtracted before splitting.
    let total_gap = if max_cols > 1 {
        gap_x * (max_cols - 1) as f32
    } else {
        0.0
    };
    let col_widths = resolve_table_columns(
        &bx.children,
        &bx.col_widths,
        max_cols,
        (content_w - total_gap).max(0.0),
        cv.font_size,
    );

    let mut cursor_y = content_y;
    // Per-column "still occupied for N more rows" counter — drives
    // rowspan: a cell with rowspan=2 marks its columns as occupied
    // for one more row, so the next row's cells skip past them.
    let mut occupied: Vec<u32> = vec![0; max_cols];
    layout_table_rows(
        &mut bx.children,
        content_x,
        &mut cursor_y,
        &col_widths,
        max_cols,
        &mut occupied,
        gap_x,
        gap_y,
    );

    let height = (cursor_y - content_y).max(0.0);
    bx.frame = Frame {
        x: x + m.left,
        y: y + m.top,
        width: content_w + p.left + p.right + b.left + b.right,
        height: height + p.top + p.bottom + b.top + b.bottom,
    };
}

/// Build per-column widths for a `<table>` from the first row's
/// declared cell widths. Cells with `width: <length>` (or the legacy
/// `width="N"` attribute, supported via the existing CSS pipeline)
/// pin their column. Auto columns share the leftover space equally.
///
/// We don't (yet) walk `<col>` / `<colgroup>` siblings — most pages
/// declare widths on the cells themselves anyway, and the v1
/// behaviour matches Wikipedia's typical taxobox shape (narrow
/// label column + wide value column).
fn resolve_table_columns(
    children: &[LayoutBox],
    col_widths: &[Option<f32>],
    max_cols: usize,
    content_w: f32,
    font_size: f32,
) -> Vec<f32> {
    let mut declared: Vec<Option<f32>> = vec![None; max_cols];
    // Pre-seed from <col> / <colgroup> declared widths (collected
    // during build phase). First-row cell widths still override these
    // if the cell declares an explicit width — matches HTML's
    // historic behaviour where cell widths win over col widths.
    for (i, w) in col_widths.iter().enumerate().take(max_cols) {
        if let Some(px) = *w {
            declared[i] = Some(px);
        }
    }

    fn walk_first_row<'a>(
        children: &'a [LayoutBox],
        out: &mut Option<&'a [LayoutBox]>,
    ) {
        for c in children {
            if out.is_some() {
                return;
            }
            if matches!(c.style.display, Display::TableRow) {
                *out = Some(&c.children);
                return;
            } else {
                walk_first_row(&c.children, out);
            }
        }
    }
    let mut first_row: Option<&[LayoutBox]> = None;
    walk_first_row(children, &mut first_row);

    if let Some(row_cells) = first_row {
        let mut col_idx = 0usize;
        for cell in row_cells {
            if !matches!(cell.style.display, Display::TableCell) {
                continue;
            }
            let cspan = cell.colspan.max(1) as usize;
            if let Dimension::Length(l) = cell.style.width {
                let w = l.resolve(font_size, 16.0, content_w).max(0.0);
                // Distribute the declared width evenly across the
                // spanned columns. v1 simplification — real tables
                // use intrinsic-content sizing under colspan, but
                // the equal-split looks right for the common
                // single-column declaration case.
                let per = w / cspan as f32;
                for k in 0..cspan {
                    let c = col_idx + k;
                    if c < max_cols {
                        declared[c] = Some(per);
                    }
                }
            }
            col_idx += cspan;
        }
    }

    let fixed_total: f32 = declared.iter().filter_map(|o| *o).sum();
    let auto_count = declared.iter().filter(|o| o.is_none()).count();
    let remaining = (content_w - fixed_total).max(0.0);
    let auto_share = if auto_count > 0 {
        remaining / auto_count as f32
    } else {
        0.0
    };
    declared
        .into_iter()
        .map(|o| o.unwrap_or(auto_share))
        .collect()
}

/// Walk a `<table>` subtree and zero out the borders that would
/// double up on a collapsed grid. We strip:
///
///   * each cell's `right` border, *except* the row's last cell;
///   * each cell's `bottom` border, *except* cells in the table's
///     last row.
///
/// The neighbour's `left` / `top` border survives and visually
/// represents both edges. We don't try to merge widths or colours
/// — for the typical infobox case where every cell declares the
/// same `1px solid` border, picking one wins is indistinguishable
/// from a true merge.
fn collapse_cell_borders(children: &mut [LayoutBox], max_cols: usize) {
    // Flatten rows out of any tbody/thead/tfoot wrappers so we can
    // operate uniformly. We use an index pass to preserve borrow
    // discipline (mut walk via a local helper).
    fn collect_rows<'a>(
        nodes: &'a mut [LayoutBox],
        out: &mut Vec<*mut LayoutBox>,
    ) {
        for n in nodes.iter_mut() {
            if matches!(n.style.display, Display::TableRow) {
                out.push(n as *mut _);
            } else {
                collect_rows(&mut n.children, out);
            }
        }
    }
    let mut rows: Vec<*mut LayoutBox> = Vec::new();
    collect_rows(children, &mut rows);
    let row_count = rows.len();
    for (ri, row_ptr) in rows.iter().enumerate() {
        let row = unsafe { &mut **row_ptr };
        let cells: Vec<&mut LayoutBox> = row
            .children
            .iter_mut()
            .filter(|c| matches!(c.style.display, Display::TableCell))
            .collect();
        let cell_count = cells.len();
        for (ci, cell) in cells.into_iter().enumerate() {
            let last_col = ci + 1 == cell_count.max(max_cols);
            let last_row = ri + 1 == row_count;
            if !last_col {
                cell.style.border.right = bui_style::Length::Px(0.0);
            }
            if !last_row {
                cell.style.border.bottom = bui_style::Length::Px(0.0);
            }
        }
    }
}

/// Given a box's display rect and an image's intrinsic dimensions,
/// compute the rectangle where the texture should actually paint
/// per the CSS `object-fit` keyword. Returns `(paint_rect, clip)`
/// where `clip` is true when the paint rect can fall outside the
/// box and the renderer needs to push a clip layer.
fn compute_object_fit_rect(
    frame: Frame,
    intrinsic: (f32, f32),
    fit: bui_style::ObjectFit,
) -> (Frame, bool) {
    let (iw, ih) = (intrinsic.0.max(1.0), intrinsic.1.max(1.0));
    let bw = frame.width.max(0.0);
    let bh = frame.height.max(0.0);
    if bw <= 0.0 || bh <= 0.0 {
        return (frame, false);
    }
    match fit {
        bui_style::ObjectFit::Fill => (frame, false),
        bui_style::ObjectFit::Contain => {
            let scale = (bw / iw).min(bh / ih);
            let pw = iw * scale;
            let ph = ih * scale;
            let px = frame.x + (bw - pw) * 0.5;
            let py = frame.y + (bh - ph) * 0.5;
            (Frame { x: px, y: py, width: pw, height: ph }, false)
        }
        bui_style::ObjectFit::Cover => {
            let scale = (bw / iw).max(bh / ih);
            let pw = iw * scale;
            let ph = ih * scale;
            let px = frame.x + (bw - pw) * 0.5;
            let py = frame.y + (bh - ph) * 0.5;
            (Frame { x: px, y: py, width: pw, height: ph }, true)
        }
        bui_style::ObjectFit::None => {
            let px = frame.x + (bw - iw) * 0.5;
            let py = frame.y + (bh - ih) * 0.5;
            (Frame { x: px, y: py, width: iw, height: ih }, true)
        }
        bui_style::ObjectFit::ScaleDown => {
            // Whichever of `none` / `contain` produces the smaller box.
            let contain_scale = (bw / iw).min(bh / ih);
            let scale = contain_scale.min(1.0);
            let pw = iw * scale;
            let ph = ih * scale;
            let px = frame.x + (bw - pw) * 0.5;
            let py = frame.y + (bh - ph) * 0.5;
            (Frame { x: px, y: py, width: pw, height: ph }, false)
        }
    }
}

/// Paint a background image inside `bx.frame` honouring CSS
/// `background-size`, `background-position`, and `background-repeat`.
/// We don't know the texture's intrinsic dimensions at this layer
/// (the GPU image cache holds them), so for the cover/contain math
/// we treat the box as both source and destination — works
/// correctly for the common case where authors hand-pick a
/// background image whose intrinsic ratio they want preserved via
/// width/height: <length> + background-size: cover.
fn paint_background_image(out: &mut DisplayList, bx: &LayoutBox, key: &str) {
    let frame = bx.frame;
    let cv = &bx.style;

    // Resolve background-size to a paint rect within the frame. With
    // no intrinsic dimensions plumbed through to the painter, Cover
    // and Contain collapse to "fill the frame" (still correct for
    // typical chrome icons that ship at the box's exact size).
    let (paint_w, paint_h) = match cv.background_size {
        bui_style::BackgroundSize::Auto => (frame.width, frame.height),
        bui_style::BackgroundSize::Cover | bui_style::BackgroundSize::Contain => {
            (frame.width, frame.height)
        }
        bui_style::BackgroundSize::Length(lw, lh) => {
            let w = match lw {
                bui_style::Length::Px(0.0) => frame.width,
                _ => lw.resolve(cv.font_size, 16.0, frame.width).max(0.0),
            };
            let h = match lh {
                bui_style::Length::Px(0.0) => frame.height,
                _ => lh.resolve(cv.font_size, 16.0, frame.height).max(0.0),
            };
            (w, h)
        }
    };

    let bx_pos = |axis: bui_style::BackgroundAxisPos, basis: f32, image_extent: f32| -> f32 {
        match axis {
            bui_style::BackgroundAxisPos::Anchor(t) => (basis - image_extent) * t,
            bui_style::BackgroundAxisPos::Length(l) => {
                l.resolve(cv.font_size, 16.0, basis)
            }
        }
    };
    let dx = bx_pos(cv.background_position.x, frame.width, paint_w);
    let dy = bx_pos(cv.background_position.y, frame.height, paint_h);

    let origin_x = frame.x + dx;
    let origin_y = frame.y + dy;
    if paint_w <= 0.0 || paint_h <= 0.0 {
        return;
    }

    // background-repeat determines tiling. With no-repeat we paint
    // once at (origin); repeat / repeat-x / repeat-y fill the box
    // along the matching axis. We cap the tile count so a tiny
    // 1px image can't blow paint cost on a huge box.
    use bui_style::BackgroundRepeat as BR;
    let (rx, ry) = match cv.background_repeat {
        BR::NoRepeat => (false, false),
        BR::Repeat => (true, true),
        BR::RepeatX => (true, false),
        BR::RepeatY => (false, true),
    };
    let max_tiles = 64;
    let tile_w = paint_w.max(1.0);
    let tile_h = paint_h.max(1.0);

    // Compute the leftmost / topmost tile origins so a centred
    // anchor still tiles outward.
    let mut start_x = origin_x;
    if rx {
        while start_x - tile_w >= frame.x {
            start_x -= tile_w;
        }
    }
    let mut start_y = origin_y;
    if ry {
        while start_y - tile_h >= frame.y {
            start_y -= tile_h;
        }
    }

    let mut tiles_emitted = 0;
    let mut y = start_y;
    while y < frame.y + frame.height {
        let mut x = start_x;
        while x < frame.x + frame.width {
            // Skip tiles entirely outside the box — origin shift can
            // place a tile to the left/above when not repeating.
            if x + tile_w > frame.x && y + tile_h > frame.y {
                out.image(Rect::new(x, y, tile_w, tile_h), key.to_string());
                tiles_emitted += 1;
                if tiles_emitted >= max_tiles {
                    return;
                }
            }
            if !rx {
                break;
            }
            x += tile_w;
        }
        if !ry {
            break;
        }
        y += tile_h;
    }
}

fn max_cells_in_any_row(children: &[LayoutBox]) -> usize {
    let mut max = 0usize;
    for c in children {
        if matches!(c.style.display, Display::TableRow) {
            // Sum cell colspans so a row with one colspan-2 cell
            // contributes 2 to the column count, not 1.
            let cells: u32 = c
                .children
                .iter()
                .filter(|cc| matches!(cc.style.display, Display::TableCell))
                .map(|cc| cc.colspan.max(1))
                .sum();
            let cells = cells as usize;
            if cells > max {
                max = cells;
            }
        } else {
            let inner = max_cells_in_any_row(&c.children);
            if inner > max {
                max = inner;
            }
        }
    }
    max
}

fn layout_table_rows(
    children: &mut [LayoutBox],
    content_x: f32,
    cursor_y: &mut f32,
    col_widths: &[f32],
    max_cols: usize,
    occupied: &mut Vec<u32>,
    gap_x: f32,
    gap_y: f32,
) {
    let column_offset = |col_idx: usize| -> f32 {
        col_widths.iter().take(col_idx).sum::<f32>() + gap_x * col_idx as f32
    };
    for row in children.iter_mut() {
        if matches!(row.style.display, Display::TableRow) {
            let row_y = *cursor_y;
            let mut col_idx = 0usize;
            let mut max_h = 0.0f32;
            for cell in row.children.iter_mut() {
                if !matches!(cell.style.display, Display::TableCell) {
                    continue;
                }
                // Skip columns still claimed by a rowspanning cell
                // from a row above. Without this, a row with one
                // explicit <td> would land at column 0 even when the
                // spanning cell from the previous row is still
                // occupying it.
                while col_idx < max_cols && occupied.get(col_idx).copied().unwrap_or(0) > 0 {
                    col_idx += 1;
                }
                let cspan = cell.colspan.max(1) as usize;
                let rspan = cell.rowspan.max(1);
                let widths_sum: f32 = col_widths
                    .iter()
                    .skip(col_idx)
                    .take(cspan)
                    .sum();
                // Spanned cells absorb the gaps between the columns
                // they cover.
                let cell_w = widths_sum + gap_x * cspan.saturating_sub(1) as f32;
                let col_x = content_x + column_offset(col_idx);
                layout_block(cell, col_x, row_y, cell_w, None);
                // Only single-row cells contribute to this row's
                // height. Spanning cells stretch over multiple rows
                // and shouldn't dominate any single row.
                if rspan == 1 && cell.frame.height > max_h {
                    max_h = cell.frame.height;
                }
                // Mark this cell's columns as occupied for the next
                // (rspan-1) rows. Decrement happens at row end.
                if rspan > 1 {
                    for k in 0..cspan {
                        let c = col_idx + k;
                        if c < max_cols {
                            occupied[c] = rspan; // we decrement at row end
                        }
                    }
                }
                col_idx += cspan;
            }
            // Stretch single-row cells to the row's tallest height so
            // backgrounds align across the row. Spanning cells keep
            // their natural heights — the flow still works because
            // following rows skip the occupied columns. Each cell is
            // also vertically aligned within the row using its
            // computed `vertical-align`: baseline / top stay at the
            // row's top edge; middle shifts the content down by half
            // the slack; bottom places it flush with the row's
            // bottom. Without this, a row with a tall infobox image
            // and a short label cell rendered the label at the very
            // top instead of centered.
            for cell in row.children.iter_mut() {
                if matches!(cell.style.display, Display::TableCell)
                    && cell.rowspan.max(1) == 1
                {
                    let natural_h = cell.frame.height;
                    let slack = (max_h - natural_h).max(0.0);
                    let dy = match cell.style.vertical_align {
                        bui_style::VerticalAlign::Middle => slack * 0.5,
                        bui_style::VerticalAlign::Bottom
                        | bui_style::VerticalAlign::TextBottom => slack,
                        _ => 0.0,
                    };
                    if dy > 0.0 {
                        // Shift the cell's content (lines + children)
                        // without moving the cell's own frame — we
                        // still want the cell to span the full row
                        // height so its background/border match.
                        for line in &mut cell.lines {
                            line.frame.y += dy;
                            for item in &mut line.items {
                                match item {
                                    LineItem::Text(r) => r.frame.y += dy,
                                    LineItem::Image { frame, .. } => frame.y += dy,
                                    LineItem::Control { frame, .. } => frame.y += dy,
                                    LineItem::Svg { frame, .. } => frame.y += dy,
                                    LineItem::InlineBlock { frame, host, .. } => {
                                        frame.y += dy;
                                        shift_subtree(host, 0.0, dy);
                                    }
                                }
                            }
                        }
                        for child in &mut cell.children {
                            shift_subtree(child, 0.0, dy);
                        }
                    }
                    cell.frame.height = max_h;
                }
            }
            row.frame = Frame {
                x: content_x,
                y: row_y,
                width: col_widths.iter().sum::<f32>(),
                height: max_h,
            };
            *cursor_y = row_y + max_h + gap_y;
            // Decrement column-occupancy counters at end of row.
            for v in occupied.iter_mut() {
                if *v > 0 {
                    *v -= 1;
                }
            }
        } else {
            // tbody / thead / tfoot — transparent wrapper. Recurse into it
            // but also bracket its frame around the rows it contains so
            // hit-tests on the wrapper don't fall through to the table.
            let wrap_y = *cursor_y;
            layout_table_rows(
                &mut row.children,
                content_x,
                cursor_y,
                col_widths,
                max_cols,
                occupied,
                gap_x,
                gap_y,
            );
            row.frame = Frame {
                x: content_x,
                y: wrap_y,
                width: col_widths.iter().sum::<f32>(),
                height: (*cursor_y - wrap_y).max(0.0),
            };
        }
    }
}

fn layout_block(
    bx: &mut LayoutBox,
    x: f32,
    y: f32,
    container_w: f32,
    container_h: Option<f32>,
) {
    if matches!(bx.style.display, Display::Flex) {
        return layout_flex(bx, x, y, container_w, container_h);
    }
    if matches!(bx.style.display, Display::Grid) {
        return layout_grid(bx, x, y, container_w, container_h);
    }
    if matches!(bx.style.display, Display::Table) {
        return layout_table(bx, x, y, container_w, container_h);
    }
    let cv = &bx.style;
    let basis = container_w;
    let mut m = resolve_edges(&cv.margin, cv.font_size, basis);
    let p = resolve_edges(&cv.padding, cv.font_size, basis);
    let b = resolve_edges(&cv.border, cv.font_size, basis);

    let mut content_w = match cv.width {
        Dimension::Auto => (container_w - m.left - m.right - p.left - p.right - b.left - b.right)
            .max(0.0),
        Dimension::Length(len) => {
            let raw = len.resolve(cv.font_size, 16.0, container_w);
            // `box-sizing: border-box` includes padding + border in the
            // declared size; subtract them to recover the content width.
            if matches!(cv.box_sizing, bui_style::BoxSizing::BorderBox) {
                (raw - p.left - p.right - b.left - b.right).max(0.0)
            } else {
                raw
            }
        }
    };
    // CSS 2.1 §10.4: clamp content width to [min-width, max-width].
    // `max-width` wins over `min-width` only when min > max. Lengths
    // are resolved against the container width so percentages work.
    if let Some(mx) = cv.max_width {
        let raw = mx.resolve(cv.font_size, 16.0, container_w);
        let lim = if matches!(cv.box_sizing, bui_style::BoxSizing::BorderBox) {
            (raw - p.left - p.right - b.left - b.right).max(0.0)
        } else {
            raw
        };
        if content_w > lim {
            content_w = lim;
        }
    }
    if let Some(mn) = cv.min_width {
        let raw = mn.resolve(cv.font_size, 16.0, container_w);
        let lim = if matches!(cv.box_sizing, bui_style::BoxSizing::BorderBox) {
            (raw - p.left - p.right - b.left - b.right).max(0.0)
        } else {
            raw
        };
        if content_w < lim {
            content_w = lim;
        }
    }

    // CSS 2.1 §10.3.3: when `width` is fixed and one or both side
    // margins are `auto`, the leftover horizontal space is distributed
    // to the auto sides — both auto = horizontal centering; one auto =
    // align to the opposite side. We also honour this when `max-width`
    // clamped content_w smaller than the container (the
    // `margin: 0 auto; max-width: 688px` recipe Google's search box
    // uses): the box has effectively-fixed width even though
    // `cv.width` is still `Auto`.
    let max_width_clamped = cv.max_width.is_some()
        && content_w + p.left + p.right + b.left + b.right + 0.5 < container_w;
    if matches!(cv.width, Dimension::Length(_)) || max_width_clamped {
        let used = content_w + p.left + p.right + b.left + b.right;
        let leftover = (container_w - used).max(0.0);
        match (cv.margin_left_auto, cv.margin_right_auto) {
            (true, true) => {
                m.left = leftover * 0.5;
                m.right = leftover * 0.5;
            }
            (true, false) => {
                m.left = (leftover - m.right).max(0.0);
            }
            (false, true) => {
                m.right = (leftover - m.left).max(0.0);
            }
            (false, false) => {}
        }
    }

    let outer_x = x;
    let outer_y = y;
    let content_x = outer_x + m.left + b.left + p.left;
    let content_y = outer_y + m.top + b.top + p.top;

    // The basis we hand to children's percent-on-height. If our own
    // height is definite (CSS Length, not auto), resolve it against
    // the parent's basis and clamp by max-height — children see
    // the same number as `getComputedStyle().height` would in a
    // browser. If our height is auto, we just forward whatever
    // basis our parent gave us, so calc() inside grandchildren
    // still gets a definite ancestor when one exists higher up.
    let parent_basis_for_h = container_h.unwrap_or_else(|| {
        let (_, vh) = bui_style::viewport();
        vh
    });
    let child_container_h: Option<f32> = match cv.height {
        Dimension::Length(len) => {
            let raw = len.resolve(cv.font_size, 16.0, parent_basis_for_h);
            let inner = if matches!(cv.box_sizing, bui_style::BoxSizing::BorderBox) {
                (raw - p.top - p.bottom - b.top - b.bottom).max(0.0)
            } else {
                raw
            };
            let mut h = inner;
            if let Some(mx) = cv.max_height {
                let raw = mx.resolve(cv.font_size, 16.0, parent_basis_for_h);
                let lim = if matches!(cv.box_sizing, bui_style::BoxSizing::BorderBox) {
                    (raw - p.top - p.bottom - b.top - b.bottom).max(0.0)
                } else {
                    raw
                };
                if h > lim { h = lim; }
            }
            Some(h)
        }
        Dimension::Auto => {
            // Auto height with a tight max-height is still a definite
            // upper bound — pass the max as the basis so a child's
            // calc(100% - Npx) computes against it (Google's
            // .kKvsb { max-height: 230px } parent).
            cv.max_height.map(|mx| {
                let raw = mx.resolve(cv.font_size, 16.0, parent_basis_for_h);
                if matches!(cv.box_sizing, bui_style::BoxSizing::BorderBox) {
                    (raw - p.top - p.bottom - b.top - b.bottom).max(0.0)
                } else {
                    raw
                }
            })
        }
    };

    let mut cursor_y = content_y;

    // Active floats in the current block formatting context. `Left`
    // and `Right` lists track each side independently — non-float
    // siblings shrink against both, and `clear` pushes cursor_y past
    // whichever side(s) the property names.
    let mut left_floats: Vec<Frame> = Vec::new();
    let mut right_floats: Vec<Frame> = Vec::new();

    // CSS 2.1 §8.3.1 margin collapsing for adjacent block siblings:
    // the gap between a block and the next is `max(prev.margin_bottom,
    // next.margin_top)`, not their sum. We track the previous block
    // child's margin_bottom and pull cursor_y back by the smaller of
    // the two margins right before placing the next block — so
    // layout_block's own margin_top still adds in cleanly.
    let mut last_margin_bottom: f32 = 0.0;

    // Two cases: children all blocks (or anonymous), or this is an anonymous
    // box whose children are inline. Anonymous boxes lay out inline content
    // as wrapped text runs in the parent's content rect.
    let mut anon_children = Vec::new();
    std::mem::swap(&mut anon_children, &mut bx.children);

    // Pre-count floats per side so the auto-width fallback can
    // divide content_w evenly: 1 left-float → full width, 2 → 50/50,
    // 3 → ~33 each. Without this, the first auto float on a side
    // grabs all the room and pushes its siblings to a new row.
    let mut left_count = 0usize;
    let mut right_count = 0usize;
    for child in &anon_children {
        match child.style.float {
            bui_style::Float::Left => left_count += 1,
            bui_style::Float::Right => right_count += 1,
            bui_style::Float::None => {}
        }
    }

    'children: for mut child in anon_children {
        // Out-of-flow boxes (position: absolute / fixed) get a hypothetical
        // in-flow layout for sizing only — siblings don't reserve vertical
        // space for them. The post-layout `apply_positioning` pass will
        // shift them to their containing block's edge offsets.
        let out_of_flow = matches!(
            child.style.position,
            bui_style::Position::Absolute | bui_style::Position::Fixed,
        );

        // CSS `clear`: push cursor_y past the relevant floats before
        // we lay this child out.
        match child.style.clear {
            bui_style::Clear::None => {}
            bui_style::Clear::Left => {
                cursor_y = float_clear_y(cursor_y, &left_floats);
            }
            bui_style::Clear::Right => {
                cursor_y = float_clear_y(cursor_y, &right_floats);
            }
            bui_style::Clear::Both => {
                cursor_y = float_clear_y(cursor_y, &left_floats);
                cursor_y = float_clear_y(cursor_y, &right_floats);
            }
        }

        let float_side = match child.style.float {
            bui_style::Float::Left => Some(false), // false = left
            bui_style::Float::Right => Some(true), // true = right
            bui_style::Float::None => None,
        };

        if let Some(is_right) = float_side {
            // Float width: declared > shrink-to-fit approximation.
            // Without intrinsic measurement, we divide content_w
            // by the number of floats on this side: 1 = full width,
            // 2 = 50/50, etc. Inner content (tables, fixed-width
            // imgs, declared-width LIs) collapses the float back
            // down regardless. Lets wrapper floats like Wikipedia's
            // `.vector-menu` be wide enough for their content while
            // still allowing two side-by-side floats to share a row.
            let total_same_side = if is_right { right_count } else { left_count }.max(1);
            let target_w = match child.style.width {
                Dimension::Length(l) => l.resolve(child.style.font_size, 16.0, content_w),
                Dimension::Auto => (content_w / total_same_side as f32).max(0.0),
            };
            // Try to fit the float on the same row as existing
            // same-side floats first; only push down when there's no
            // horizontal room left at the current y. Without this,
            // every float wraps to its own row even when the row had
            // 800 px of free space (Wikipedia's tab list rendered
            // vertically because each tab pushed the next one below
            // instead of beside it).
            let same_side = if is_right { &right_floats } else { &left_floats };
            let mut place_y = cursor_y;
            loop {
                // Floats with y-extent overlapping place_y and on the
                // same side push our left edge in. If after their
                // combined push there's still target_w of room, take
                // place_y. Otherwise drop to the bottom of the
                // earliest-clearing float and try again.
                let mut row_offset = 0.0_f32;
                let mut earliest_clear = f32::INFINITY;
                for f in same_side {
                    if f.y < place_y + 1.0 && f.y + f.height > place_y {
                        if is_right {
                            row_offset = row_offset.max((content_x + content_w) - f.x);
                        } else {
                            row_offset = row_offset.max(f.x + f.width - content_x);
                        }
                        let bot = f.y + f.height;
                        if bot < earliest_clear {
                            earliest_clear = bot;
                        }
                    }
                }
                if row_offset + target_w <= content_w + 0.5 {
                    let row_offset_final = row_offset;
                    let place_x = if is_right {
                        content_x + content_w - target_w - row_offset_final
                    } else {
                        content_x + row_offset_final
                    };
                    match child.kind {
                        BoxKind::Block => layout_block(&mut child, place_x, place_y, target_w, child_container_h),
                        BoxKind::Anonymous => {
                            layout_inline(&mut child, place_x, place_y, target_w, child_container_h)
                        }
                        _ => layout_block(&mut child, place_x, place_y, target_w, child_container_h),
                    }
                    let float_frame = child.frame;
                    if is_right {
                        right_floats.push(float_frame);
                    } else {
                        left_floats.push(float_frame);
                    }
                    bx.children.push(child);
                    continue 'children;
                }
                if earliest_clear.is_finite() && earliest_clear > place_y {
                    place_y = earliest_clear;
                } else {
                    // No more floats above to clear; lay out at this y
                    // even if it overflows. Better to draw than to
                    // loop forever.
                    let place_x = if is_right { content_x } else { content_x };
                    match child.kind {
                        BoxKind::Block => layout_block(&mut child, place_x, place_y, target_w, child_container_h),
                        BoxKind::Anonymous => {
                            layout_inline(&mut child, place_x, place_y, target_w, child_container_h)
                        }
                        _ => layout_block(&mut child, place_x, place_y, target_w, child_container_h),
                    }
                    let float_frame = child.frame;
                    if is_right {
                        right_floats.push(float_frame);
                    } else {
                        left_floats.push(float_frame);
                    }
                    bx.children.push(child);
                    continue 'children;
                }
            }
        }

        // Non-float child: shrink available width by intruding floats
        // at this cursor_y.
        let (avail_x, avail_w) =
            available_at_y(content_x, content_w, cursor_y, &left_floats, &right_floats);
        match child.kind {
            BoxKind::Block => {
                let next_margin_top = resolve_length(
                    child.style.margin.top,
                    child.style.font_size,
                    content_w,
                );
                let collapse = last_margin_bottom.min(next_margin_top).max(0.0);
                let placement_y = cursor_y - collapse;
                layout_block(&mut child, avail_x, placement_y, avail_w, child_container_h);
                if !out_of_flow {
                    let mb = resolve_length(
                        child.style.margin.bottom,
                        child.style.font_size,
                        content_w,
                    );
                    cursor_y = child.frame.y + child.frame.height + mb;
                    last_margin_bottom = mb;
                }
                bx.children.push(child);
            }
            BoxKind::Anonymous => {
                layout_inline(&mut child, avail_x, cursor_y, avail_w, child_container_h);
                if !out_of_flow {
                    cursor_y = child.frame.y + child.frame.height;
                    last_margin_bottom = 0.0;
                }
                bx.children.push(child);
            }
            BoxKind::InlineText(_)
            | BoxKind::InlineImage(_)
            | BoxKind::InlineControl(_)
            | BoxKind::InlineSvg(_)
            | BoxKind::InlineBreak
            | BoxKind::InlineBlockHost(_) => {
                // Lone inline content inside a block — wrap in an anonymous box.
                let mut anon = LayoutBox {
                    node: None,
                    style: bx.style.clone(),
                    kind: BoxKind::Anonymous,
                    children: vec![child],
                    frame: Frame::ZERO,
                    lines: Vec::new(), list_marker: None, colspan: 1, rowspan: 1, col_widths: Vec::new(),
                };
                layout_inline(&mut anon, avail_x, cursor_y, avail_w, child_container_h);
                cursor_y = anon.frame.y + anon.frame.height;
                last_margin_bottom = 0.0;
                bx.children.push(anon);
            }
        }
    }

    // After all in-flow siblings, ensure the parent's cursor_y reaches
    // past any taller float on either side — without this an article
    // image taller than its surrounding paragraphs would extend beyond
    // the block, and following content would render on top of it.
    cursor_y = float_clear_y(cursor_y, &left_floats);
    cursor_y = float_clear_y(cursor_y, &right_floats);

    let content_height = (cursor_y - content_y).max(0.0);
    // CSS `aspect-ratio: W/H` derives one of the two axes when the
    // other is determined and the deriving axis is `auto`. Common
    // shape: `<img style="width: 200px; aspect-ratio: 16 / 9">`.
    let derived_height_from_aspect = if matches!(cv.height, Dimension::Auto) {
        cv.aspect_ratio.map(|ratio| {
            let outer_width = content_w + p.left + p.right + b.left + b.right;
            let derived_outer_h = outer_width / ratio;
            (derived_outer_h - p.top - p.bottom - b.top - b.bottom).max(0.0)
        })
    } else {
        None
    };
    // CSS Values 4 §6.6.1: a percentage on `height` resolves
    // against the containing block's height. The right basis is
    // `container_h` plumbed by the parent — its resolved (and
    // max-height-clamped) content height — falling back to vh
    // when no parent height is definite. When the parent's height
    // is itself indefinite (auto with no enclosing definite
    // ancestor), the percentage computes back to auto, which we
    // treat as content_height. Without this, Google's
    // `<div style="height: calc(100% - 200px)">` inside a 230-px
    // max-height parent resolved against container_w (1304),
    // pushing the logo image past the viewport.
    let height_percent_basis = container_h.unwrap_or_else(|| {
        let (_, vh) = bui_style::viewport();
        vh
    });
    let mut total_height = match cv.height {
        Dimension::Length(len) => {
            let parent_indefinite = container_h.is_none();
            let plain_percent =
                matches!(cv.height, Dimension::Length(bui_style::Length::Percent(_)));
            // Plain `height: 100%` against an indefinite parent
            // collapses to content_height (Wikipedia's `.mw-logo`
            // case). Calc(percent ± px) and definite parents go
            // through the resolver with the proper basis.
            let raw = if plain_percent && parent_indefinite {
                content_height
            } else {
                len.resolve(cv.font_size, 16.0, height_percent_basis)
            };
            if matches!(cv.box_sizing, bui_style::BoxSizing::BorderBox) {
                (raw - p.top - p.bottom - b.top - b.bottom).max(0.0)
            } else {
                raw
            }
        }
        Dimension::Auto => derived_height_from_aspect
            .filter(|h| *h > content_height)
            .unwrap_or(content_height),
    };
    if let Some(mx) = cv.max_height {
        let raw = mx.resolve(cv.font_size, 16.0, height_percent_basis);
        let lim = if matches!(cv.box_sizing, bui_style::BoxSizing::BorderBox) {
            (raw - p.top - p.bottom - b.top - b.bottom).max(0.0)
        } else {
            raw
        };
        if total_height > lim {
            total_height = lim;
        }
    }
    if let Some(mn) = cv.min_height {
        let raw = mn.resolve(cv.font_size, 16.0, height_percent_basis);
        let lim = if matches!(cv.box_sizing, bui_style::BoxSizing::BorderBox) {
            (raw - p.top - p.bottom - b.top - b.bottom).max(0.0)
        } else {
            raw
        };
        if total_height < lim {
            total_height = lim;
        }
    }

    bx.frame = Frame {
        x: outer_x + m.left,
        y: outer_y + m.top,
        width: content_w + p.left + p.right + b.left + b.right,
        height: total_height + p.top + p.bottom + b.top + b.bottom,
    };
}

fn layout_inline(bx: &mut LayoutBox, x: f32, y: f32, container_w: f32, container_h: Option<f32>) {
    let mut lines: Vec<LineBox> = Vec::new();
    let mut cur_items: Vec<LineItem> = Vec::new();
    // CSS `text-indent` applies to the first line of inline content.
    // We seed cur_x with the (positive) indent so the first
    // measure-and-place runs from there. Subsequent lines reset to 0.
    let mut cur_x = bx.style.text_indent.max(0.0);
    let mut max_font_size = 0.0f32;
    // Cumulative y of the next line's top edge. We can't derive this
    // from `lines.len() * line_h` because line_h varies across lines
    // (a paragraph with mixed font sizes would have differently-tall
    // lines, and `len * current_h` overcounts or undercounts).
    let mut cur_y = y;

    let font = bui_text::shared_font();
    // Consume children: an Anonymous box's children are entirely
    // inline (collect_children guarantees this), and after this
    // function the laid-out content lives in `bx.lines`. Taking
    // ownership lets us move data out of the children — needed for
    // `InlineBlockHost`, whose payload is a non-Clone Box<LayoutBox>.
    let owned_children: Vec<LayoutBox> = std::mem::take(&mut bx.children);
    for mut child in owned_children {
        match child.kind {
            BoxKind::InlineText(ref text) => {
                let cv = &child.style;
                let space_w = font.glyph_advance(' ', cv.font_size);
                max_font_size = max_font_size.max(cv.font_size);

                // CSS `text-transform` reshapes the source text before
                // shaping. We apply per-word so capitalize keeps space-
                // delimited word boundaries.
                let transformed = apply_text_transform(text, cv.text_transform);
                let words: Vec<&str> = transformed.split_inclusive(' ').collect();
                for word in words {
                    let trimmed = word.trim_end_matches(' ');
                    let trailing_space = word.ends_with(' ');
                    if trimmed.is_empty() {
                        if trailing_space && cur_x > 0.0 {
                            cur_x += space_w;
                        }
                        continue;
                    }
                    let text_w = font.measure_text_with_spacing(trimmed, cv.font_size, cv.letter_spacing);
                    let nowrap = matches!(
                        cv.white_space,
                        bui_style::WhiteSpace::Nowrap | bui_style::WhiteSpace::Pre,
                    );
                    if !nowrap && cur_x + text_w > container_w && !cur_items.is_empty() {
                        let line_h = max_font_size * cv.line_height.max(1.0);
                        lines.push(LineBox {
                            frame: Frame {
                                x,
                                y: cur_y,
                                width: container_w,
                                height: line_h,
                            },
                            items: std::mem::take(&mut cur_items),
                        });
                        cur_y += line_h;
                        cur_x = 0.0;
                        max_font_size = cv.font_size;
                    }
                    // word-break: break-all / overflow-wrap: break-word —
                    // when the word can't fit even on a fresh line,
                    // split it character-by-character into chunks that
                    // each fit within container_w.
                    let allow_break = !nowrap
                        && (matches!(cv.word_break, bui_style::WordBreak::BreakAll)
                            || matches!(
                                cv.overflow_wrap,
                                bui_style::OverflowWrap::BreakWord
                                    | bui_style::OverflowWrap::Anywhere
                            ));
                    if allow_break && text_w > container_w - cur_x {
                        let chunks = break_word_chunks(
                            trimmed,
                            &font,
                            cv.font_size,
                            cv.letter_spacing,
                            container_w,
                            cur_x,
                        );
                        for (i, chunk) in chunks.iter().enumerate() {
                            let chunk_w = font.measure_text_with_spacing(
                                chunk,
                                cv.font_size,
                                cv.letter_spacing,
                            );
                            if i > 0 {
                                // commit the running line and start fresh.
                                let line_h = max_font_size * cv.line_height.max(1.0);
                                lines.push(LineBox {
                                    frame: Frame {
                                        x,
                                        y: cur_y,
                                        width: container_w,
                                        height: line_h,
                                    },
                                    items: std::mem::take(&mut cur_items),
                                });
                                cur_y += line_h;
                                cur_x = 0.0;
                                max_font_size = cv.font_size;
                            }
                            let run_x = x + cur_x;
                            cur_items.push(LineItem::Text(TextRun {
                                text: chunk.clone(),
                                style: cv.clone(),
                                frame: Frame {
                                    x: run_x,
                                    y: 0.0,
                                    width: chunk_w,
                                    height: cv.font_size,
                                },
                                node: child.node,
                            }));
                            cur_x += chunk_w;
                        }
                        if trailing_space {
                            cur_x += space_w;
                        }
                        continue;
                    }
                    let run_x = x + cur_x;
                    cur_items.push(LineItem::Text(TextRun {
                        text: trimmed.to_string(),
                        style: cv.clone(),
                        frame: Frame {
                            x: run_x,
                            y: 0.0,
                            width: text_w,
                            height: cv.font_size,
                        },
                        node: child.node,
                    }));
                    cur_x += text_w;
                    if trailing_space {
                        cur_x += space_w;
                    }
                }
            }
            BoxKind::InlineImage(entry) => {
                // CSS width/height on <img> overrides the intrinsic
                // dimensions so authors can lock a thumb to a fixed
                // size; object-fit then decides how the texture sits
                // inside that box. Auto on either axis falls back to
                // the intrinsic, matching browser behaviour.
                let intrinsic_w = entry.width.max(1.0);
                let intrinsic_h = entry.height.max(1.0);
                let cv = &child.style;
                let (img_w, img_h) = resolve_replaced_size(
                    cv, intrinsic_w, intrinsic_h, container_w, container_h,
                );
                max_font_size = max_font_size.max(img_h);
                if cur_x + img_w > container_w && !cur_items.is_empty() {
                    let line_h = max_font_size * cv.line_height.max(1.0);
                    lines.push(LineBox {
                        frame: Frame {
                            x,
                            y: cur_y,
                            width: container_w,
                            height: line_h,
                        },
                        items: std::mem::take(&mut cur_items),
                    });
                    cur_y += line_h;
                    cur_x = 0.0;
                    max_font_size = img_h;
                }
                cur_items.push(LineItem::Image {
                    frame: Frame {
                        x: x + cur_x,
                        y: 0.0, // filled in when committing the line
                        width: img_w,
                        height: img_h,
                    },
                    key: entry.key.clone(),
                    node: child.node,
                    vertical_align: cv.vertical_align,
                    intrinsic: (intrinsic_w, intrinsic_h),
                    object_fit: cv.object_fit,
                });
                cur_x += img_w;
            }
            BoxKind::InlineSvg(entry) => {
                let cv = &child.style;
                let intrinsic_w = entry.width.max(1.0);
                let intrinsic_h = entry.height.max(1.0);
                // Author CSS `width` / `height` win over the SVG's
                // viewBox-derived intrinsic size — the same rule we
                // apply to <img> and <input>. Preserve aspect ratio
                // from the viewBox when only one dimension is given:
                // an SVG with viewBox="0 -960 960 960" and CSS
                // `width: 24px` should render 24×24, not 24×960.
                // Google's `svg { height: 100%; width: 100% }` icons
                // would otherwise render at 36 wide but 960 tall and
                // stretch their parent's line to 1152 px.
                // Icon SVGs that ship as `<svg viewBox="0 -960 960 960">`
                // with no width/height attrs and rely on a parent class to
                // size them: when BOTH axes are auto, fall back to a
                // font-size-scaled square so they render as glyph-sized
                // icons instead of blowing out the line to the viewBox's
                // hundreds of px (Google's material icons). Everything else
                // — including a definite axis with the other auto, and the
                // wordmark's `width:auto; height:100%` — goes through the
                // shared replaced-size resolver (percent height resolves
                // against the container height, aspect ratio preserved).
                let no_attr_size = entry.no_attr_size;
                let both_auto = matches!(
                    (cv.width, cv.height),
                    (Dimension::Auto, Dimension::Auto)
                );
                let looks_iconlike = no_attr_size
                    && (intrinsic_w - intrinsic_h).abs() < 0.5
                    && intrinsic_w >= 64.0;
                let (svg_w, svg_h) = if both_auto && looks_iconlike {
                    let s = cv.font_size.max(16.0);
                    (s, s)
                } else {
                    resolve_replaced_size(cv, intrinsic_w, intrinsic_h, container_w, container_h)
                };
                let svg_w = svg_w.max(1.0);
                let svg_h = svg_h.max(1.0);
                max_font_size = max_font_size.max(svg_h);
                if cur_x + svg_w > container_w && !cur_items.is_empty() {
                    let line_h = max_font_size * bx.style.line_height.max(1.0);
                    lines.push(LineBox {
                        frame: Frame {
                            x,
                            y: cur_y,
                            width: container_w,
                            height: line_h,
                        },
                        items: std::mem::take(&mut cur_items),
                    });
                    cur_y += line_h;
                    cur_x = 0.0;
                    max_font_size = svg_h;
                }
                cur_items.push(LineItem::Svg {
                    frame: Frame {
                        x: x + cur_x,
                        y: 0.0,
                        width: svg_w,
                        height: svg_h,
                    },
                    entry: entry.clone(),
                    node: child.node,
                    vertical_align: child.style.vertical_align,
                });
                cur_x += svg_w;
            }
            BoxKind::InlineControl(entry) => {
                let cv = &child.style;
                // Padding + border drawn around the label text. We resolve
                // them once here so paint can rebuild the same rect.
                let pad = resolve_edges(&cv.padding, cv.font_size, container_w);
                let border = resolve_edges(&cv.border, cv.font_size, container_w);
                let label_w = font.measure_text(&entry.label, cv.font_size);
                let em = font.glyph_advance('M', cv.font_size).max(cv.font_size * 0.5);
                // <input> default size ≈ 20 'M's when there's no label;
                // <button> shrink-to-fit. An empty button gets a small
                // floor so it's still visible.
                let intrinsic_w = match entry.kind {
                    ControlKind::Input => label_w.max(em * 20.0),
                    ControlKind::Button => label_w.max(em * 2.0),
                    // Checkboxes / radios are square indicators sized
                    // to roughly the cap-height of the surrounding text.
                    ControlKind::Checkbox { .. } | ControlKind::Radio { .. } => cv.font_size,
                };
                let is_indicator = matches!(
                    entry.kind,
                    ControlKind::Checkbox { .. } | ControlKind::Radio { .. },
                );
                // Author CSS `width` wins over the intrinsic default.
                // Without this, `input { width: 100% }` (Wikipedia's
                // search box uses `.cdx-text-input__input { width: 100% }`)
                // had no effect — the input rendered at the 20-em
                // default, ignoring its container.
                let css_w = match cv.width {
                    Dimension::Length(l) => Some(l.resolve(cv.font_size, 16.0, container_w).max(0.0)),
                    Dimension::Auto => None,
                };
                let mut ctrl_w = if is_indicator {
                    intrinsic_w
                } else if let Some(w) = css_w {
                    w
                } else {
                    intrinsic_w + pad.left + pad.right + border.left + border.right
                };
                // Apply min-width / max-width clamps. Wikipedia's
                // `.cdx-text-input { min-width: 256px }` for example
                // forces the search input to be at least 256 wide
                // even when its CSS width is auto and 20-em default
                // would have been smaller.
                if let Some(mn) = cv.min_width {
                    let raw = mn.resolve(cv.font_size, 16.0, container_w).max(0.0);
                    if ctrl_w < raw {
                        ctrl_w = raw;
                    }
                }
                if let Some(mx) = cv.max_width {
                    let raw = mx.resolve(cv.font_size, 16.0, container_w).max(0.0);
                    if ctrl_w > raw {
                        ctrl_w = raw;
                    }
                }
                let ctrl_h = if is_indicator {
                    cv.font_size
                } else {
                    cv.font_size + pad.top + pad.bottom + border.top + border.bottom
                };
                max_font_size = max_font_size.max(ctrl_h);
                if cur_x + ctrl_w > container_w && !cur_items.is_empty() {
                    let line_h = max_font_size * cv.line_height.max(1.0);
                    lines.push(LineBox {
                        frame: Frame {
                            x,
                            y: cur_y,
                            width: container_w,
                            height: line_h,
                        },
                        items: std::mem::take(&mut cur_items),
                    });
                    cur_y += line_h;
                    cur_x = 0.0;
                    max_font_size = ctrl_h;
                }
                cur_items.push(LineItem::Control {
                    frame: Frame {
                        x: x + cur_x,
                        y: 0.0,
                        width: ctrl_w,
                        height: ctrl_h,
                    },
                    label: entry.label.clone(),
                    kind: entry.kind,
                    is_placeholder: entry.is_placeholder,
                    style: cv.clone(),
                    node: child.node,
                });
                cur_x += ctrl_w;
            }
            BoxKind::InlineBreak => {
                // Commit the current line (even if empty) and start a
                // new one. We track at least the current font_size so
                // the empty line still has a sensible height.
                let line_h =
                    max_font_size.max(child.style.font_size) * child.style.line_height.max(1.0);
                lines.push(LineBox {
                    frame: Frame {
                        x,
                        y: cur_y,
                        width: container_w,
                        height: line_h,
                    },
                    items: std::mem::take(&mut cur_items),
                });
                cur_y += line_h;
                cur_x = 0.0;
                max_font_size = child.style.font_size;
            }
            BoxKind::InlineBlockHost(mut host_box) => {
                // Shrink-to-fit: explicit `width` wins, otherwise we
                // walk the subtree once to compute its preferred (no-
                // wrap) max-content width and clamp to what's left
                // on the line.
                let cv = child.style.clone();
                let avail = (container_w - cur_x).max(0.0);
                let pad = resolve_edges(&cv.padding, cv.font_size, container_w);
                let border = resolve_edges(&cv.border, cv.font_size, container_w);
                let pb_w = pad.left + pad.right + border.left + border.right;
                let target_w = match cv.width {
                    Dimension::Length(l) => l.resolve(cv.font_size, 16.0, container_w),
                    Dimension::Auto => {
                        let intrinsic = intrinsic_max_width(&host_box) + pb_w;
                        intrinsic.min(container_w).max(0.0)
                    }
                };
                let target_w = if target_w > avail && cur_x > 0.0 && target_w <= container_w {
                    // Doesn't fit on this line — wrap.
                    let line_h = max_font_size * cv.line_height.max(1.0);
                    lines.push(LineBox {
                        frame: Frame {
                            x,
                            y: cur_y,
                            width: container_w,
                            height: line_h,
                        },
                        items: std::mem::take(&mut cur_items),
                    });
                    cur_y += line_h;
                    cur_x = 0.0;
                    max_font_size = cv.font_size;
                    target_w
                } else {
                    target_w
                };
                layout_block(&mut host_box, x + cur_x, 0.0, target_w, None);
                let h = host_box.frame.height;
                let w = host_box.frame.width;
                max_font_size = max_font_size.max(h);
                cur_items.push(LineItem::InlineBlock {
                    frame: Frame {
                        x: x + cur_x,
                        y: 0.0,
                        width: w,
                        height: h,
                    },
                    host: host_box,
                    node: child.node,
                    vertical_align: cv.vertical_align,
                });
                cur_x += w.max(target_w);
            }
            _ => continue,
        }
    }

    if !cur_items.is_empty() {
        // Honour the container's line-height for the final (unwrapped)
        // line. Previously hardcoded to 1.2 — that gave h1's single
        // line a 1.2 multiplier even when the cascade said 1.38,
        // making the heading 5 px shorter than the spec implies.
        let lh = bx.style.line_height.max(1.0);
        let line_h = max_font_size * lh;
        lines.push(LineBox {
            frame: Frame {
                x,
                y: cur_y,
                width: container_w,
                height: line_h,
            },
            items: std::mem::take(&mut cur_items),
        });
        cur_y += line_h;
    }

    // Place items vertically within their line. Text gets a typographic
    // baseline; replaced inline boxes (image / svg / control) align
    // bottom-to-baseline by default, with `vertical-align` shifting
    // them by half their height for `middle`, snapping them to the
    // line top/bottom for `top`/`bottom`, etc.
    for line in &mut lines {
        let baseline = line.frame.y + line.frame.height * 0.8;
        let line_top = line.frame.y;
        let line_bottom = line.frame.y + line.frame.height;
        for item in &mut line.items {
            match item {
                LineItem::Text(run) => {
                    let default_y = baseline - run.style.font_size * 0.8;
                    run.frame.y = match run.style.vertical_align {
                        bui_style::VerticalAlign::Baseline => default_y,
                        bui_style::VerticalAlign::Top | bui_style::VerticalAlign::TextTop => {
                            line_top
                        }
                        bui_style::VerticalAlign::Bottom
                        | bui_style::VerticalAlign::TextBottom => {
                            line_bottom - run.style.font_size
                        }
                        bui_style::VerticalAlign::Middle => {
                            (line_top + line_bottom) * 0.5 - run.style.font_size * 0.5
                        }
                        bui_style::VerticalAlign::Sub => default_y + run.style.font_size * 0.2,
                        bui_style::VerticalAlign::Super => default_y - run.style.font_size * 0.4,
                    };
                }
                LineItem::Image { frame, vertical_align, .. } => {
                    frame.y = vertical_align_y(*vertical_align, baseline, line_top, line_bottom, frame.height);
                }
                LineItem::Control { frame, .. } => {
                    frame.y = baseline - frame.height;
                }
                LineItem::Svg { frame, vertical_align, .. } => {
                    frame.y = vertical_align_y(*vertical_align, baseline, line_top, line_bottom, frame.height);
                }
                LineItem::InlineBlock { frame, host, vertical_align, .. } => {
                    let new_y = vertical_align_y(
                        *vertical_align,
                        baseline,
                        line_top,
                        line_bottom,
                        frame.height,
                    );
                    let dy = new_y - frame.y;
                    frame.y = new_y;
                    // The host was laid out at y=0 — shift its full
                    // subtree to the resolved line position.
                    shift_subtree(host, 0.0, new_y);
                    let _ = dy;
                }
            }
        }
    }

    // Horizontal alignment within each line. The anonymous block's
    // style was inherited from its parent, so `text_align` here is the
    // *containing* block's value — exactly what CSS uses to align
    // inline content.
    let align = bx.style.text_align;
    if !matches!(align, bui_style::TextAlign::Left | bui_style::TextAlign::Justify) {
        for line in &mut lines {
            // Used width = right edge of the rightmost item minus the
            // line's left edge. Items have absolute coordinates already,
            // so we compute extents in absolute space and shift.
            let line_left = line.frame.x;
            let line_right = line.frame.x + line.frame.width;
            let mut used_right = line_left;
            for item in &line.items {
                let r = match item {
                    LineItem::Text(run) => run.frame.x + run.frame.width,
                    LineItem::Image { frame, .. } => frame.x + frame.width,
                    LineItem::Control { frame, .. } => frame.x + frame.width,
                    LineItem::Svg { frame, .. } => frame.x + frame.width,
                    LineItem::InlineBlock { frame, .. } => frame.x + frame.width,
                };
                if r > used_right {
                    used_right = r;
                }
            }
            let leftover = (line_right - used_right).max(0.0);
            let dx = match align {
                bui_style::TextAlign::Center => leftover * 0.5,
                bui_style::TextAlign::Right => leftover,
                _ => 0.0,
            };
            if dx > 0.0 {
                for item in &mut line.items {
                    match item {
                        LineItem::Text(run) => run.frame.x += dx,
                        LineItem::Image { frame, .. } => frame.x += dx,
                        LineItem::Control { frame, .. } => frame.x += dx,
                        LineItem::Svg { frame, .. } => frame.x += dx,
                        LineItem::InlineBlock { frame, host, .. } => {
                            frame.x += dx;
                            shift_subtree(host, dx, 0.0);
                        }
                    }
                }
            }
        }
    }

    let total_h: f32 = lines.iter().map(|l| l.frame.height).sum();
    bx.frame = Frame {
        x,
        y,
        width: container_w,
        height: total_h,
    };
    // text-overflow: ellipsis — only meaningful when the container
    // both clips overflowing content and forbids wrapping, which is
    // the canonical "truncated single-line label" recipe.
    let wants_ellipsis = matches!(bx.style.text_overflow, bui_style::TextOverflow::Ellipsis)
        && matches!(
            bx.style.white_space,
            bui_style::WhiteSpace::Nowrap | bui_style::WhiteSpace::Pre,
        )
        && !matches!(bx.style.overflow_x, bui_style::Overflow::Visible);
    if wants_ellipsis {
        truncate_lines_with_ellipsis(&mut lines, container_w);
    }
    // line-clamp: drop lines past the cap and ellipsize the last
    // surviving line. Common pattern for card descriptions and nav
    // titles. Spec strictly requires `overflow: hidden`, but most
    // authors set it together; we apply whenever line_clamp > 0.
    if let Some(max_lines) = bx.style.line_clamp {
        let max = max_lines as usize;
        if max > 0 && lines.len() > max {
            lines.truncate(max);
            // Force an ellipsis suffix even when the surviving line's
            // own width fits — line-clamp's signal is "more was
            // truncated below", not a horizontal overflow.
            append_ellipsis_to_last_line(&mut lines);
        }
    }
    bx.lines = lines;
}

/// Append "…" to the rightmost text run of the last line. Used by
/// line-clamp to signal "content was dropped below". If the last
/// line's last run already ends with "…", we don't duplicate.
fn append_ellipsis_to_last_line(lines: &mut Vec<LineBox>) {
    let font = bui_text::shared_font();
    let Some(line) = lines.last_mut() else {
        return;
    };
    for item in line.items.iter_mut().rev() {
        if let LineItem::Text(run) = item {
            if !run.text.ends_with('\u{2026}') {
                run.text.push('\u{2026}');
                run.frame.width = font.measure_text_with_spacing(
                    &run.text,
                    run.style.font_size,
                    run.style.letter_spacing,
                );
            }
            return;
        }
    }
}

/// Walk lines and, where the rightmost item extends past `container_w`,
/// trim text runs character-by-character and append "…" until the run
/// fits. We don't re-shape — the appended ellipsis uses the run's own
/// font_size, so widths stay close enough at typical UI sizes.
fn truncate_lines_with_ellipsis(lines: &mut Vec<LineBox>, container_w: f32) {
    let font = bui_text::shared_font();
    for line in lines.iter_mut() {
        // line.frame.x is absolute; container_w is relative (the
        // box's content width). Mixing them was a bug — comparing a
        // run's absolute right edge against a relative width
        // mis-flagged every line as overflowing and produced max_w =
        // container_w - run.frame.x = a huge negative, which then
        // popped every character and left only "…".
        let line_left = line.frame.x;
        let line_right_max = line_left + container_w;
        let line_right = line
            .items
            .iter()
            .map(item_right)
            .fold(line_left, f32::max);
        if line_right <= line_right_max + 0.5 {
            continue;
        }
        // Find the last text run that still starts inside the box;
        // truncate its text + append ellipsis.
        for item in line.items.iter_mut().rev() {
            if let LineItem::Text(run) = item {
                let ellipsis = "\u{2026}";
                let ell_w = font.measure_text_with_spacing(
                    ellipsis,
                    run.style.font_size,
                    run.style.letter_spacing,
                );
                let max_w = (line_right_max - run.frame.x - ell_w).max(0.0);
                // Longest prefix that fits, via a running width — text
                // width is a plain per-char advance sum (no kerning),
                // so prefixes are additive. The old pop-one-char-and-
                // re-measure loop was O(n²) on long overflowing runs.
                let mut width = 0.0;
                let mut keep_bytes = 0;
                for (i, c) in run.text.char_indices() {
                    let adv = font.glyph_advance(c, run.style.font_size)
                        + run.style.letter_spacing;
                    if width + adv > max_w {
                        break;
                    }
                    width += adv;
                    keep_bytes = i + c.len_utf8();
                }
                let mut truncated = run.text[..keep_bytes].to_string();
                truncated.push_str(ellipsis);
                run.text = truncated;
                run.frame.width = width + ell_w;
                break;
            }
        }
    }
}

/// Split `text` into successive char chunks where each chunk's
/// rendered width fits inside `container_w`, accounting for the
/// already-occupied portion of the first line via `start_x`. Used by
/// `word-break: break-all` / `overflow-wrap: break-word`. Always
/// produces at least one non-empty chunk.
fn break_word_chunks(
    text: &str,
    font: &bui_text::Font,
    font_size: f32,
    letter_spacing: f32,
    container_w: f32,
    start_x: f32,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    // Running chunk width instead of re-measuring the whole prefix per
    // char (which made long unbroken words O(n²)). Exact: width is a
    // per-char advance sum, so it accumulates.
    let mut current_w = 0.0;
    let mut budget = (container_w - start_x).max(font_size); // never zero — guarantees progress
    for c in text.chars() {
        let adv = font.glyph_advance(c, font_size) + letter_spacing;
        if current_w + adv > budget && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            budget = container_w.max(font_size);
            current.push(c);
            current_w = adv;
        } else {
            current.push(c);
            current_w += adv;
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(text.to_string());
    }
    out
}

fn item_right(item: &LineItem) -> f32 {
    match item {
        LineItem::Text(run) => run.frame.x + run.frame.width,
        LineItem::Image { frame, .. } => frame.x + frame.width,
        LineItem::Control { frame, .. } => frame.x + frame.width,
        LineItem::Svg { frame, .. } => frame.x + frame.width,
        LineItem::InlineBlock { frame, .. } => frame.x + frame.width,
    }
}

fn resolve_edges(e: &EdgeSizes, font_size: f32, basis: f32) -> ResolvedEdges {
    ResolvedEdges {
        top: resolve_length(e.top, font_size, basis),
        right: resolve_length(e.right, font_size, basis),
        bottom: resolve_length(e.bottom, font_size, basis),
        left: resolve_length(e.left, font_size, basis),
    }
}

fn resolve_length(l: Length, font_size: f32, basis: f32) -> f32 {
    l.resolve(font_size, 16.0, basis)
}

struct ResolvedEdges {
    top: f32,
    right: f32,
    bottom: f32,
    left: f32,
}

/// Walk the laid-out box tree and emit paint commands.
pub fn paint(root: &LayoutBox, out: &mut DisplayList) {
    paint_box(root, out, 1.0);
}

/// Find the deepest layout box whose frame contains `(x, y)`. Walks line
/// boxes too so clicks on text-runs return the originating element node.
/// Returns the *DOM* `NodeId` associated with the hit box, if any.
pub fn hit_test(root: &LayoutBox, x: f32, y: f32) -> Option<NodeId> {
    // pointer-events: none — this box is invisible to hit testing.
    // We still recurse since a descendant may re-declare `auto`, but
    // the box itself never wins the hit.
    let blocks_self = matches!(root.style.pointer_events, bui_style::PointerEvents::None);
    if !contains(&root.frame, x, y) {
        // Children may extend beyond root if margin collapsing went weird;
        // fall through and try them anyway.
    }
    // Depth-first, last-child-first so visually-on-top boxes win.
    for child in root.children.iter().rev() {
        if let Some(hit) = hit_test(child, x, y) {
            return Some(hit);
        }
    }
    // Inline runs in this box (only set on anonymous block boxes).
    for line in root.lines.iter().rev() {
        if !contains(&line.frame, x, y) {
            continue;
        }
        for item in line.items.iter().rev() {
            // Inline-blocks have their own subtree to walk first so a
            // click lands on the deepest matching descendant rather
            // than the host's outer rect.
            if let LineItem::InlineBlock { host, .. } = item {
                if let Some(hit) = hit_test(host, x, y) {
                    return Some(hit);
                }
            }
            let (frame, node) = match item {
                LineItem::Text(run) => (&run.frame, run.node),
                LineItem::Image { frame, node, .. } => (frame, *node),
                LineItem::Control { frame, node, .. } => (frame, *node),
                LineItem::Svg { frame, node, .. } => (frame, *node),
                LineItem::InlineBlock { frame, node, .. } => (frame, *node),
            };
            if !blocks_self && contains(frame, x, y) {
                // Return the originating DOM node so the caller can walk
                // up to the nearest <a href> via `enclosing_anchor`.
                if let Some(id) = node {
                    return Some(id);
                }
                if let Some(id) = root.node {
                    return Some(id);
                }
            }
        }
    }
    if !blocks_self && contains(&root.frame, x, y) {
        return root.node;
    }
    None
}

fn contains(frame: &Frame, x: f32, y: f32) -> bool {
    x >= frame.x
        && x < frame.x + frame.width
        && y >= frame.y
        && y < frame.y + frame.height
}

/// Walk up the DOM from `node`, looking for the nearest `<a href>` ancestor.
/// Returns `(anchor_node_id, href_value)` if found.
pub fn enclosing_anchor<'a>(
    doc: &'a bui_dom::Document,
    node: NodeId,
) -> Option<(NodeId, &'a str)> {
    let mut cur = Some(node);
    while let Some(id) = cur {
        if let Some(elem) = doc.element(id) {
            if elem.name == "a" {
                if let Some(href) = elem.get_attr("href") {
                    return Some((id, href));
                }
            }
        }
        cur = doc.node(id).parent;
    }
    None
}

fn paint_box(bx: &LayoutBox, out: &mut DisplayList, parent_alpha: f32) {
    let alpha = (parent_alpha * bx.style.opacity).clamp(0.0, 1.0);
    if alpha <= 0.0 {
        // Fully transparent — no point recursing into children.
        return;
    }
    // `position: sticky` is resolved at paint time by the host's
    // scroll-shift loop. We bracket this box's commands with a
    // PushStickyGroup / PopStickyGroup so the host can apply a
    // per-group clamp ("scroll normally until you'd cross `top_edge`,
    // then pin"). `range_bottom` defaults to a sentinel large value;
    // for body-level stickies this never trips, which matches the
    // common case (Wikipedia's vector-header).
    let is_sticky = matches!(bx.style.position, bui_style::Position::Sticky);
    if is_sticky {
        let top_edge = bx
            .style
            .top
            .map(|t| resolve_length(t, bx.style.font_size, bx.frame.height.max(1.0)))
            .unwrap_or(0.0);
        out.commands.push(PaintCommand::PushStickyGroup {
            natural_y: bx.frame.y,
            top_edge,
            range_bottom: f32::INFINITY,
        });
    }
    // visibility: hidden / collapse — skip self-paint but still
    // recurse so a descendant that re-declares `visibility: visible`
    // can paint normally. CSS inheritance does most of the work
    // (descendants inherit hidden by default), but the override case
    // is real and shows up on accessibility-skip-link patterns.
    if matches!(
        bx.style.visibility,
        bui_style::Visibility::Hidden | bui_style::Visibility::Collapse,
    ) {
        for child in &bx.children {
            paint_box(child, out, alpha);
        }
        return;
    }
    let pc = |c: RgbaColor| paint_color_alpha(c, alpha);
    // Drop shadow. Drawn first so the box itself paints over it. We
    // expand the rect by the spread, offset by (dx, dy), and use the
    // average corner radius — vello's `draw_blurred_rounded_rect`
    // takes a single corner radius. Multi-shadow CSS keeps only the
    // first shadow at parse time.
    // box-shadow allows multiple stacked shadows; CSS paints them
    // top-to-bottom in declaration order. Reverse our list once so
    // `out.box_shadow` calls happen back-to-front and the first
    // declared shadow ends up on top of the rest.
    let radii = resolve_border_radii(&bx.style, bx.frame.width, bx.frame.height);
    let avg_radius = (radii[0] + radii[1] + radii[2] + radii[3]) * 0.25;
    for shadow in bx.style.box_shadows.iter().rev() {
        let dx = resolve_length(shadow.offset_x, bx.style.font_size, bx.frame.width);
        let dy = resolve_length(shadow.offset_y, bx.style.font_size, bx.frame.height);
        let blur = resolve_length(shadow.blur, bx.style.font_size, bx.frame.width).max(0.0);
        let spread = resolve_length(shadow.spread, bx.style.font_size, bx.frame.width);
        let rect = Rect::new(
            bx.frame.x + dx - spread,
            bx.frame.y + dy - spread,
            (bx.frame.width + 2.0 * spread).max(0.0),
            (bx.frame.height + 2.0 * spread).max(0.0),
        );
        if rect.w > 0.0 && rect.h > 0.0 && shadow.color.a > 0 {
            out.box_shadow(rect, pc(shadow.color), avg_radius, blur);
        }
    }

    // Background + border. With border-radius, the four-rect border
    // approach produces sharp corners that visibly stick out past
    // the rounded background (Google's `.RNNXgb { border-radius:
    // 26px; border: 1px solid #dadce0 }` looked like a rounded pill
    // with four pointy lines around it). When ANY corner has a
    // radius > 0, paint the border as an outer rounded fill and
    // inset the background on top — same visual as a stroked
    // rounded rect, no Stroke primitive needed.
    let radii = resolve_border_radii(&bx.style, bx.frame.width, bx.frame.height);
    let has_radius = radii.iter().any(|r| *r > 0.0);
    let border = resolve_edges(&bx.style.border, bx.style.font_size, bx.frame.width);
    let bg = bx.style.background_color;
    let bc = bx.style.border_color;
    let any_border = bc.a > 0
        && (border.top > 0.0 || border.bottom > 0.0 || border.left > 0.0 || border.right > 0.0);
    if has_radius && any_border {
        // Outer ring in border color.
        out.fill_rounded_rect(
            Rect::new(bx.frame.x, bx.frame.y, bx.frame.width, bx.frame.height),
            pc(bc),
            radii,
        );
        // Inset rect for the background. Use the smallest of the
        // four border widths as the inset on each side; for the
        // common uniform-border case (1px all around) this matches
        // exactly. Asymmetric borders fall back to the per-side
        // inset.
        let inner_x = bx.frame.x + border.left;
        let inner_y = bx.frame.y + border.top;
        let inner_w = (bx.frame.width - border.left - border.right).max(0.0);
        let inner_h = (bx.frame.height - border.top - border.bottom).max(0.0);
        // Inner radii shrink by the average border thickness so
        // the corner stays concentric. Clamp to non-negative.
        let avg_border = (border.left + border.right + border.top + border.bottom) * 0.25;
        let inner_radii = [
            (radii[0] - avg_border).max(0.0),
            (radii[1] - avg_border).max(0.0),
            (radii[2] - avg_border).max(0.0),
            (radii[3] - avg_border).max(0.0),
        ];
        if bg.a > 0 && inner_w > 0.0 && inner_h > 0.0 {
            if inner_radii.iter().any(|r| *r > 0.0) {
                out.fill_rounded_rect(
                    Rect::new(inner_x, inner_y, inner_w, inner_h),
                    pc(bg),
                    inner_radii,
                );
            } else {
                out.fill_rect(Rect::new(inner_x, inner_y, inner_w, inner_h), pc(bg));
            }
        }
    } else {
        // No border-radius (or no border): keep the existing
        // background-then-four-rects path. Sharp corners look fine
        // when there's no curvature to mismatch.
        if bg.a > 0 {
            let rect = Rect::new(bx.frame.x, bx.frame.y, bx.frame.width, bx.frame.height);
            if has_radius {
                out.fill_rounded_rect(rect, pc(bg), radii);
            } else {
                out.fill_rect(rect, pc(bg));
            }
        }
        if any_border {
            let f = bx.frame;
            if border.top > 0.0 {
                out.fill_rect(Rect::new(f.x, f.y, f.width, border.top), pc(bc));
            }
            if border.bottom > 0.0 {
                out.fill_rect(
                    Rect::new(f.x, f.y + f.height - border.bottom, f.width, border.bottom),
                    pc(bc),
                );
            }
            if border.left > 0.0 {
                out.fill_rect(Rect::new(f.x, f.y, border.left, f.height), pc(bc));
            }
            if border.right > 0.0 {
                out.fill_rect(
                    Rect::new(f.x + f.width - border.right, f.y, border.right, f.height),
                    pc(bc),
                );
            }
        }
    }
    // Background image — placed inside the box's frame according to
    // background-size, background-position, and background-repeat.
    // Default (Auto + Anchor(0,0) + Repeat) without intrinsic-size
    // info still renders stretched-to-frame; anything more specific
    // routes through compute_bg_paint to derive the right rect.
    if let Some(key) = &bx.style.background_image {
        if bx.frame.width > 0.0 && bx.frame.height > 0.0 {
            paint_background_image(out, bx, key);
        }
    }

    // Outline. Drawn outside the border-box, optionally pushed
    // further out by `outline-offset`. Outline doesn't take part in
    // layout, so its geometry is purely visual.
    if let Some(ow) = bx.style.outline_width {
        let w = resolve_length(ow, bx.style.font_size, bx.frame.width);
        let off = resolve_length(bx.style.outline_offset, bx.style.font_size, bx.frame.width);
        let oc = bx.style.outline_color;
        if w > 0.0 && oc.a > 0 {
            // Inflate the rect by `off`; outline rings the inflated rect.
            let f = Frame {
                x: bx.frame.x - off,
                y: bx.frame.y - off,
                width: bx.frame.width + 2.0 * off,
                height: bx.frame.height + 2.0 * off,
            };
            // Top edge.
            out.fill_rect(Rect::new(f.x - w, f.y - w, f.width + 2.0 * w, w), pc(oc));
            // Bottom edge.
            out.fill_rect(
                Rect::new(f.x - w, f.y + f.height, f.width + 2.0 * w, w),
                pc(oc),
            );
            // Left edge.
            out.fill_rect(Rect::new(f.x - w, f.y, w, f.height), pc(oc));
            // Right edge.
            out.fill_rect(Rect::new(f.x + f.width, f.y, w, f.height), pc(oc));
        }
    }

    // List-item marker. Drawn in the parent list's padding-left strip,
    // baseline-aligned with the first line of content (≈ 0.95 of
    // font_size below the box's top edge, matching the empirical
    // baseline our text runs use).
    if let Some(label) = &bx.list_marker {
        let font_size = bx.style.font_size;
        let font = bui_text::shared_font();
        let label_w = font.measure_text(label, font_size);
        let marker_x = bx.frame.x - label_w - 6.0;
        let baseline = bx.frame.y + font_size * 0.95;
        out.text(
            marker_x,
            baseline,
            label_w,
            font_size,
            pc(bx.style.color),
            label.clone(),
        );
    }

    // Lines (inline content of an anonymous block).
    for line in &bx.lines {
        for item in &line.items {
            match item {
                LineItem::Text(run) => {
                    let baseline = run.frame.y + run.style.font_size * 0.8;
                    // Paint a single shadow layer FIRST, behind the
                    // main text, when the author specified
                    // `text-shadow`. We don't model blur — the offset
                    // alone reproduces the typographic "lifted" feel
                    // most authors are after at button / heading sizes.
                    if let Some(sh) = run.style.text_shadow {
                        let dx = sh.offset_x.resolve(run.style.font_size, 16.0, run.frame.width);
                        let dy = sh.offset_y.resolve(run.style.font_size, 16.0, run.frame.width);
                        out.text(
                            run.frame.x + dx,
                            baseline + dy,
                            run.frame.width,
                            run.style.font_size,
                            pc(sh.color),
                            run.text.clone(),
                        );
                    }
                    out.text(
                        run.frame.x,
                        baseline,
                        run.frame.width,
                        run.style.font_size,
                        pc(run.style.color),
                        run.text.clone(),
                    );
                    if run.style.text_decoration_underline {
                        // Draw the line one pixel below the baseline, at
                        // a thickness scaled to the font size — matches
                        // the convention browsers use for underlines.
                        // text-decoration-color overrides the default
                        // (which is the text colour itself).
                        let thickness = (run.style.font_size * 0.07).max(1.0);
                        let y = baseline + 1.0;
                        let underline_color = run
                            .style
                            .text_decoration_color
                            .unwrap_or(run.style.color);
                        out.fill_rect(
                            Rect::new(run.frame.x, y, run.frame.width, thickness),
                            pc(underline_color),
                        );
                    }
                    if run.style.text_decoration_line_through {
                        // Mid-x-height stroke through the run. Real
                        // browsers anchor at half the cap-height; we
                        // don't have per-font cap metrics, so we
                        // approximate at 0.4 * font_size above the
                        // baseline — visually centred for typical sans.
                        let thickness = (run.style.font_size * 0.07).max(1.0);
                        let y = baseline - run.style.font_size * 0.32;
                        let strike_color = run
                            .style
                            .text_decoration_color
                            .unwrap_or(run.style.color);
                        out.fill_rect(
                            Rect::new(run.frame.x, y, run.frame.width, thickness),
                            pc(strike_color),
                        );
                    }
                }
                LineItem::Image { frame, key, intrinsic, object_fit, .. } => {
                    let (paint_rect, needs_clip) =
                        compute_object_fit_rect(*frame, *intrinsic, *object_fit);
                    if needs_clip {
                        out.commands.push(PaintCommand::PushClip {
                            rect: Rect::new(frame.x, frame.y, frame.width, frame.height),
                            radii: [0.0; 4],
                        });
                    }
                    out.image(
                        Rect::new(paint_rect.x, paint_rect.y, paint_rect.width, paint_rect.height),
                        key.clone(),
                    );
                    if needs_clip {
                        out.commands.push(PaintCommand::PopClip);
                    }
                }
                LineItem::Control {
                    frame,
                    label,
                    style,
                    kind,
                    is_placeholder,
                    ..
                } => {
                    paint_control(out, frame, label, style, *kind, *is_placeholder);
                }
                LineItem::Svg { frame, entry, .. } => {
                    paint_svg(out, frame, entry);
                }
                LineItem::InlineBlock { host, .. } => {
                    // The host has been laid out at its final
                    // absolute position; paint walks it like any
                    // other block subtree.
                    paint_box(host, out, alpha);
                }
            }
        }
    }

    // `overflow: hidden | scroll | auto | clip` clips children to the
    // box's rounded border-rect. We emit one Push/Pop pair around the
    // child walk so the box itself (background, border, shadow) stays
    // visible — only descendants get cropped. Visible on either axis
    // means no clip.
    let clipped = !matches!(bx.style.overflow_x, bui_style::Overflow::Visible)
        || !matches!(bx.style.overflow_y, bui_style::Overflow::Visible);
    if clipped && !bx.children.is_empty() {
        let radii = resolve_border_radii(&bx.style, bx.frame.width, bx.frame.height);
        out.push_clip(
            Rect::new(bx.frame.x, bx.frame.y, bx.frame.width, bx.frame.height),
            radii,
        );
    }
    // Stacking-context order: children paint in source order with
    // their z-index acting as a stable secondary sort. `auto`
    // (None) maps to 0 so positioned children with no explicit
    // z-index keep their source order. Stable sort means
    // equal-z items still paint in source order.
    let mut order: Vec<usize> = (0..bx.children.len()).collect();
    let any_z = bx
        .children
        .iter()
        .any(|c| c.style.z_index.is_some());
    if any_z {
        order.sort_by(|a, b| {
            let za = bx.children[*a].style.z_index.unwrap_or(0);
            let zb = bx.children[*b].style.z_index.unwrap_or(0);
            za.cmp(&zb)
        });
    }
    for &i in &order {
        paint_box(&bx.children[i], out, alpha);
    }
    if clipped && !bx.children.is_empty() {
        out.pop_clip();
    }
    if is_sticky {
        out.commands.push(PaintCommand::PopStickyGroup);
    }
}

fn paint_color(c: RgbaColor) -> Color {
    Color::rgba(c.r, c.g, c.b, c.a)
}

/// Clamp `y` upward so it sits below every float in `same_side` —
/// used by `clear` and by the after-loop "extend the parent past
/// floats" pass.
fn float_clear_y(y: f32, same_side: &[Frame]) -> f32 {
    let mut out = y;
    for f in same_side {
        let bot = f.y + f.height;
        if bot > out {
            out = bot;
        }
    }
    out
}

/// Compute the available x-range for a non-float sibling positioned
/// at `cursor_y`. Left-side floats push `avail_x` to the right;
/// right-side floats clip `avail_w` from the right.
fn available_at_y(
    content_x: f32,
    content_w: f32,
    cursor_y: f32,
    left_floats: &[Frame],
    right_floats: &[Frame],
) -> (f32, f32) {
    let mut left_intrusion: f32 = 0.0;
    for f in left_floats {
        if f.y < cursor_y + 1.0 && f.y + f.height > cursor_y {
            let intrude = (f.x + f.width) - content_x;
            if intrude > left_intrusion {
                left_intrusion = intrude;
            }
        }
    }
    let mut right_intrusion: f32 = 0.0;
    for f in right_floats {
        if f.y < cursor_y + 1.0 && f.y + f.height > cursor_y {
            let intrude = (content_x + content_w) - f.x;
            if intrude > right_intrusion {
                right_intrusion = intrude;
            }
        }
    }
    let avail_x = content_x + left_intrusion;
    let avail_w = (content_w - left_intrusion - right_intrusion).max(0.0);
    (avail_x, avail_w)
}

/// Walk a subtree and return its preferred (no-wrap) max-content
/// width — equivalent to laying out at infinite width and measuring
/// the resulting extent. Used by `display: inline-block` shrink-to-
/// fit when no explicit `width` is set.
fn intrinsic_max_width(b: &LayoutBox) -> f32 {
    if let Dimension::Length(l) = b.style.width {
        return l.resolve(b.style.font_size, 16.0, 0.0);
    }
    // Sum of inline-axis content. For block containers, take the
    // max child preferred width; for an Anonymous (inline content
    // container) sum the children since they flow side-by-side.
    let font = bui_text::shared_font();
    match &b.kind {
        BoxKind::InlineText(t) => font.measure_text_with_spacing(
            t,
            b.style.font_size,
            b.style.letter_spacing,
        ),
        BoxKind::InlineImage(e) => e.width,
        BoxKind::InlineSvg(e) => e.width,
        BoxKind::InlineControl(e) => {
            let lw = font.measure_text(&e.label, b.style.font_size);
            let pad = resolve_edges(&b.style.padding, b.style.font_size, 0.0);
            let bw = resolve_edges(&b.style.border, b.style.font_size, 0.0);
            lw + pad.left + pad.right + bw.left + bw.right
        }
        BoxKind::InlineBlockHost(host) => intrinsic_max_width(host),
        BoxKind::InlineBreak => 0.0,
        BoxKind::Anonymous => b
            .children
            .iter()
            .map(intrinsic_max_width)
            .sum::<f32>(),
        BoxKind::Block => {
            let pad = resolve_edges(&b.style.padding, b.style.font_size, 0.0);
            let bw = resolve_edges(&b.style.border, b.style.font_size, 0.0);
            let max_child = b
                .children
                .iter()
                .map(intrinsic_max_width)
                .fold(0.0_f32, f32::max);
            max_child + pad.left + pad.right + bw.left + bw.right
        }
    }
}

/// Resolve a `vertical-align` keyword for a replaced inline element
/// (image / svg / control) of given `height`. Default behaviour
/// (`baseline`) is to place the element so its bottom edge sits on
/// the line's baseline — matches the existing pre-`vertical-align`
/// rendering exactly.
fn vertical_align_y(
    va: bui_style::VerticalAlign,
    baseline: f32,
    line_top: f32,
    line_bottom: f32,
    height: f32,
) -> f32 {
    match va {
        bui_style::VerticalAlign::Baseline => baseline - height,
        bui_style::VerticalAlign::Top | bui_style::VerticalAlign::TextTop => line_top,
        bui_style::VerticalAlign::Bottom | bui_style::VerticalAlign::TextBottom => {
            line_bottom - height
        }
        bui_style::VerticalAlign::Middle => (line_top + line_bottom) * 0.5 - height * 0.5,
        bui_style::VerticalAlign::Sub => baseline - height + (height * 0.2),
        bui_style::VerticalAlign::Super => baseline - height - (height * 0.4),
    }
}

/// Apply CSS `text-transform` to a fragment of source text. We
/// uppercase / lowercase via ASCII tables (good enough for the bulk
/// of Wikipedia / Google English content); `capitalize` titlecases
/// the first character of every word.
fn apply_text_transform(text: &str, t: bui_style::TextTransform) -> String {
    match t {
        bui_style::TextTransform::None => text.to_string(),
        bui_style::TextTransform::Uppercase => text.to_uppercase(),
        bui_style::TextTransform::Lowercase => text.to_lowercase(),
        bui_style::TextTransform::Capitalize => {
            let mut out = String::with_capacity(text.len());
            let mut at_word_start = true;
            for c in text.chars() {
                if c.is_whitespace() {
                    out.push(c);
                    at_word_start = true;
                } else if at_word_start {
                    for u in c.to_uppercase() {
                        out.push(u);
                    }
                    at_word_start = false;
                } else {
                    out.push(c);
                }
            }
            out
        }
    }
}

/// Multiply an `RgbaColor`'s alpha channel by a 0..1 multiplier and
/// emit a `paint::Color`. Used to fold `opacity` into per-command
/// alphas without introducing a layer-stack paint command.
fn paint_color_alpha(c: RgbaColor, alpha: f32) -> Color {
    let a = (c.a as f32 * alpha.clamp(0.0, 1.0)).round() as u8;
    Color::rgba(c.r, c.g, c.b, a)
}

/// Emit one `PaintCommand::Svg` per shape inside the entry. The
/// renderer maps the entry's view-box into `frame`, so geometry stays
/// in user-space coords until paint time.
fn paint_svg(out: &mut DisplayList, frame: &Frame, entry: &SvgEntry) {
    let rect = Rect::new(frame.x, frame.y, frame.width, frame.height);
    for shape in &entry.shapes {
        out.svg(
            rect,
            entry.view_box,
            shape.segments.clone(),
            shape.fill,
            shape.stroke,
            shape.stroke_width,
        );
    }
}

/// Resolve `border-radius` to physical pixels and clamp so opposite
/// corners can't overlap (a 100% radius on a 200x40 pill becomes 20).
fn resolve_border_radii(style: &ComputedValues, w: f32, h: f32) -> [f32; 4] {
    let mut r = [
        resolve_length(style.border_radius[0], style.font_size, w),
        resolve_length(style.border_radius[1], style.font_size, w),
        resolve_length(style.border_radius[2], style.font_size, w),
        resolve_length(style.border_radius[3], style.font_size, w),
    ];
    let max_w = w * 0.5;
    let max_h = h * 0.5;
    let cap = max_w.min(max_h).max(0.0);
    for v in r.iter_mut() {
        if *v > cap {
            *v = cap;
        }
        if *v < 0.0 {
            *v = 0.0;
        }
    }
    r
}

/// Paint an `<input>` / `<button>` control: background, single-pixel
/// border, then the label text inside the padding box. The label is
/// clipped by truncation if it overflows; we draw whatever fits.
fn paint_control(
    out: &mut DisplayList,
    frame: &Frame,
    label: &str,
    style: &ComputedValues,
    kind: ControlKind,
    is_placeholder: bool,
) {
    // Indicator inputs (checkbox / radio) draw a small square or
    // circle instead of the text-input chrome. Returning early keeps
    // the code below dedicated to text-bearing controls.
    if let ControlKind::Checkbox { checked } | ControlKind::Radio { checked } = kind {
        let radio = matches!(kind, ControlKind::Radio { .. });
        let pad = (frame.width * 0.12).min(2.0);
        let inner = Rect::new(
            frame.x + pad,
            frame.y + pad,
            (frame.width - pad * 2.0).max(1.0),
            (frame.height - pad * 2.0).max(1.0),
        );
        let radii = if radio {
            [inner.w * 0.5; 4]
        } else {
            [2.0; 4]
        };
        // Background.
        out.commands.push(bui_paint::PaintCommand::FillRoundedRect {
            rect: inner,
            color: paint_color(bui_style::RgbaColor::WHITE),
            radii,
        });
        // 1px border via a slightly inset second rect — cheap stroke
        // approximation that matches the rounded shape.
        let border_color = bui_paint::Color::rgba(120, 120, 120, 255);
        out.commands.push(bui_paint::PaintCommand::FillRoundedRect {
            rect: inner,
            color: border_color,
            radii,
        });
        let inset = 1.0_f32;
        let inside = Rect::new(
            inner.x + inset,
            inner.y + inset,
            (inner.w - inset * 2.0).max(0.0),
            (inner.h - inset * 2.0).max(0.0),
        );
        if inside.w > 0.0 && inside.h > 0.0 {
            out.commands.push(bui_paint::PaintCommand::FillRoundedRect {
                rect: inside,
                color: paint_color(bui_style::RgbaColor::WHITE),
                radii: if radio {
                    [inside.w * 0.5; 4]
                } else {
                    [1.5; 4]
                },
            });
        }
        if checked {
            // Filled centre disk for radios; check glyph for
            // checkboxes (we don't have a vector check, so a centred
            // small square stands in — it reads as "selected" at the
            // sizes a 13–16 px input renders at).
            let mid_pad = (inner.w * 0.25).max(2.0);
            let centre = Rect::new(
                inner.x + mid_pad,
                inner.y + mid_pad,
                (inner.w - mid_pad * 2.0).max(1.0),
                (inner.h - mid_pad * 2.0).max(1.0),
            );
            let fill = bui_paint::Color::rgba(40, 100, 200, 255);
            out.commands.push(bui_paint::PaintCommand::FillRoundedRect {
                rect: centre,
                color: fill,
                radii: if radio {
                    [centre.w * 0.5; 4]
                } else {
                    [1.0; 4]
                },
            });
        }
        return;
    }
    let bg = style.background_color;
    if bg.a > 0 {
        out.fill_rect(
            Rect::new(frame.x, frame.y, frame.width, frame.height),
            paint_color(bg),
        );
    }
    let border = resolve_edges(&style.border, style.font_size, frame.width);
    let bc = style.border_color;
    if bc.a > 0 {
        if border.top > 0.0 {
            out.fill_rect(
                Rect::new(frame.x, frame.y, frame.width, border.top),
                paint_color(bc),
            );
        }
        if border.bottom > 0.0 {
            out.fill_rect(
                Rect::new(
                    frame.x,
                    frame.y + frame.height - border.bottom,
                    frame.width,
                    border.bottom,
                ),
                paint_color(bc),
            );
        }
        if border.left > 0.0 {
            out.fill_rect(
                Rect::new(frame.x, frame.y, border.left, frame.height),
                paint_color(bc),
            );
        }
        if border.right > 0.0 {
            out.fill_rect(
                Rect::new(
                    frame.x + frame.width - border.right,
                    frame.y,
                    border.right,
                    frame.height,
                ),
                paint_color(bc),
            );
        }
    }
    if !label.is_empty() {
        let pad = resolve_edges(&style.padding, style.font_size, frame.width);
        // Place the text baseline ~0.8 of the font_size down from the top
        // of the inner box, matching how text-runs in line-boxes baseline.
        let inner_top = frame.y + border.top + pad.top;
        let baseline = inner_top + style.font_size * 0.8;
        let inner_w = (frame.width - border.left - border.right - pad.left - pad.right).max(0.0);
        // Honour `text-align` on submit-style buttons. CSS default for
        // <button> is center; <input type="submit"> matches the
        // computed value (UA usually injects center-align; author
        // CSS can override). For text inputs we keep left-align.
        let label_w = bui_text::shared_font().measure_text(label, style.font_size);
        let extra = (inner_w - label_w).max(0.0);
        let align_offset = match style.text_align {
            bui_style::TextAlign::Center => extra * 0.5,
            bui_style::TextAlign::Right => extra,
            _ => 0.0,
        };
        let text_x = frame.x + border.left + pad.left + align_offset;
        // Placeholders paint with reduced opacity so the user can
        // visually distinguish hint text from actual entered values
        // (matches `::placeholder` styling in real browsers).
        let text_color = if is_placeholder {
            let c = style.color;
            bui_paint::Color::rgba(c.r, c.g, c.b, c.a.min(140))
        } else {
            paint_color(style.color)
        };
        out.text(
            text_x,
            baseline,
            inner_w,
            style.font_size,
            text_color,
            label.to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bui_css::Stylesheet;
    use bui_dom::Document;

    fn doc_from_html(html: &str) -> (Document, StyleTree) {
        let doc = bui_html::parse(html);
        let sheets = bui_style::extract_inline_stylesheets(&doc);
        let style = bui_style::style_document(&doc, &sheets);
        (doc, style)
    }

    #[test]
    fn block_stack() {
        let (doc, style) = doc_from_html("<body><p>One</p><p>Two</p></body>");
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        // body should have two block children (the two <p>s become anonymous-wrapped blocks).
        let block_children: Vec<&LayoutBox> = bx
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        assert_eq!(block_children.len(), 2);
        // Second p must sit below first p.
        assert!(block_children[1].frame.y > block_children[0].frame.y);
    }

    #[test]
    fn inline_text_wraps() {
        let (doc, style) = doc_from_html(
            "<p>The quick brown fox jumps over the lazy dog and then keeps running across the field</p>",
        );
        let p = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "p").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, p);
        // A 200px container should force wrapping.
        layout(&mut bx, 0.0, 0.0, 200.0);
        let total_lines: usize = bx
            .children
            .iter()
            .map(|c| c.lines.len())
            .sum();
        assert!(total_lines >= 2, "expected wrap, got {total_lines} line(s)");
    }

    #[test]
    fn hit_test_finds_anchor_text() {
        let (doc, style) = doc_from_html(
            "<body><p>before <a href=\"https://example.org/\">Learn more</a> after</p></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        // Locate the rendered "Learn more" run inside the <p>'s anonymous
        // line box and click squarely on it.
        let mut anchor_run: Option<Frame> = None;
        for child in &bx.children {
            for sub in &child.children {
                for line in &sub.lines {
                    for item in &line.items {
                        if let LineItem::Text(run) = item {
                            if run.text.contains("Learn") {
                                anchor_run = Some(run.frame);
                            }
                        }
                    }
                }
            }
        }
        let frame = anchor_run.expect("rendered 'Learn more' text run");
        let cx = frame.x + 4.0;
        let cy = frame.y + frame.height * 0.5;
        let hit = hit_test(&bx, cx, cy).expect("hit something");
        // hit should be the text node *inside* the <a>; enclosing_anchor
        // climbs up and reports the href.
        let (anchor_id, href) = enclosing_anchor(&doc, hit).expect("found anchor");
        assert_eq!(href, "https://example.org/");
        assert_eq!(doc.element(anchor_id).unwrap().name, "a");
    }

    #[test]
    fn enclosing_anchor_walks_up() {
        let (doc, _style) = doc_from_html("<p><a href=\"/x\"><span>click me</span></a></p>");
        let span = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "span").unwrap_or(false))
            .unwrap();
        let (anchor_id, href) = enclosing_anchor(&doc, span).expect("should find anchor");
        assert_eq!(href, "/x");
        assert_eq!(doc.element(anchor_id).unwrap().name, "a");
    }

    #[test]
    fn flex_row_distributes_width() {
        let (doc, style) = doc_from_html(
            "<style>div.row{display:flex} div.row > div{flex:1}</style>\
             <body><div class=row><div>A</div><div>B</div><div>C</div></div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 600.0);
        let row = &bx.children[0];
        assert!(matches!(row.kind, BoxKind::Block));
        let items: Vec<&LayoutBox> = row
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        assert_eq!(items.len(), 3);
        // Items lie side-by-side.
        assert_eq!(items[0].frame.y, items[1].frame.y);
        assert!(items[1].frame.x > items[0].frame.x);
        assert!(items[2].frame.x > items[1].frame.x);
        // With flex: 1 each, all three items get the same width.
        let w0 = items[0].frame.width;
        let w1 = items[1].frame.width;
        let w2 = items[2].frame.width;
        assert!((w0 - w1).abs() < 1.0 && (w1 - w2).abs() < 1.0);
    }

    #[test]
    fn flex_grow_distribution_is_proportional() {
        // First item flex:1, second flex:3 — second item should be ~3x wider.
        let (doc, style) = doc_from_html(
            "<style>div.row{display:flex} \
                    div.row > div.a{flex:1} \
                    div.row > div.b{flex:3}</style>\
             <body><div class=row><div class=a>A</div><div class=b>B</div></div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        let row = &bx.children[0];
        let items: Vec<&LayoutBox> = row
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        assert_eq!(items.len(), 2);
        let ratio = items[1].frame.width / items[0].frame.width;
        assert!(
            (ratio - 3.0).abs() < 0.2,
            "expected flex:3 / flex:1 ratio ≈ 3, got {ratio}"
        );
    }

    #[test]
    fn justify_content_center() {
        // Two items with fixed flex-basis; centred should leave equal slack
        // before the first and after the last.
        let (doc, style) = doc_from_html(
            "<style>div.row{display:flex; justify-content:center} \
                    div.row > div{flex:0 0 100px}</style>\
             <body><div class=row><div>A</div><div>B</div></div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 600.0);
        let row = &bx.children[0];
        let items: Vec<&LayoutBox> = row
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        assert_eq!(items.len(), 2);
        // Total item width = 200px in a 600px row → 200px slack on each side
        // when centered (so 200px before the first, after the second).
        let first_x = items[0].frame.x;
        // Body has 8px UA margin, so content origin is x = 8.
        let expected = 8.0 + (600.0 - 16.0 - 200.0) * 0.5;
        assert!(
            (first_x - expected).abs() < 2.0,
            "first item x {first_x}, expected near {expected}"
        );
    }

    #[test]
    fn justify_content_space_between() {
        let (doc, style) = doc_from_html(
            "<style>div.row{display:flex; justify-content:space-between} \
                    div.row > div{flex:0 0 100px}</style>\
             <body><div class=row><div>A</div><div>B</div><div>C</div></div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 600.0);
        let row = &bx.children[0];
        let items: Vec<&LayoutBox> = row
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        // Content width 600 - 2*8(body margin) = 584; items 300; slack 284
        // distributed as 142 + 142 between the three items. So:
        //   item 0 starts at 8
        //   item 1 starts at 8 + 100 + 142 = 250
        //   item 2 starts at 8 + 200 + 284 = 492
        assert!((items[0].frame.x - 8.0).abs() < 2.0);
        let gap_01 = items[1].frame.x - (items[0].frame.x + items[0].frame.width);
        let gap_12 = items[2].frame.x - (items[1].frame.x + items[1].frame.width);
        assert!((gap_01 - gap_12).abs() < 2.0, "expected equal inter-item gaps, got {gap_01} vs {gap_12}");
    }

    #[test]
    fn align_items_center() {
        // Items with different heights — center should align them on a
        // common centre line.
        let (doc, style) = doc_from_html(
            "<style>div.row{display:flex; align-items:center; height:200px} \
                    div.a{flex:0 0 100px; height:50px} \
                    div.b{flex:0 0 100px; height:150px}</style>\
             <body><div class=row><div class=a>A</div><div class=b>B</div></div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 600.0);
        let row = &bx.children[0];
        let items: Vec<&LayoutBox> = row
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        assert_eq!(items.len(), 2);
        // Centred items have the same vertical-centre y. The taller item
        // (150px) sits closer to the top, the shorter (50px) is offset
        // by (150-50)/2 = 50 from where it would be at flex-start.
        let mid_a = items[0].frame.y + items[0].frame.height * 0.5;
        let mid_b = items[1].frame.y + items[1].frame.height * 0.5;
        assert!(
            (mid_a - mid_b).abs() < 2.0,
            "expected centred items to share a midline, got {mid_a} vs {mid_b}"
        );
    }

    #[test]
    fn input_renders_as_inline_control() {
        let (doc, style) = doc_from_html(
            "<body><p>Search: <input type=\"text\" value=\"hello\"></p></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        // Walk the tree and look for a Control line item with the right label.
        let mut found = false;
        fn walk(b: &LayoutBox, found: &mut bool) {
            for line in &b.lines {
                for item in &line.items {
                    if let LineItem::Control { label, kind, .. } = item {
                        if label == "hello" && matches!(kind, ControlKind::Input) {
                            *found = true;
                        }
                    }
                }
            }
            for c in &b.children {
                walk(c, found);
            }
        }
        walk(&bx, &mut found);
        assert!(found, "expected an Input control with label 'hello'");
    }

    #[test]
    fn button_label_comes_from_text_content() {
        let (doc, style) =
            doc_from_html("<body><button>Click <span>me</span></button></body>");
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        let mut got: Option<String> = None;
        fn walk(b: &LayoutBox, got: &mut Option<String>) {
            for line in &b.lines {
                for item in &line.items {
                    if let LineItem::Control { label, kind, .. } = item {
                        if matches!(kind, ControlKind::Button) {
                            *got = Some(label.clone());
                        }
                    }
                }
            }
            for c in &b.children {
                walk(c, got);
            }
        }
        walk(&bx, &mut got);
        assert_eq!(got.as_deref(), Some("Click me"));
    }

    #[test]
    fn position_relative_shifts_visually_keeping_flow() {
        // The shifted box visually moves by (top: 20px, left: 30px), but
        // its sibling underneath still sits where it would have been
        // without the shift (relative stays in flow).
        let (doc, style) = doc_from_html(
            "<style>\
                 body { margin: 0 }\
                 div.shifted { position: relative; top: 20px; left: 30px; height: 50px }\
                 div.next { height: 40px }\
             </style>\
             <body>\
                 <div class=\"shifted\">A</div>\
                 <div class=\"next\">B</div>\
             </body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        let blocks: Vec<&LayoutBox> = bx
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        assert_eq!(blocks.len(), 2);
        // Shifted: original (0, 0) → (30, 20).
        assert!((blocks[0].frame.x - 30.0).abs() < 0.5);
        assert!((blocks[0].frame.y - 20.0).abs() < 0.5);
        // Next sibling sits at y = 50 (the shifted one's original height).
        assert!((blocks[1].frame.y - 50.0).abs() < 0.5);
    }

    #[test]
    fn position_absolute_pulls_out_of_flow() {
        // The absolute box doesn't take vertical space, so the sibling
        // sits at y = 0 (right under body's top), and the absolute box
        // is positioned via top/left from the body (its containing block).
        let (doc, style) = doc_from_html(
            "<style>\
                 body { margin: 0; position: relative; height: 500px }\
                 div.abs { position: absolute; top: 100px; left: 50px; width: 80px; height: 40px }\
                 div.flow { height: 30px }\
             </style>\
             <body>\
                 <div class=\"abs\">A</div>\
                 <div class=\"flow\">B</div>\
             </body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        let blocks: Vec<&LayoutBox> = bx
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        assert_eq!(blocks.len(), 2);
        // Absolute box at (50, 100) — body is its own containing block.
        assert!((blocks[0].frame.x - 50.0).abs() < 0.5);
        assert!((blocks[0].frame.y - 100.0).abs() < 0.5);
        // Sibling stays in flow at y = 0 (no margin, no preceding box).
        assert!((blocks[1].frame.y - 0.0).abs() < 0.5);
    }

    #[test]
    fn col_and_colgroup_seed_table_column_widths() {
        // <col width="N"> and <colgroup width="N" span="K">
        // pre-populate per-column widths. Cells in the first row
        // inherit those widths.
        let (doc, style) = doc_from_html(
            "<table>\
                 <colgroup>\
                     <col width=\"200\">\
                     <col width=\"100\">\
                 </colgroup>\
                 <tr><td>A</td><td>B</td></tr>\
             </table>",
        );
        let table = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "table").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, table);
        layout(&mut bx, 0.0, 0.0, 800.0);
        // Find the cells.
        let mut cells: Vec<&LayoutBox> = Vec::new();
        fn walk<'a>(b: &'a LayoutBox, out: &mut Vec<&'a LayoutBox>) {
            if matches!(b.style.display, Display::TableCell)
                && matches!(b.kind, BoxKind::Block)
            {
                out.push(b);
            }
            for c in &b.children {
                walk(c, out);
            }
        }
        walk(&bx, &mut cells);
        assert_eq!(cells.len(), 2, "expected 2 cells");
        // First cell at col 0 should be 200 wide; second at col 1
        // should be 100 wide. (Allow ~1px tolerance.)
        assert!((cells[0].frame.width - 200.0).abs() < 1.0,
            "cell A width {}", cells[0].frame.width);
        assert!((cells[1].frame.width - 100.0).abs() < 1.0,
            "cell B width {}", cells[1].frame.width);
    }

    #[test]
    fn flex_column_margin_top_auto_pushes_item_down() {
        // CSS Flexbox §8.1: margin-top: auto in a column flex item
        // pushes it to the bottom by absorbing the leftover main-
        // axis free space. Mirrors Google's `.k1zIA{margin-top:auto}`
        // pattern that vertically centers/bottoms the logo within a
        // tall flex column.
        let (doc, style) = doc_from_html(
            "<style>\
                 body { margin: 0; height: 600px }\
                 .col { display: flex; flex-direction: column; height: 600px }\
                 .item { height: 100px; margin-top: auto }\
             </style>\
             <body><div class=\"col\">\
                 <div class=\"item\">A</div>\
             </div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        let col = bx
            .children
            .iter()
            .find(|c| matches!(c.style.display, Display::Flex))
            .expect("flex col");
        // Single item should land at y = 600 - 100 = 500 (pushed
        // down by margin-top: auto absorbing 500px of free space).
        let item = &col.children[0];
        assert!((item.frame.y - 500.0).abs() < 1.0,
            "item.y={}, expected 500", item.frame.y);
    }

    #[test]
    fn flex_row_auto_margin_consumes_free_space() {
        // CSS Flexbox §8.1: `margin-left: auto` on a row-flex item
        // pushes the item to the right by absorbing positive free
        // space on the main axis. With one auto margin, the item
        // ends up at the right edge.
        let (doc, style) = doc_from_html(
            "<style>\
                 body { margin: 0 }\
                 .row { display: flex }\
                 .item { width: 100px; height: 30px }\
                 .pushed { margin-left: auto }\
             </style>\
             <body><div class=\"row\">\
                 <div class=\"item\">A</div>\
                 <div class=\"item pushed\">B</div>\
             </div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        let row = bx
            .children
            .iter()
            .find(|c| matches!(c.style.display, Display::Flex))
            .expect("flex row");
        let items: Vec<&LayoutBox> = row.children.iter().collect();
        assert_eq!(items.len(), 2);
        // First item at x=0, second at x=700 (800 - 100, pushed right).
        assert!((items[0].frame.x - 0.0).abs() < 1.0,
            "item A at {}", items[0].frame.x);
        assert!((items[1].frame.x - 700.0).abs() < 1.0,
            "item B at {}, expected 700", items[1].frame.x);
    }

    #[test]
    fn flex_row_reverse_places_first_item_last() {
        // flex-direction: row-reverse lays the FIRST DOM item at the
        // RIGHT. DuckDuckGo's CTA cards are row-reverse: DOM order
        // [browser, search] must render [search, browser] L→R.
        let (doc, style) = doc_from_html(
            "<style>\
                 body { margin: 0 }\
                 .row { display: flex; flex-direction: row-reverse }\
                 .item { width: 100px; height: 30px }\
             </style>\
             <body><div class=\"row\">\
                 <div id=\"a\" class=\"item\">A</div>\
                 <div id=\"b\" class=\"item\">B</div>\
             </div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        let row = bx.children.iter()
            .find(|c| matches!(c.style.display, Display::Flex)).expect("flex row");
        // Find A and B by id and assert A is to the RIGHT of B.
        let mut ax = -1.0; let mut bxx = -1.0;
        for c in &row.children {
            if let Some(n) = c.node {
                if let Some(e) = doc.element(n) {
                    match e.get_attr("id") {
                        Some("a") => ax = c.frame.x,
                        Some("b") => bxx = c.frame.x,
                        _ => {}
                    }
                }
            }
        }
        assert!(ax > bxx, "row-reverse: A (x={ax}) must be right of B (x={bxx})");
    }

    #[test]
    fn replaced_percent_height_resolves_against_container_height() {
        // `width:auto; height:100%` on a replaced element (the
        // DuckDuckGo header wordmark, viewBox 189x53) must resolve
        // height against the container's HEIGHT (32px), preserving the
        // viewBox aspect — NOT against the width (which exploded it).
        let mut cv = ComputedValues::root_default();
        cv.width = Dimension::Auto;
        cv.height = Dimension::Length(Length::Percent(100.0));
        let (w, h) = resolve_replaced_size(&cv, 189.0, 53.0, 1152.0, Some(32.0));
        assert!((h - 32.0).abs() < 0.5, "height should be 32 (100% of container), got {h}");
        let expected_w = 32.0 * (189.0 / 53.0);
        assert!((w - expected_w).abs() < 1.0, "width should preserve aspect (~{expected_w}), got {w}");
    }

    #[test]
    fn replaced_percent_height_indefinite_falls_back_to_intrinsic() {
        // With no definite container height, percent height is auto →
        // intrinsic size (not the viewport / width).
        let mut cv = ComputedValues::root_default();
        cv.width = Dimension::Auto;
        cv.height = Dimension::Length(Length::Percent(100.0));
        let (w, h) = resolve_replaced_size(&cv, 189.0, 53.0, 1152.0, None);
        assert!((w - 189.0).abs() < 0.5 && (h - 53.0).abs() < 0.5, "got {w}x{h}");
    }

    #[test]
    fn flex_order_reorders_items() {
        // CSS `order` lays items in order-modified document order:
        // `order:-1` on the second item moves it before the first.
        // DuckDuckGo's CTA cards rely on this.
        let (doc, style) = doc_from_html(
            "<style>\
                 body { margin: 0 }\
                 .row { display: flex }\
                 .item { width: 100px; height: 30px }\
                 .first { order: -1 }\
             </style>\
             <body><div class=\"row\">\
                 <div id=\"a\" class=\"item\">A</div>\
                 <div id=\"b\" class=\"item first\">B</div>\
             </div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        let row = bx.children.iter()
            .find(|c| matches!(c.style.display, Display::Flex)).expect("flex row");
        let mut ax = -1.0; let mut bxx = -1.0;
        for c in &row.children {
            if let Some(n) = c.node {
                match doc.element(n).and_then(|e| e.get_attr("id")) {
                    Some("a") => ax = c.frame.x,
                    Some("b") => bxx = c.frame.x,
                    _ => {}
                }
            }
        }
        // B (order:-1) lays out first → left of A.
        assert!(bxx < ax, "order:-1 B (x={bxx}) must be left of A (x={ax})");
    }

    #[test]
    fn flex_inline_block_children_are_separate_items() {
        // Per CSS Flexbox §4, every in-flow ELEMENT child of a flex
        // container is its own flex item — even if it's
        // display: inline-block. Was previously bundled together with
        // text-runs in one anonymous flex item, which crushed Google's
        // top nav layout (two anchors + a flex-grow:1 wrapper became
        // one item taking 1/3 each instead of three items at content-
        // sized + content-sized + grow).
        let (doc, style) = doc_from_html(
            "<style>\
                 body { margin: 0 }\
                 .row { display: flex }\
                 .a { display: inline-block; padding: 5px }\
                 .b { display: inline-block; flex-grow: 1 }\
             </style>\
             <body><div class=\"row\">\
                 <a class=\"a\">First</a>\
                 <a class=\"a\">Second</a>\
                 <div class=\"b\">grows</div>\
             </div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        // Find the flex container (should be the .row div directly).
        let row = bx
            .children
            .iter()
            .find(|c| matches!(c.style.display, Display::Flex))
            .expect("flex row");
        // Three flex items, in source order along the main axis.
        let items: Vec<&LayoutBox> = row.children.iter().collect();
        assert_eq!(items.len(), 3, "expected 3 flex items, got {}", items.len());
        // The two anchors should sit at content size (small), the
        // grow:1 div should expand to fill the rest. Sum is 800.
        let sum: f32 = items.iter().map(|i| i.frame.width).sum();
        assert!((sum - 800.0).abs() < 1.0, "sum was {}", sum);
        // Anchor widths much smaller than the grow item.
        assert!(
            items[2].frame.width > items[0].frame.width * 5.0,
            "grow item ({}) should dominate anchor widths ({}, {})",
            items[2].frame.width, items[0].frame.width, items[1].frame.width
        );
    }

    #[test]
    fn flex_position_absolute_skipped_from_main_axis_budget() {
        // The position:absolute child shouldn't take space in the
        // flex layout — it's out of flow per Flexbox §4. The two
        // remaining items should split the full main axis. Mirrors
        // Google's search bar where a position:absolute overlay was
        // crushing the input column to a fraction of its width.
        let (doc, style) = doc_from_html(
            "<style>\
                 body { margin: 0 }\
                 .row { display: flex; position: relative }\
                 .item { flex-grow: 1; height: 30px }\
                 .ovl { position: absolute; top: 0; left: 0; right: 0; height: 30px }\
             </style>\
             <body><div class=\"row\">\
                 <div class=\"item\">A</div>\
                 <div class=\"ovl\">overlay</div>\
                 <div class=\"item\">B</div>\
             </div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        let row = bx
            .children
            .iter()
            .find(|c| matches!(c.style.display, Display::Flex))
            .expect("flex row");
        // Two in-flow items + one out-of-flow appended at the end.
        // The two in-flow items should split 800px.
        let item_a = &row.children[0];
        let item_b = &row.children[1];
        assert!((item_a.frame.width - 400.0).abs() < 1.0,
            "item A width {} should be ~400", item_a.frame.width);
        assert!((item_b.frame.width - 400.0).abs() < 1.0,
            "item B width {} should be ~400", item_b.frame.width);
    }

    #[test]
    fn position_absolute_uses_nearest_positioned_ancestor() {
        // The .abs box should pin to .outer (position: relative), not body.
        let (doc, style) = doc_from_html(
            "<style>\
                 body { margin: 0 }\
                 div.outer { position: relative; margin-top: 200px; height: 300px }\
                 div.abs { position: absolute; top: 10px; left: 10px; height: 20px }\
             </style>\
             <body><div class=\"outer\"><div class=\"abs\">x</div></div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        // outer.frame.y ≈ 200 (its margin-top); absolute child at y ≈ 210.
        let outer = bx
            .children
            .iter()
            .find(|c| matches!(c.kind, BoxKind::Block))
            .unwrap();
        let abs = outer
            .children
            .iter()
            .find(|c| matches!(c.kind, BoxKind::Block))
            .unwrap();
        assert!(
            (abs.frame.y - (outer.frame.y + 10.0)).abs() < 0.5,
            "abs.y={} outer.y={}, expected outer.y + 10",
            abs.frame.y,
            outer.frame.y
        );
        assert!((abs.frame.x - 10.0).abs() < 0.5);
    }

    #[test]
    fn img_display_block_with_svg_paints_via_inline_svg() {
        // Wikipedia's logo CSS sets `.mw-logo-wordmark { display: block }`
        // on the <img>. Without the build_block img branch, the SVG
        // entry never reaches a LineItem::Svg and no PaintCommand::Svg
        // is emitted (the logo paints as a blank rectangle).
        use bui_paint::PathSegment;
        let html = "<body><img class=logo width=140 height=22></body>";
        let doc = bui_html::parse(html);
        let sheets = bui_style::extract_inline_stylesheets(&doc);
        let style_tree = bui_style::style_document(&doc, &sheets);
        let img_node = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "img").unwrap_or(false))
            .expect("img");
        let mut svgs = SvgRegistry::new();
        svgs.insert(
            img_node,
            crate::svg::SvgEntry {
                width: 140.0,
                height: 22.0,
                view_box: (0.0, 0.0, 140.0, 22.0),
                shapes: vec![crate::svg::SvgShape {
                    segments: vec![
                        PathSegment::MoveTo(0.0, 0.0),
                        PathSegment::LineTo(140.0, 0.0),
                        PathSegment::LineTo(140.0, 22.0),
                        PathSegment::LineTo(0.0, 22.0),
                        PathSegment::Close,
                    ],
                    fill: Some(bui_paint::Color::rgb(14, 101, 192)),
                    stroke: None,
                    stroke_width: 1.0,
                }],
                no_attr_size: false,
            },
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let images = ImageRegistry::new();
        let mut bx = build_with_images(&doc, &style_tree, &images, &svgs, body);
        layout(&mut bx, 0.0, 0.0, 1400.0);
        let mut dl = bui_paint::DisplayList::new();
        paint(&bx, &mut dl);
        let svg_cmds = dl
            .commands
            .iter()
            .filter(|c| matches!(c, bui_paint::PaintCommand::Svg { .. }))
            .count();
        assert!(
            svg_cmds >= 1,
            "expected ≥1 Svg paint command for img.logo, got {svg_cmds}",
        );
    }

    #[test]
    fn sticky_emits_paint_group_with_natural_y_and_top_edge() {
        // A `position: sticky; top: 12px` box should NOT have its
        // layout coordinates shifted (sticky's offset is resolved at
        // paint time against scroll), but its paint output must be
        // bracketed with PushStickyGroup / PopStickyGroup so the host
        // can apply the per-group scroll clamp.
        let (doc, style) = doc_from_html(
            "<style>\
                 body { margin: 0 }\
                 .sk { position: sticky; top: 12px; height: 40px }\
             </style>\
             <body><div class=sk>head</div><div>body</div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        // No relative shift: the sticky box stays at its natural y.
        let sticky_box = bx
            .children
            .iter()
            .find(|c| {
                c.node
                    .and_then(|n| doc.element(n))
                    .map(|e| {
                        e.attrs.iter().any(|(k, v)| {
                            k == "class" && v.split_whitespace().any(|c| c == "sk")
                        })
                    })
                    .unwrap_or(false)
            })
            .expect("sticky box");
        assert!(sticky_box.frame.y.abs() < 0.5, "sticky.y={} (expected 0)", sticky_box.frame.y);
        // The next sibling sits BELOW the sticky box (sticky still
        // takes flow space, unlike absolute).
        let body_div = bx.children.iter().skip(1).find(|c| {
            c.node
                .and_then(|n| doc.element(n))
                .map(|e| e.name == "div")
                .unwrap_or(false)
        });
        if let Some(b) = body_div {
            assert!(
                b.frame.y >= 40.0 - 0.5,
                "second div should sit below sticky (got y={})",
                b.frame.y
            );
        }
        // Paint output brackets the sticky subtree.
        let mut dl = bui_paint::DisplayList::new();
        paint(&bx, &mut dl);
        let push_idx = dl.commands.iter().position(|c| {
            matches!(
                c,
                bui_paint::PaintCommand::PushStickyGroup { top_edge, .. } if (*top_edge - 12.0).abs() < 0.5
            )
        });
        let pop_idx = dl.commands.iter().position(|c| {
            matches!(c, bui_paint::PaintCommand::PopStickyGroup)
        });
        let push_idx = push_idx.expect("PushStickyGroup with top_edge=12px");
        let pop_idx = pop_idx.expect("PopStickyGroup");
        assert!(push_idx < pop_idx, "push must precede pop");
    }

    #[test]
    fn flex_item_sizes_to_float_child_width() {
        // Wikipedia's `a.mw-logo` is a flex item whose only child is a
        // `float: left` span wrapping two 140-px images. Before this
        // test the flex item collapsed to 0 width because
        // estimate_max_content_width didn't see the float subtree's
        // images, and the next flex sibling (`vector-header-end`)
        // painted on top of the logo.
        let (doc, style) = doc_from_html(
            "<style>\
                 body { display: flex; margin: 0 }\
                 a.logo { display: block }\
                 span.fl { float: left }\
                 img.w { width: 140px; height: 22px }\
                 img.t { width: 140px; height: 11px }\
             </style>\
             <body><a class=logo><span class=fl>\
               <img class=w><img class=t>\
             </span></a><div>next</div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 1400.0);
        let logo = bx
            .children
            .iter()
            .find(|c| {
                c.node
                    .and_then(|id| doc.element(id))
                    .map(|e| e.name == "a")
                    .unwrap_or(false)
            })
            .expect("a.logo flex item");
        assert!(
            logo.frame.width >= 140.0,
            "logo flex item should be ≥ 140 wide (got {})",
            logo.frame.width,
        );
    }

    #[test]
    fn table_two_columns_lay_out_side_by_side() {
        // Wikipedia infobox shape: a 2-column, 2-row table inside <tbody>.
        // Cells in the same row should share a y; cells in the same column
        // should share an x.
        let (doc, style) = doc_from_html(
            "<table><tbody>\
                <tr><td>Born</td><td>1879</td></tr>\
                <tr><td>Died</td><td>1955</td></tr>\
             </tbody></table>",
        );
        let table = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "table").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, table);
        layout(&mut bx, 0.0, 0.0, 600.0);
        // Walk to find the four cells.
        let mut cells: Vec<&LayoutBox> = Vec::new();
        fn walk<'a>(b: &'a LayoutBox, out: &mut Vec<&'a LayoutBox>) {
            // Anonymous wrappers AND child text-leaves inherit `display`
            // from their parent cell. Restrict the match to true block
            // boxes that came from a real DOM element.
            if matches!(b.style.display, Display::TableCell)
                && matches!(b.kind, BoxKind::Block)
                && b.node.is_some()
            {
                out.push(b);
            }
            for c in &b.children {
                walk(c, out);
            }
        }
        walk(&bx, &mut cells);
        assert_eq!(cells.len(), 4, "expected 4 cells, got {}", cells.len());
        // Row 1: cells[0..2] share y; row 2: cells[2..4] share y.
        assert!((cells[0].frame.y - cells[1].frame.y).abs() < 0.5);
        assert!((cells[2].frame.y - cells[3].frame.y).abs() < 0.5);
        assert!(cells[2].frame.y > cells[0].frame.y);
        // Columns: 0 and 2 share x; 1 and 3 share x; col 1 is to the right.
        assert!((cells[0].frame.x - cells[2].frame.x).abs() < 0.5);
        assert!((cells[1].frame.x - cells[3].frame.x).abs() < 0.5);
        assert!(cells[1].frame.x > cells[0].frame.x);
    }

    #[test]
    fn list_markers_disambiguate_ul_and_ol() {
        let (doc, style) =
            doc_from_html("<body><ul><li>a</li><li>b</li></ul><ol><li>x</li><li>y</li></ol></body>");
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let bx = build(&doc, &style, body);
        let mut markers: Vec<String> = Vec::new();
        fn walk(b: &LayoutBox, out: &mut Vec<String>) {
            if let Some(m) = &b.list_marker {
                out.push(m.clone());
            }
            for c in &b.children {
                walk(c, out);
            }
        }
        walk(&bx, &mut markers);
        assert_eq!(markers, vec!["•", "•", "1.", "2."]);
    }

    #[test]
    fn inline_block_shrink_to_fit_sits_inline() {
        let (doc, style) = doc_from_html(
            "<style>\
                 body { margin: 0 }\
                 span.btn { display: inline-block; padding: 0 10px; width: 80px; height: 24px }\
             </style>\
             <body>before <span class=btn>X</span> after</body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 600.0);
        // Find the inline-block. It should be on the same line as the
        // surrounding text — text run before, then inline-block, then
        // text run after.
        fn collect_items<'a>(b: &'a LayoutBox, out: &mut Vec<&'a LineItem>) {
            for line in &b.lines {
                for it in &line.items {
                    out.push(it);
                }
            }
            for c in &b.children {
                collect_items(c, out);
            }
        }
        let mut items = Vec::new();
        collect_items(&bx, &mut items);
        let inline_block_count = items
            .iter()
            .filter(|i| matches!(i, LineItem::InlineBlock { .. }))
            .count();
        assert_eq!(inline_block_count, 1);
        // Frame width is the outer border-box: 80 (content) + 20
        // (10px left + 10px right padding).
        let host_w = items
            .iter()
            .find_map(|i| if let LineItem::InlineBlock { frame, .. } = i { Some(frame.width) } else { None })
            .unwrap();
        assert!((host_w - 100.0).abs() < 1.0, "expected width ≈ 100, got {host_w}");
    }

    #[test]
    fn before_and_after_inject_synthetic_inline_text() {
        // Wikipedia-style external-link arrow: a.external::after
        // appends "↗" after the link text. The synthetic content
        // should appear as an InlineText next to the real text.
        let (doc, style) = doc_from_html(
            "<style>\
                 a.ext::before { content: \"[\" } \
                 a.ext::after { content: \"]\" }\
             </style>\
             <body><p><a class=\"ext\" href=\"/x\">link</a></p></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 600.0);
        // Walk lines for text runs in source order. The first should
        // be "[", the second "link", the third "]" — proving the
        // synthetic ::before / ::after content joins the inline flow
        // alongside the real text.
        fn collect_text<'a>(b: &'a LayoutBox, out: &mut Vec<String>) {
            for line in &b.lines {
                for it in &line.items {
                    if let LineItem::Text(run) = it {
                        out.push(run.text.clone());
                    }
                }
            }
            for c in &b.children {
                collect_text(c, out);
            }
        }
        let mut texts = Vec::new();
        collect_text(&bx, &mut texts);
        let visible: Vec<&String> = texts.iter().filter(|t| !t.trim().is_empty()).collect();
        assert!(
            visible.len() >= 3,
            "expected ::before, real text, ::after, got {visible:?}"
        );
        assert!(
            visible.iter().any(|t| t.contains("[")),
            "missing ::before bracket in {visible:?}"
        );
        assert!(
            visible.iter().any(|t| t.contains("]")),
            "missing ::after bracket in {visible:?}"
        );
    }

    #[test]
    fn adjacent_block_margins_collapse() {
        // Two paragraphs with margin: 30px each. Without collapse the
        // gap would be 60; with collapse it should be 30.
        let (doc, style) = doc_from_html(
            "<style>body { margin: 0 } div.a { margin: 30px 0; height: 10px } \
             div.b { margin: 30px 0; height: 10px }</style>\
             <body><div class=a>A</div><div class=b>B</div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        let blocks: Vec<&LayoutBox> = bx
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        // Block A occupies y=30..40 (margin-top:30 + height:10).
        // Block B should sit at y = 70 (40 + max(30, 30) = 70), not 100.
        let gap = blocks[1].frame.y - (blocks[0].frame.y + blocks[0].frame.height);
        assert!(
            (gap - 30.0).abs() < 1.0,
            "expected collapsed gap ≈ 30, got {gap}"
        );
    }

    #[test]
    fn float_right_pulls_to_right_edge_and_shrinks_following_sibling() {
        // Wikipedia-shape: a floated thumbnail at the right edge,
        // text continues in the remaining width on (roughly) the
        // same y. Use <div>s for the text so we don't pick up the
        // <p> UA margin and can assert exact coords.
        let (doc, style) = doc_from_html(
            "<style>\
                 body { margin: 0 }\
                 .thumb { float: right; width: 200px; height: 100px }\
                 .text { height: 80px }\
             </style>\
             <body>\
                 <div class=thumb></div>\
                 <div class=text>x</div>\
             </body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        let blocks: Vec<&LayoutBox> = bx
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        assert_eq!(blocks.len(), 2);
        let thumb = blocks[0];
        let text = blocks[1];
        // Thumb sits at the right edge (800 - 200 = 600).
        assert!((thumb.frame.x - 600.0).abs() < 1.0,
            "thumb.x={}, expected ≈ 600", thumb.frame.x);
        // Both blocks share y = 0 (text wraps next to the float).
        assert!((thumb.frame.y - 0.0).abs() < 1.0);
        assert!((text.frame.y - 0.0).abs() < 1.0,
            "text.y={}, expected ≈ 0 (next to float)", text.frame.y);
        // Text shrinks to 800 - 200 = 600 wide.
        assert!((text.frame.width - 600.0).abs() < 1.0,
            "text.width={}, expected ≈ 600", text.frame.width);
    }

    #[test]
    fn clear_pushes_past_preceding_float() {
        let (doc, style) = doc_from_html(
            "<style>\
                 body { margin: 0 }\
                 .thumb { float: left; width: 100px; height: 50px }\
                 .clr { clear: left; height: 20px }\
             </style>\
             <body>\
                 <div class=thumb></div>\
                 <div class=clr>x</div>\
             </body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        let blocks: Vec<&LayoutBox> = bx
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        let cleared = blocks[1];
        // clear: left → cleared block sits below the 50px float.
        assert!((cleared.frame.y - 50.0).abs() < 1.0,
            "cleared.y={}, expected ≈ 50", cleared.frame.y);
    }

    #[test]
    fn flex_wrap_splits_into_rows_when_items_overflow() {
        // Three 200px items in a 500px row with flex-wrap: wrap should
        // place items 0,1 on row 0 and item 2 on row 1.
        let (doc, style) = doc_from_html(
            "<style>div.row{display:flex; flex-wrap:wrap} \
                    div.row > div{flex:0 0 200px; height: 30px}</style>\
             <body><div class=row><div>A</div><div>B</div><div>C</div></div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 500.0);
        let row = &bx.children[0];
        let items: Vec<&LayoutBox> = row
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        assert_eq!(items.len(), 3);
        // 0 and 1 share y; 2 is below.
        assert!((items[0].frame.y - items[1].frame.y).abs() < 0.5);
        assert!(items[2].frame.y > items[0].frame.y + 10.0);
        // 0 and 2 share x (both at the row start of their row).
        assert!((items[0].frame.x - items[2].frame.x).abs() < 0.5);
    }

    #[test]
    fn flex_with_inline_children_doesnt_recurse_forever() {
        // Regression for the Wikipedia-main-page crash: a flex
        // container with bare text children used to wrap the inline
        // run in an anonymous box that inherited `display: flex`,
        // which made layout_block re-enter layout_flex, wrap again,
        // and overflow the stack. The anon's display must be Block.
        let (doc, style) = doc_from_html(
            "<style>div.row{display:flex}</style>\
             <body><div class=row>hello world</div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        // Just running layout to completion is the assertion.
        layout(&mut bx, 0.0, 0.0, 600.0);
    }

    #[test]
    fn grid_three_fixed_columns_place_side_by_side() {
        let (doc, style) = doc_from_html(
            "<style>.g{display:grid;grid-template-columns:100px 200px 300px}\
             .c{display:block}</style>\
             <body><div class=g>\
             <div class=c>A</div><div class=c>B</div><div class=c>C</div>\
             </div></body>",
        );
        let grid = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs
                            .iter()
                            .any(|(k, v)| k == "class" && v.split_whitespace().any(|c| c == "g"))
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        let mut bx = build(&doc, &style, grid);
        layout(&mut bx, 0.0, 0.0, 800.0);
        let cells: Vec<&LayoutBox> = bx
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        assert_eq!(cells.len(), 3);
        assert!((cells[0].frame.x - 0.0).abs() < 0.5, "A at 0");
        assert!((cells[1].frame.x - 100.0).abs() < 0.5, "B at 100");
        assert!((cells[2].frame.x - 300.0).abs() < 0.5, "C at 300");
        assert!((cells[0].frame.width - 100.0).abs() < 0.5);
        assert!((cells[1].frame.width - 200.0).abs() < 0.5);
        assert!((cells[2].frame.width - 300.0).abs() < 0.5);
        // All cells share the row's top.
        assert!((cells[0].frame.y - cells[1].frame.y).abs() < 0.5);
        assert!((cells[0].frame.y - cells[2].frame.y).abs() < 0.5);
    }

    #[test]
    fn grid_fr_units_split_remaining_space() {
        let (doc, style) = doc_from_html(
            "<style>.g{display:grid;grid-template-columns:200px 1fr 2fr}\
             .c{display:block}</style>\
             <body><div class=g>\
             <div class=c>A</div><div class=c>B</div><div class=c>C</div>\
             </div></body>",
        );
        let grid = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs
                            .iter()
                            .any(|(k, v)| k == "class" && v.split_whitespace().any(|c| c == "g"))
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        let mut bx = build(&doc, &style, grid);
        layout(&mut bx, 0.0, 0.0, 800.0);
        let cells: Vec<&LayoutBox> = bx
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        // 800 - 200 = 600 remaining, split 1:2 → 200 + 400.
        assert!((cells[0].frame.width - 200.0).abs() < 0.5);
        assert!((cells[1].frame.width - 200.0).abs() < 0.5);
        assert!((cells[2].frame.width - 400.0).abs() < 0.5);
    }

    #[test]
    fn grid_gap_separates_cells() {
        let (doc, style) = doc_from_html(
            "<style>.g{display:grid;grid-template-columns:100px 100px;gap:20px}\
             .c{display:block}</style>\
             <body><div class=g>\
             <div class=c>A</div><div class=c>B</div>\
             </div></body>",
        );
        let grid = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs
                            .iter()
                            .any(|(k, v)| k == "class" && v.split_whitespace().any(|c| c == "g"))
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        let mut bx = build(&doc, &style, grid);
        layout(&mut bx, 0.0, 0.0, 600.0);
        let cells: Vec<&LayoutBox> = bx
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        assert!((cells[0].frame.x - 0.0).abs() < 0.5);
        // Column gap pushes B to 100 + 20 = 120.
        assert!((cells[1].frame.x - 120.0).abs() < 0.5, "B at {}", cells[1].frame.x);
    }

    #[test]
    fn grid_wraps_to_next_row_when_columns_exhausted() {
        let (doc, style) = doc_from_html(
            "<style>.g{display:grid;grid-template-columns:100px 100px}\
             .c{display:block}</style>\
             <body><div class=g>\
             <div class=c>A</div><div class=c>B</div>\
             <div class=c>C</div><div class=c>D</div>\
             </div></body>",
        );
        let grid = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs
                            .iter()
                            .any(|(k, v)| k == "class" && v.split_whitespace().any(|c| c == "g"))
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        let mut bx = build(&doc, &style, grid);
        layout(&mut bx, 0.0, 0.0, 600.0);
        let cells: Vec<&LayoutBox> = bx
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        // A and B share row 1; C and D share row 2.
        assert!((cells[0].frame.y - cells[1].frame.y).abs() < 0.5);
        assert!((cells[2].frame.y - cells[3].frame.y).abs() < 0.5);
        assert!(cells[2].frame.y > cells[0].frame.y);
        // Columns repeat.
        assert!((cells[0].frame.x - cells[2].frame.x).abs() < 0.5);
        assert!((cells[1].frame.x - cells[3].frame.x).abs() < 0.5);
    }

    #[test]
    fn grid_explicit_column_placement_skips_auto_cursor() {
        let (doc, style) = doc_from_html(
            "<style>.g{display:grid;grid-template-columns:100px 100px 100px}\
             .c{display:block}\
             .right{grid-column:3}</style>\
             <body><div class=g>\
             <div class=c>A</div><div class=\"c right\">R</div>\
             </div></body>",
        );
        let grid = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs
                            .iter()
                            .any(|(k, v)| k == "class" && v.split_whitespace().any(|c| c == "g"))
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        let mut bx = build(&doc, &style, grid);
        layout(&mut bx, 0.0, 0.0, 600.0);
        let cells: Vec<&LayoutBox> = bx
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        // A goes in column 1 (x=0); R is pinned to column 3 (x=200).
        assert!((cells[0].frame.x - 0.0).abs() < 0.5);
        assert!((cells[1].frame.x - 200.0).abs() < 0.5, "R at {}", cells[1].frame.x);
        assert!((cells[0].frame.y - cells[1].frame.y).abs() < 0.5, "same row");
    }

    #[test]
    fn grid_repeat_expands_track_list() {
        let (doc, style) = doc_from_html(
            "<style>.g{display:grid;grid-template-columns:repeat(4, 50px)}\
             .c{display:block}</style>\
             <body><div class=g>\
             <div class=c>A</div><div class=c>B</div>\
             <div class=c>C</div><div class=c>D</div>\
             </div></body>",
        );
        let grid = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs
                            .iter()
                            .any(|(k, v)| k == "class" && v.split_whitespace().any(|c| c == "g"))
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        let mut bx = build(&doc, &style, grid);
        layout(&mut bx, 0.0, 0.0, 600.0);
        let cells: Vec<&LayoutBox> = bx
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        assert_eq!(cells.len(), 4);
        for (i, c) in cells.iter().enumerate() {
            let expected_x = i as f32 * 50.0;
            assert!(
                (c.frame.x - expected_x).abs() < 0.5,
                "cell {} at {}, expected {}",
                i,
                c.frame.x,
                expected_x
            );
        }
    }

    #[test]
    fn line_clamp_truncates_with_ellipsis() {
        // Three-line paragraph clamped to 2 lines. Result: 2 lines,
        // last line ends with "…".
        let (doc, style) = doc_from_html(
            "<style>.b{display:block;width:120px;line-clamp:2}</style>\
             <body><div class=b>Alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa</div></body>",
        );
        let b = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs.iter().any(|(k, v)| {
                            k == "class"
                                && v.split_whitespace().any(|c| c == "b")
                        })
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        let mut bx = build(&doc, &style, b);
        layout(&mut bx, 0.0, 0.0, 800.0);
        fn collect_lines(bx: &LayoutBox, out: &mut Vec<usize>) {
            if !bx.lines.is_empty() {
                out.push(bx.lines.len());
            }
            for c in &bx.children {
                collect_lines(c, out);
            }
        }
        let mut counts = Vec::new();
        collect_lines(&bx, &mut counts);
        // Expect the inline anonymous block has exactly 2 lines.
        assert!(
            counts.iter().any(|&n| n == 2),
            "expected 2-line block, got line counts {counts:?}"
        );
        // Last text run ends with U+2026.
        fn last_text_ends_with_ellipsis(bx: &LayoutBox) -> bool {
            for line in bx.lines.iter().rev() {
                for item in line.items.iter().rev() {
                    if let LineItem::Text(run) = item {
                        return run.text.ends_with('\u{2026}');
                    }
                }
            }
            for c in &bx.children {
                if last_text_ends_with_ellipsis(c) {
                    return true;
                }
            }
            false
        }
        assert!(
            last_text_ends_with_ellipsis(&bx),
            "line-clamp should append ellipsis"
        );
    }

    #[test]
    fn overflow_wrap_breaks_long_unbreakable_word() {
        // A long URL-like word in a 100px box. Without overflow-wrap
        // it'd render as one overflowing run; with break-word the
        // layout splits it across multiple lines.
        let (doc, style) = doc_from_html(
            "<style>.b{display:block;width:100px;\
             overflow-wrap:break-word}</style>\
             <body><div class=b>ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnop</div></body>",
        );
        let b = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs.iter().any(|(k, v)| {
                            k == "class"
                                && v.split_whitespace().any(|c| c == "b")
                        })
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        let mut bx = build(&doc, &style, b);
        layout(&mut bx, 0.0, 0.0, 800.0);
        // Walk to anonymous block holding the line(s).
        fn count_text_lines(bx: &LayoutBox) -> usize {
            let mut n = 0;
            for line in &bx.lines {
                if line
                    .items
                    .iter()
                    .any(|it| matches!(it, LineItem::Text(_)))
                {
                    n += 1;
                }
            }
            for c in &bx.children {
                n += count_text_lines(c);
            }
            n
        }
        let lines = count_text_lines(&bx);
        assert!(
            lines >= 2,
            "expected the long word to break across lines, got {lines}"
        );
    }

    #[test]
    fn pointer_events_none_lets_clicks_pass_through() {
        // Outer <a> covers the page; an overlay <div> with
        // pointer-events:none sits on top. A click at the overlay's
        // position should still resolve to the underlying anchor.
        let (doc, style) = doc_from_html(
            "<style>.over{pointer-events:none}</style>\
             <body><a id=lnk href=\"/x\">\
             link <span class=over>Overlay text</span> done\
             </a></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 600.0);
        // Find the overlay text run's frame.
        let mut overlay_x = 0.0_f32;
        let mut overlay_y = 0.0_f32;
        fn find_run(bx: &LayoutBox, target: &str, x: &mut f32, y: &mut f32) {
            for line in &bx.lines {
                for item in &line.items {
                    if let LineItem::Text(run) = item {
                        if run.text.contains(target) {
                            *x = run.frame.x + 1.0;
                            *y = run.frame.y + run.frame.height * 0.5;
                        }
                    }
                }
            }
            for c in &bx.children {
                find_run(c, target, x, y);
            }
        }
        find_run(&bx, "Overlay", &mut overlay_x, &mut overlay_y);
        assert!(overlay_y > 0.0, "overlay must be laid out");
        // Hit at the overlay's coords. Walk up to the anchor.
        let hit = hit_test(&bx, overlay_x, overlay_y).expect("a hit");
        let anchor = enclosing_anchor(&doc, hit);
        assert!(
            anchor.is_some(),
            "click on pointer-events:none overlay should still find the surrounding <a>"
        );
    }

    #[test]
    fn visibility_hidden_skips_paint_but_keeps_layout() {
        // Two divs: hidden + visible. Layout should reserve space for
        // the hidden one (its frame is non-zero), but no Text command
        // for "Invisible" should reach the display list.
        let (doc, style) = doc_from_html(
            "<style>.h{visibility:hidden}</style>\
             <body><div class=h>Invisible</div><div>Visible</div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 600.0);
        let mut dl = DisplayList::new();
        paint(&bx, &mut dl);
        let invisible_painted = dl.commands.iter().any(|c| {
            matches!(c, bui_paint::PaintCommand::Text { content, .. } if content == "Invisible")
        });
        let visible_painted = dl.commands.iter().any(|c| {
            matches!(c, bui_paint::PaintCommand::Text { content, .. } if content == "Visible")
        });
        assert!(!invisible_painted, "hidden box must not paint text");
        assert!(visible_painted, "visible sibling must paint");
        // Both children still occupy layout space — the hidden one's
        // frame should have non-zero height so the visible one sits
        // below it.
        let blocks: Vec<&LayoutBox> = bx
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        assert_eq!(blocks.len(), 2);
        assert!(blocks[1].frame.y > blocks[0].frame.y, "layout reserved space");
    }

    #[test]
    fn inline_svg_currentcolor_inherits_host_color() {
        // The <svg> sits inside a <span style="color:red"> — its
        // <path fill="currentColor"> should resolve to red, not the
        // black default.
        let (doc, style) = doc_from_html(
            "<body><span style=\"color: rgb(200, 0, 0)\">\
             <svg width=\"10\" height=\"10\" viewBox=\"0 0 10 10\">\
             <path d=\"M0 0 L10 0 L10 10 Z\" fill=\"currentColor\"/>\
             </svg></span></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        let mut found = false;
        fn walk(bx: &LayoutBox, found: &mut bool) {
            for line in &bx.lines {
                for item in &line.items {
                    if let LineItem::Svg { entry, .. } = item {
                        for shape in &entry.shapes {
                            if let Some(c) = shape.fill {
                                if c.r == 200 && c.g == 0 && c.b == 0 {
                                    *found = true;
                                }
                            }
                        }
                    }
                }
            }
            for c in &bx.children {
                walk(c, found);
            }
        }
        walk(&bx, &mut found);
        assert!(found, "currentColor should resolve to host's red");
    }

    #[test]
    fn text_overflow_ellipsis_truncates_long_label() {
        let (doc, style) = doc_from_html(
            "<style>.t{display:block;width:60px;\
             white-space:nowrap;overflow:hidden;text-overflow:ellipsis}</style>\
             <body><div class=t>This is a very long label</div></body>",
        );
        let t = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs.iter().any(|(k, v)| {
                            k == "class"
                                && v.split_whitespace().any(|c| c == "t")
                        })
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        let mut bx = build(&doc, &style, t);
        layout(&mut bx, 0.0, 0.0, 800.0);
        let mut text_runs: Vec<String> = Vec::new();
        fn collect(bx: &LayoutBox, out: &mut Vec<String>) {
            for line in &bx.lines {
                for item in &line.items {
                    if let LineItem::Text(run) = item {
                        out.push(run.text.clone());
                    }
                }
            }
            for c in &bx.children {
                collect(c, out);
            }
        }
        collect(&bx, &mut text_runs);
        let joined = text_runs.join("");
        assert!(
            joined.contains('\u{2026}'),
            "expected ellipsis, got: {joined}"
        );
        // The original full string mustn't survive intact.
        assert!(
            !joined.contains("very long label"),
            "expected truncation, got: {joined}"
        );
    }

    #[test]
    fn details_without_open_hides_non_summary_children() {
        // UA stylesheet has `details:not([open]) > :not(summary) {
        // display: none }`. A closed <details> should render only its
        // <summary>; the body content stays out of the layout tree.
        let (doc, style) = doc_from_html(
            "<body><details>\
             <summary>Click to expand</summary>\
             <p>Hidden body</p>\
             </details></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 600.0);
        let mut texts: Vec<String> = Vec::new();
        fn collect(bx: &LayoutBox, out: &mut Vec<String>) {
            for line in &bx.lines {
                for item in &line.items {
                    if let LineItem::Text(run) = item {
                        out.push(run.text.clone());
                    }
                }
            }
            for c in &bx.children {
                collect(c, out);
            }
        }
        collect(&bx, &mut texts);
        let joined = texts.join(" ");
        assert!(joined.contains("Click"), "summary should render: {joined}");
        assert!(
            !joined.contains("Hidden"),
            "body of closed details should be hidden: {joined}"
        );
    }

    #[test]
    fn details_with_open_renders_body() {
        let (doc, style) = doc_from_html(
            "<body><details open>\
             <summary>S</summary>\
             <p>Visible body</p>\
             </details></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 600.0);
        let mut texts: Vec<String> = Vec::new();
        fn collect2(bx: &LayoutBox, out: &mut Vec<String>) {
            for line in &bx.lines {
                for item in &line.items {
                    if let LineItem::Text(run) = item {
                        out.push(run.text.clone());
                    }
                }
            }
            for c in &bx.children {
                collect2(c, out);
            }
        }
        collect2(&bx, &mut texts);
        let joined = texts.join(" ");
        assert!(
            joined.contains("Visible"),
            "open details body should render: {joined}"
        );
    }

    #[test]
    fn grid_template_shorthand_sets_columns() {
        // Wikipedia's .mw-page-container-inner uses
        //   grid-template: <rows> / 12.25rem minmax(0, 1fr)
        // Without the shorthand parsed, both columns auto-split
        // evenly and the article body lands at 50% width. With
        // the shorthand parsing, the first column is 196px (=12.25
        // * 16) and the second column takes the rest.
        let (doc, style) = doc_from_html(
            "<style>.g{display:grid;\
             grid-template:auto / 12.25rem minmax(0,1fr);\
             grid-template-areas:'sidebar main'}\
             .s{grid-area:sidebar;display:block}\
             .m{grid-area:main;display:block}</style>\
             <body><div class=g>\
             <div class=s>sb</div><div class=m>main</div>\
             </div></body>",
        );
        let grid = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs.iter().any(|(k, v)| {
                            k == "class"
                                && v.split_whitespace().any(|c| c == "g")
                        })
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        let mut bx = build(&doc, &style, grid);
        layout(&mut bx, 0.0, 0.0, 1400.0);
        let cells: Vec<&LayoutBox> = bx
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        assert_eq!(cells.len(), 2);
        let sb = cells.iter().find(|c| c.style.grid_column_start.clone() == bui_style::GridLine::Named("sidebar".into())).expect("sidebar");
        let main = cells.iter().find(|c| c.style.grid_column_start.clone() == bui_style::GridLine::Named("main".into())).expect("main");
        // 12.25rem = 196px sidebar.
        assert!(
            (sb.frame.width - 196.0).abs() < 1.0,
            "sidebar 196px (12.25rem), got {}",
            sb.frame.width
        );
        // Main column gets the remainder = 1400 - 196 = 1204.
        assert!(
            main.frame.width > 1100.0,
            "main column should be wide, got {}",
            main.frame.width
        );
    }

    #[test]
    fn z_index_orders_paint_after_source_order() {
        // Two block siblings A (red) and B (blue) at the same
        // position. Without z-index, B paints over A. With
        // z-index: 5 on A and z-index: 1 on B, A paints over B.
        let (doc, style) = doc_from_html(
            "<style>body{position:relative}\
             .a,.b{position:absolute;width:50px;height:50px}\
             .a{background-color:red;z-index:5}\
             .b{background-color:blue;z-index:1}</style>\
             <body><div class=a></div><div class=b></div></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 600.0);
        let mut dl = DisplayList::new();
        paint(&bx, &mut dl);
        // Find the order of red (255,0,0) and blue (0,0,255) FillRect
        // commands in the display list.
        let mut red_idx = None;
        let mut blue_idx = None;
        for (i, c) in dl.commands.iter().enumerate() {
            if let bui_paint::PaintCommand::FillRect { color, .. } = c {
                if color.r == 255 && color.b == 0 && red_idx.is_none() {
                    red_idx = Some(i);
                }
                if color.b == 255 && color.r == 0 && blue_idx.is_none() {
                    blue_idx = Some(i);
                }
            }
        }
        let (r, b) = (red_idx.expect("red"), blue_idx.expect("blue"));
        assert!(b < r, "z-index 5 (red) should paint AFTER z-index 1 (blue)");
    }

    #[test]
    fn select_renders_selected_option_as_button() {
        // <select> with one <option selected> — should produce a
        // single Control with the selected option's text as label.
        let (doc, style) = doc_from_html(
            "<body><select>\
             <option>One</option>\
             <option selected>Two</option>\
             <option>Three</option>\
             </select></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 600.0);
        // Find the Control for the <select>.
        fn find_label(bx: &LayoutBox) -> Option<(String, ControlKind)> {
            for line in &bx.lines {
                for item in &line.items {
                    if let LineItem::Control { label, kind, .. } = item {
                        return Some((label.clone(), *kind));
                    }
                }
            }
            for c in &bx.children {
                if let Some(r) = find_label(c) {
                    return Some(r);
                }
            }
            None
        }
        let (label, kind) = find_label(&bx).expect("control");
        assert_eq!(label, "Two");
        assert!(matches!(kind, ControlKind::Button));
    }

    #[test]
    fn s_and_del_get_strikethrough_via_ua_stylesheet() {
        let (doc, style) = doc_from_html(
            "<body><p><s>old</s> new <del>removed</del></p></body>",
        );
        let p = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "p").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, p);
        layout(&mut bx, 0.0, 0.0, 800.0);
        // Find the runs for "old" and "removed" — both should have
        // text_decoration_line_through set.
        fn check(bx: &LayoutBox, target: &str) -> Option<bool> {
            for line in &bx.lines {
                for item in &line.items {
                    if let LineItem::Text(run) = item {
                        if run.text.contains(target) {
                            return Some(run.style.text_decoration_line_through);
                        }
                    }
                }
            }
            for c in &bx.children {
                if let Some(r) = check(c, target) {
                    return Some(r);
                }
            }
            None
        }
        assert_eq!(check(&bx, "old"), Some(true), "<s> should set line-through");
        assert_eq!(check(&bx, "removed"), Some(true), "<del> should set line-through");
        // The "new" text in plain <p> shouldn't have it.
        assert_eq!(check(&bx, "new"), Some(false));
    }

    #[test]
    fn checkbox_and_radio_get_indicator_kind_not_text_input() {
        let (doc, style) = doc_from_html(
            "<body><input type=\"checkbox\" checked>\
             <input type=\"checkbox\">\
             <input type=\"radio\" checked></body>",
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, body);
        layout(&mut bx, 0.0, 0.0, 600.0);
        // Walk for Control items + verify their kinds.
        let mut kinds: Vec<ControlKind> = Vec::new();
        fn walk(bx: &LayoutBox, out: &mut Vec<ControlKind>) {
            for line in &bx.lines {
                for item in &line.items {
                    if let LineItem::Control { kind, .. } = item {
                        out.push(*kind);
                    }
                }
            }
            for c in &bx.children {
                walk(c, out);
            }
        }
        walk(&bx, &mut kinds);
        assert_eq!(kinds.len(), 3);
        assert!(matches!(kinds[0], ControlKind::Checkbox { checked: true }));
        assert!(matches!(kinds[1], ControlKind::Checkbox { checked: false }));
        assert!(matches!(kinds[2], ControlKind::Radio { checked: true }));
    }

    #[test]
    fn table_honours_per_cell_declared_width() {
        // 2-column table at 600 content width, first row's first cell
        // declares width: 100px. Result: column 1 = 100, column 2 =
        // 500 (the rest); the second row's cells respect those widths.
        let (doc, style) = doc_from_html(
            "<style>table{width:600px}</style>\
             <body><table>\
             <tr><td style=\"width:100px\">L</td><td>R</td></tr>\
             <tr><td>x</td><td>y</td></tr>\
             </table></body>",
        );
        let table = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "table").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, table);
        layout(&mut bx, 0.0, 0.0, 800.0);
        // Walk to row 2's cells.
        fn cells_in_row<'a>(bx: &'a LayoutBox, row_index: usize, out: &mut Vec<&'a LayoutBox>) {
            // Collect rows then index into them.
            let mut rows: Vec<&LayoutBox> = Vec::new();
            fn collect_rows<'a>(b: &'a LayoutBox, out: &mut Vec<&'a LayoutBox>) {
                if matches!(b.style.display, Display::TableRow) {
                    out.push(b);
                } else {
                    for c in &b.children {
                        collect_rows(c, out);
                    }
                }
            }
            collect_rows(bx, &mut rows);
            if let Some(row) = rows.get(row_index) {
                for c in &row.children {
                    if matches!(c.style.display, Display::TableCell) {
                        out.push(c);
                    }
                }
            }
        }
        let mut row1: Vec<&LayoutBox> = Vec::new();
        cells_in_row(&bx, 0, &mut row1);
        let mut row2: Vec<&LayoutBox> = Vec::new();
        cells_in_row(&bx, 1, &mut row2);
        assert_eq!(row1.len(), 2);
        assert_eq!(row2.len(), 2);
        // Row 2's cell positions reflect the column widths (100 + 500).
        assert!(
            (row2[0].frame.x - 0.0).abs() < 0.5,
            "row 2 col 1 at x=0, got {}",
            row2[0].frame.x
        );
        assert!(
            (row2[1].frame.x - 100.0).abs() < 0.5,
            "row 2 col 2 at x=100, got {}",
            row2[1].frame.x
        );
        // The right column should be ~5x wider than the left.
        let ratio = row2[1].frame.width / row2[0].frame.width.max(1.0);
        assert!(
            ratio > 3.0,
            "right column should be much wider than narrow left, got ratio {ratio}"
        );
    }

    #[test]
    fn rowspan_skips_columns_in_following_row() {
        // 3-column table:
        //   row 1: A (rowspan=2)  B  C
        //   row 2:                 D  E
        // Without rowspan handling, D would land in column 1; with it,
        // D should sit in column 2 (where B was) and E in column 3.
        let (doc, style) = doc_from_html(
            "<style>table{width:300px}</style>\
             <body><table>\
             <tr><td rowspan=\"2\">A</td><td>B</td><td>C</td></tr>\
             <tr><td>D</td><td>E</td></tr>\
             </table></body>",
        );
        let table = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "table").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, table);
        layout(&mut bx, 0.0, 0.0, 600.0);
        // Pick out cells by their text run.
        fn collect_cells<'a>(bx: &'a LayoutBox, out: &mut Vec<(String, &'a LayoutBox)>) {
            if matches!(bx.style.display, Display::TableCell) {
                let mut texts: Vec<String> = Vec::new();
                fn collect_text(bx: &LayoutBox, out: &mut Vec<String>) {
                    for line in &bx.lines {
                        for item in &line.items {
                            if let LineItem::Text(run) = item {
                                out.push(run.text.clone());
                            }
                        }
                    }
                    for c in &bx.children {
                        collect_text(c, out);
                    }
                }
                collect_text(bx, &mut texts);
                out.push((texts.join(""), bx));
            }
            for c in &bx.children {
                collect_cells(c, out);
            }
        }
        let mut cells: Vec<(String, &LayoutBox)> = Vec::new();
        collect_cells(&bx, &mut cells);
        let by_label = |s: &str| -> &LayoutBox {
            cells.iter().find(|(t, _)| t == s).expect("cell").1
        };
        let a = by_label("A");
        let d = by_label("D");
        let e = by_label("E");
        // 300px / 3 cols = 100px per column. A at x=0, D at x=100 (skipping A's column), E at x=200.
        assert!((a.frame.x - 0.0).abs() < 0.5);
        assert!(
            (d.frame.x - 100.0).abs() < 0.5,
            "D should skip A's column, got x={}",
            d.frame.x
        );
        assert!(
            (e.frame.x - 200.0).abs() < 0.5,
            "E at column 3, got x={}",
            e.frame.x
        );
    }

    #[test]
    fn colspan_widens_cell_and_advances_cursor() {
        // Row 1: <td colspan="2"> — should span both columns.
        // Row 2: two normal <td>s — should land in column 1 and 2.
        let (doc, style) = doc_from_html(
            "<style>table{width:400px}</style>\
             <body><table>\
             <tr><td colspan=\"2\">H</td></tr>\
             <tr><td>L</td><td>R</td></tr>\
             </table></body>",
        );
        let table = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "table").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, table);
        layout(&mut bx, 0.0, 0.0, 600.0);
        // Walk to find each cell and check widths.
        fn find_cells<'a>(bx: &'a LayoutBox, out: &mut Vec<&'a LayoutBox>) {
            for c in &bx.children {
                if matches!(c.style.display, Display::TableCell) {
                    out.push(c);
                } else {
                    find_cells(c, out);
                }
            }
        }
        let mut cells: Vec<&LayoutBox> = Vec::new();
        find_cells(&bx, &mut cells);
        assert_eq!(cells.len(), 3);
        // Header (row 1, single cell, colspan=2) — full width 400.
        assert!(
            (cells[0].frame.width - 400.0).abs() < 0.5,
            "header colspan=2 should be 400, got {}",
            cells[0].frame.width
        );
        // Row 2 cells each take 200.
        assert!((cells[1].frame.width - 200.0).abs() < 0.5);
        assert!((cells[2].frame.width - 200.0).abs() < 0.5);
        // Right cell sits at x=200 (after the left).
        assert!((cells[2].frame.x - 200.0).abs() < 0.5);
    }

    #[test]
    fn text_shadow_emits_two_text_commands_per_run() {
        // `text-shadow: 2px 3px red` should paint a red shadow run
        // before the main run. Both Text commands carry the same
        // string, but the shadow's baseline is offset.
        let (doc, style) = doc_from_html(
            "<style>p{text-shadow: 2px 3px red}</style>\
             <body><p>Hi</p></body>",
        );
        let p = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "p").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, p);
        layout(&mut bx, 0.0, 0.0, 400.0);
        let mut dl = DisplayList::new();
        paint(&bx, &mut dl);
        // Count Text commands carrying "Hi".
        let count = dl
            .commands
            .iter()
            .filter(|c| {
                matches!(c, bui_paint::PaintCommand::Text { content, .. } if content == "Hi")
            })
            .count();
        assert_eq!(count, 2, "expected one shadow + one main Text command");
    }

    #[test]
    fn object_fit_cover_centres_and_clips() {
        // Box: 100x100. Intrinsic: 200x100. cover-scale = max(0.5, 1) = 1.
        // After cover, paint-rect = 200x100 centred → x=-50, y=0.
        let frame = Frame { x: 0.0, y: 0.0, width: 100.0, height: 100.0 };
        let (rect, clip) = compute_object_fit_rect(
            frame,
            (200.0, 100.0),
            bui_style::ObjectFit::Cover,
        );
        assert!(clip, "cover needs clipping");
        assert!((rect.width - 200.0).abs() < 0.01);
        assert!((rect.height - 100.0).abs() < 0.01);
        assert!((rect.x - (-50.0)).abs() < 0.01, "centre overflowing left");
    }

    #[test]
    fn object_fit_contain_letterboxes() {
        // Box: 100x200. Intrinsic: 100x100. contain-scale = min(1, 2) = 1.
        // After contain, paint-rect = 100x100 centred → x=0, y=50.
        let frame = Frame { x: 0.0, y: 0.0, width: 100.0, height: 200.0 };
        let (rect, clip) = compute_object_fit_rect(
            frame,
            (100.0, 100.0),
            bui_style::ObjectFit::Contain,
        );
        assert!(!clip, "contain fits inside; no clip needed");
        assert!((rect.width - 100.0).abs() < 0.01);
        assert!((rect.height - 100.0).abs() < 0.01);
        assert!((rect.y - 50.0).abs() < 0.01, "letterboxed vertically");
    }

    #[test]
    fn border_collapse_zeros_shared_cell_edges() {
        // 2x2 table with `border-collapse: collapse` — every cell
        // declares a 1px border. Internal cells should lose right
        // and bottom borders so adjacent edges don't double up.
        let (doc, style) = doc_from_html(
            "<style>.t{display:table;border-collapse:collapse}\
             .r{display:table-row}\
             .c{display:table-cell;border:1px solid black}</style>\
             <body><div class=t>\
             <div class=r><div class=c>A</div><div class=c>B</div></div>\
             <div class=r><div class=c>C</div><div class=c>D</div></div>\
             </div></body>",
        );
        let table = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs.iter().any(|(k, v)| {
                            k == "class"
                                && v.split_whitespace().any(|c| c == "t")
                        })
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        let mut bx = build(&doc, &style, table);
        layout(&mut bx, 0.0, 0.0, 600.0);
        // Walk down to cells and check border state.
        fn cells_at(bx: &LayoutBox, out: &mut Vec<(f32, f32)>) {
            // Collect (right, bottom) border widths in row-major order.
            for r in &bx.children {
                if matches!(r.style.display, Display::TableRow) {
                    for c in &r.children {
                        if matches!(c.style.display, Display::TableCell) {
                            let rb = match c.style.border.right {
                                bui_style::Length::Px(v) => v,
                                _ => -1.0,
                            };
                            let bb = match c.style.border.bottom {
                                bui_style::Length::Px(v) => v,
                                _ => -1.0,
                            };
                            out.push((rb, bb));
                        }
                    }
                } else {
                    cells_at(r, out);
                }
            }
        }
        let mut borders: Vec<(f32, f32)> = Vec::new();
        cells_at(&bx, &mut borders);
        assert_eq!(borders.len(), 4);
        // Cell A (row 0, col 0): right zeroed, bottom zeroed.
        assert_eq!(borders[0], (0.0, 0.0));
        // Cell B (row 0, col 1): right kept (1.0), bottom zeroed.
        assert_eq!(borders[1].0, 1.0);
        assert_eq!(borders[1].1, 0.0);
        // Cell C (row 1, col 0): right zeroed, bottom kept.
        assert_eq!(borders[2].0, 0.0);
        assert_eq!(borders[2].1, 1.0);
        // Cell D (row 1, col 1): both kept.
        assert_eq!(borders[3], (1.0, 1.0));
    }

    #[test]
    fn whitespace_text_node_between_inline_siblings_renders_as_space() {
        // Regression: a `" "` text node between two inline elements
        // used to collapse to "" and get dropped, producing
        // "AlphaBravo" instead of "Alpha Bravo". The CSS Normal
        // whitespace rule keeps a single space; line-edge stripping
        // happens later in the inline flow.
        let (doc, style) = doc_from_html(
            "<body><p><a>Alpha</a> <a>Bravo</a></p></body>",
        );
        let p = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "p").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, p);
        layout(&mut bx, 0.0, 0.0, 800.0);
        // Bravo must be horizontally to the right of Alpha by at
        // least Alpha's width plus a space.
        let mut alpha_right: f32 = 0.0;
        let mut bravo_left: f32 = f32::INFINITY;
        fn walk(bx: &LayoutBox, alpha: &mut f32, bravo: &mut f32) {
            for line in &bx.lines {
                for item in &line.items {
                    if let LineItem::Text(run) = item {
                        if run.text == "Alpha" {
                            *alpha = run.frame.x + run.frame.width;
                        }
                        if run.text == "Bravo" {
                            *bravo = run.frame.x;
                        }
                    }
                }
            }
            for c in &bx.children {
                walk(c, alpha, bravo);
            }
        }
        walk(&bx, &mut alpha_right, &mut bravo_left);
        assert!(alpha_right > 0.0, "Alpha didn't render");
        assert!(bravo_left.is_finite(), "Bravo didn't render");
        let gap = bravo_left - alpha_right;
        assert!(
            gap >= 2.0,
            "gap between Alpha and Bravo = {gap}, expected >= 2px"
        );
    }

    #[test]
    fn viewport_units_resolve_against_set_viewport() {
        bui_style::set_viewport(1000.0, 600.0);
        let (doc, style) = doc_from_html(
            "<style>.b{display:block;width:50vw;height:25vh}</style>\
             <body><div class=b>x</div></body>",
        );
        let b = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs.iter().any(|(k, v)| {
                            k == "class"
                                && v.split_whitespace().any(|c| c == "b")
                        })
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        let mut bx = build(&doc, &style, b);
        layout(&mut bx, 0.0, 0.0, 800.0);
        // 50vw of 1000 = 500. (Container width 800 doesn't matter for vw.)
        assert!(
            (bx.frame.width - 500.0).abs() < 0.5,
            "50vw at vp 1000 → 500, got {}",
            bx.frame.width
        );
        // Reset for other tests that don't expect viewport state.
        bui_style::set_viewport(0.0, 0.0);
    }

    #[test]
    fn grid_template_areas_places_named_children() {
        // The classic "header / sidebar+main / footer" page shell.
        // Children with `grid-area: <name>` should land in the
        // bounding box of cells with that name.
        let (doc, style) = doc_from_html(
            "<style>.g{display:grid;\
             grid-template-columns:200px 600px;\
             grid-template-rows:50px 400px 50px;\
             grid-template-areas:'header header' 'sidebar main' 'footer footer'}\
             .h{display:block;grid-area:header}\
             .s{display:block;grid-area:sidebar}\
             .m{display:block;grid-area:main}\
             .f{display:block;grid-area:footer}</style>\
             <body><div class=g>\
             <div class=h>H</div><div class=s>S</div>\
             <div class=m>M</div><div class=f>F</div>\
             </div></body>",
        );
        let grid = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs.iter().any(|(k, v)| {
                            k == "class"
                                && v.split_whitespace().any(|c| c == "g")
                        })
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        let mut bx = build(&doc, &style, grid);
        layout(&mut bx, 0.0, 0.0, 800.0);
        let mut cells: std::collections::HashMap<&str, &LayoutBox> =
            std::collections::HashMap::new();
        for c in &bx.children {
            if !matches!(c.kind, BoxKind::Block) {
                continue;
            }
            let Some(node) = c.node else { continue };
            let elem = doc.element(node).unwrap();
            let class = elem
                .attrs
                .iter()
                .find(|(k, _)| k == "class")
                .map(|(_, v)| v.as_str())
                .unwrap_or("");
            cells.insert(class, c);
        }
        let header = cells["h"];
        let sidebar = cells["s"];
        let main = cells["m"];
        let footer = cells["f"];
        // Header spans both columns at top → x=0, full width 800.
        assert!((header.frame.x - 0.0).abs() < 0.5, "header x");
        assert!((header.frame.width - 800.0).abs() < 0.5, "header width");
        // Sidebar in column 1, row 2.
        assert!((sidebar.frame.x - 0.0).abs() < 0.5);
        assert!((sidebar.frame.y - 50.0).abs() < 0.5, "sidebar y after 50px header");
        // Main in column 2, row 2 (x=200, after sidebar).
        assert!((main.frame.x - 200.0).abs() < 0.5, "main x after 200 sidebar");
        assert!((main.frame.y - 50.0).abs() < 0.5);
        // Footer spans both columns at bottom (y=450).
        assert!((footer.frame.x - 0.0).abs() < 0.5);
        assert!((footer.frame.width - 800.0).abs() < 0.5);
        assert!((footer.frame.y - 450.0).abs() < 0.5, "footer y");
    }

    #[test]
    fn min_picks_smaller_resolved_length() {
        // `width: min(100%, 1200px)` is the canonical "fluid up to a
        // cap" pattern. At 800 container, 100% (= 800) wins; at 2000
        // container, 1200 wins.
        let (doc, style) = doc_from_html(
            "<style>.b{display:block;width:min(100%, 1200px)}</style>\
             <body><div class=b>x</div></body>",
        );
        let b = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs.iter().any(|(k, v)| {
                            k == "class"
                                && v.split_whitespace().any(|c| c == "b")
                        })
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        let mut bx = build(&doc, &style, b);
        layout(&mut bx, 0.0, 0.0, 800.0);
        assert!((bx.frame.width - 800.0).abs() < 0.5, "min @ 800 → 800");
        let mut bx2 = build(&doc, &style, b);
        layout(&mut bx2, 0.0, 0.0, 2000.0);
        assert!(
            (bx2.frame.width - 1200.0).abs() < 0.5,
            "min @ 2000 → 1200, got {}",
            bx2.frame.width
        );
    }

    #[test]
    fn clamp_clips_to_bounds() {
        // clamp(min, val, max) at val = 50% under different bases.
        let (doc, style) = doc_from_html(
            "<style>.b{display:block;width:clamp(200px, 50%, 600px)}</style>\
             <body><div class=b>x</div></body>",
        );
        let b = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs.iter().any(|(k, v)| {
                            k == "class"
                                && v.split_whitespace().any(|c| c == "b")
                        })
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        // Container 300: 50% = 150 → clipped up to 200 by lower bound.
        let mut bx = build(&doc, &style, b);
        layout(&mut bx, 0.0, 0.0, 300.0);
        assert!(
            (bx.frame.width - 200.0).abs() < 0.5,
            "clamp lower bound: got {}",
            bx.frame.width
        );
        // Container 800: 50% = 400 → in range.
        let mut bx = build(&doc, &style, b);
        layout(&mut bx, 0.0, 0.0, 800.0);
        assert!(
            (bx.frame.width - 400.0).abs() < 0.5,
            "clamp middle: got {}",
            bx.frame.width
        );
        // Container 2000: 50% = 1000 → clipped down to 600.
        let mut bx = build(&doc, &style, b);
        layout(&mut bx, 0.0, 0.0, 2000.0);
        assert!(
            (bx.frame.width - 600.0).abs() < 0.5,
            "clamp upper bound: got {}",
            bx.frame.width
        );
    }

    #[test]
    fn calc_subtracts_pixels_from_percent_basis() {
        // Common shape from real stylesheets: a card that fills the
        // container width minus a margin reserve. With container 600,
        // calc(100% - 40px) should resolve to a 560px content box.
        let (doc, style) = doc_from_html(
            "<style>.card{display:block;width:calc(100% - 40px)}</style>\
             <body><div class=card>x</div></body>",
        );
        let card = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs.iter().any(|(k, v)| {
                            k == "class"
                                && v.split_whitespace().any(|c| c == "card")
                        })
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        let mut bx = build(&doc, &style, card);
        layout(&mut bx, 0.0, 0.0, 600.0);
        assert!(
            (bx.frame.width - 560.0).abs() < 0.5,
            "calc(100% - 40px) at container 600 should be 560, got {}",
            bx.frame.width
        );
    }

    #[test]
    fn grid_named_lines_resolve_explicit_placement() {
        // `grid-column-start: main` should resolve to the line
        // declared as `[main]` in grid-template-columns. Without
        // named-line support the item would auto-place at column 1
        // instead of column 2 (the line *after* the 100px sidebar).
        let (doc, style) = doc_from_html(
            "<style>.g{display:grid;\
             grid-template-columns:[start] 100px [main] 1fr [end]}\
             .c{display:block}\
             .body{grid-column-start:main}</style>\
             <body><div class=g>\
             <div class=c>S</div><div class=\"c body\">B</div>\
             </div></body>",
        );
        let grid = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs.iter().any(|(k, v)| {
                            k == "class"
                                && v.split_whitespace().any(|c| c == "g")
                        })
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        let mut bx = build(&doc, &style, grid);
        layout(&mut bx, 0.0, 0.0, 600.0);
        let cells: Vec<&LayoutBox> = bx
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        // S goes in column 1 (x=0); B is pinned via `main` to
        // column 2 (x=100).
        assert!((cells[0].frame.x - 0.0).abs() < 0.5);
        assert!(
            (cells[1].frame.x - 100.0).abs() < 0.5,
            "B at {} (expected 100.0)",
            cells[1].frame.x
        );
    }

    #[test]
    fn img_registered_as_svg_renders_via_inline_svg_path() {
        // The fetch path classifies a given <img> as either raster or
        // vector. When it's vector, the build phase should mint an
        // InlineSvg replaced box for the <img> rather than skipping it
        // (raster path) or trying to look up a missing ImageEntry.
        let (doc, style) = doc_from_html(
            "<body><p><img src=\"icon.svg\" alt=\"i\"></p></body>",
        );
        let img_node = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| e.name == "img")
                    .unwrap_or(false)
            })
            .unwrap();
        let mut svgs = SvgRegistry::new();
        svgs.insert(
            img_node,
            crate::svg::SvgEntry {
                width: 16.0,
                height: 16.0,
                view_box: (0.0, 0.0, 16.0, 16.0),
                shapes: Vec::new(),
                no_attr_size: false,
            },
        );
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        let mut bx = build_with_images(&doc, &style, &ImageRegistry::new(), &svgs, body);
        layout(&mut bx, 0.0, 0.0, 800.0);
        // Walk the laid-out tree looking for an InlineSvg line item
        // associated with our <img> node.
        fn has_svg_for(bx: &LayoutBox, node: NodeId) -> bool {
            for line in &bx.lines {
                for item in &line.items {
                    if let LineItem::Svg { node: Some(n), .. } = item {
                        if *n == node {
                            return true;
                        }
                    }
                }
            }
            bx.children.iter().any(|c| has_svg_for(c, node))
        }
        assert!(
            has_svg_for(&bx, img_node),
            "<img> with SVG registry entry should appear as an InlineSvg line item"
        );
    }

    #[test]
    fn paint_emits_text_commands() {
        let (doc, style) = doc_from_html("<p>hello</p>");
        let p = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "p").unwrap_or(false))
            .unwrap();
        let mut bx = build(&doc, &style, p);
        layout(&mut bx, 0.0, 0.0, 800.0);
        let mut dl = DisplayList::new();
        paint(&bx, &mut dl);
        assert!(
            dl.commands
                .iter()
                .any(|c| matches!(c, bui_paint::PaintCommand::Text { .. }))
        );
    }

    #[test]
    fn display_contents_promotes_grandchildren_to_flex_items() {
        // A `display: contents` wrapper between a flex container and
        // its 2 children should not consume flex-item-hood. Without
        // `display: contents` handling, the wrapper alone is the
        // single flex item; with it, both inner blocks are.
        let (doc, style) = doc_from_html(
            "<style>.f{display:flex;width:600px}\
             .w{display:contents}\
             .a,.b{display:block;width:100px}</style>\
             <body><div class=f>\
             <div class=w>\
             <div class=a>A</div><div class=b>B</div>\
             </div>\
             </div></body>",
        );
        let f = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs.iter().any(|(k, v)| {
                            k == "class" && v.split_whitespace().any(|c| c == "f")
                        })
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        let mut bx = build(&doc, &style, f);
        layout(&mut bx, 0.0, 0.0, 1000.0);
        let blocks: Vec<&LayoutBox> = bx
            .children
            .iter()
            .filter(|c| matches!(c.kind, BoxKind::Block))
            .collect();
        assert_eq!(
            blocks.len(),
            2,
            "expected 2 flex items after flattening display:contents wrapper",
        );
        // Both should sit at y=0 with non-overlapping x ranges.
        assert_eq!(blocks[0].frame.y, blocks[1].frame.y);
        assert!(blocks[1].frame.x >= blocks[0].frame.x + blocks[0].frame.width);
    }

    #[test]
    fn calc_percent_on_height_resolves_against_parent_height() {
        // Regression: Google's logo container is a column flex with
        // max-height: 230px containing a spacer
        // `<div style="height: calc(100% - 200px)">` and an image.
        // Before the container_h plumb-through, percent-in-calc
        // resolved against container_w (e.g., 1304), making the
        // spacer 1100px tall and pushing the logo off-screen.
        // The fix passes the parent's max-height-clamped 230 as the
        // child's basis, yielding the spec-correct 30 px.
        let (doc, style) = doc_from_html(
            "<style>.k{display:flex;flex-direction:column;max-height:230px;width:600px}\
             .s{height:calc(100% - 200px)}</style>\
             <body><div class=k>\
             <div class=s></div>\
             <div>logo</div>\
             </div></body>",
        );
        let parent = doc
            .descendants(doc.root)
            .find(|n| {
                doc.element(*n)
                    .map(|e| {
                        e.attrs.iter().any(|(k, v)| {
                            k == "class" && v.split_whitespace().any(|c| c == "k")
                        })
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        let mut bx = build(&doc, &style, parent);
        layout(&mut bx, 0.0, 0.0, 1300.0);
        // First child is the spacer; its height should be 30, not
        // ~1100. We allow a 1-px slop for rounding.
        let spacer = &bx.children[0];
        assert!(
            (spacer.frame.height - 30.0).abs() < 1.0,
            "spacer height {} did not resolve calc(100% - 200px) against parent's 230px max-height",
            spacer.frame.height,
        );
    }
}
