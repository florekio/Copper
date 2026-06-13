//! Inline SVG → display-list lowering.
//!
//! Walks an `<svg>` element and its children, and produces a
//! `(view_box, [PaintCommand::Svg-ready data])` representation that
//! `bui-layout` plugs into a replaced inline box. The renderer
//! turns each `Shape` into a vello `BezPath`.
//!
//! Supported shapes:
//!   * `<path d="...">` — M, L, H, V, C, S, Q, T, Z (uppercase /
//!     lowercase = absolute / relative). Arcs (`A`/`a`) are skipped
//!     because they need an ellipse → cubic-bezier expansion.
//!   * `<rect>` (with optional `rx` / `ry` corner radii)
//!   * `<circle>` (cubic-bezier 4-arc approximation)
//!   * `<ellipse>` (cubic-bezier 4-arc approximation)
//!   * `<line>`, `<polyline>`, `<polygon>`
//!
//! Style: per-shape `fill="..."`, `stroke="..."`, `stroke-width="..."`,
//! plus the same attributes inherited from a `<g>` ancestor. `fill="none"`
//! disables the fill. Default fill is black per SVG spec.

use bui_dom::{Document, NodeId, NodeKind};
use bui_paint::{Color, PathSegment};
use bui_style::RgbaColor;

#[derive(Debug, Clone)]
pub struct SvgEntry {
    /// Intrinsic CSS-pixel size for layout. Pulled from `width`/`height`
    /// attributes; falls back to viewBox dimensions; then to a 24x24
    /// glyph-size default.
    pub width: f32,
    pub height: f32,
    /// User-space rectangle the shapes are drawn in. Defaults to
    /// `(0, 0, width, height)` when no `viewBox` is set.
    pub view_box: (f32, f32, f32, f32),
    pub shapes: Vec<SvgShape>,
    /// True when neither HTML width nor height attribute was declared
    /// on the source `<svg>` — only viewBox supplied the dimensions.
    /// Layout uses this to decide whether to honour a giant
    /// material-icon viewBox literally or treat it as an icon and
    /// scale to font-size when CSS doesn't size it.
    pub no_attr_size: bool,
}

#[derive(Debug, Clone)]
pub struct SvgShape {
    pub segments: Vec<PathSegment>,
    pub fill: Option<Color>,
    pub stroke: Option<Color>,
    pub stroke_width: f32,
}

/// Parse an `<svg>` element into an `SvgEntry`. `currentColor` (and
/// `fill="currentColor"`) resolves to black; use
/// `parse_svg_with_color` to inherit the surrounding text colour.
pub fn parse_svg(doc: &Document, node: NodeId) -> Option<SvgEntry> {
    parse_svg_with_color(doc, node, Color::BLACK)
}

/// Parse an `<svg>` element with an explicit `current_color`. Any
/// `fill="currentColor"` (or unset fill that defaults to inherit)
/// resolves to this colour — matches CSS's `color` inheritance for
/// SVG icons embedded inside text-coloured elements.
pub fn parse_svg_with_color(
    doc: &Document,
    node: NodeId,
    current_color: Color,
) -> Option<SvgEntry> {
    let elem = doc.element(node)?;
    if elem.name != "svg" {
        return None;
    }
    let view_box = elem
        .get_attr("viewBox")
        .or_else(|| elem.get_attr("viewbox"))
        .and_then(parse_view_box);
    let attr_w = elem.get_attr("width").and_then(parse_dim);
    let attr_h = elem.get_attr("height").and_then(parse_dim);
    let (vbx, vby, vbw, vbh) = view_box.unwrap_or_else(|| {
        let w = attr_w.unwrap_or(24.0);
        let h = attr_h.unwrap_or(24.0);
        (0.0, 0.0, w, h)
    });
    let width = attr_w.unwrap_or(vbw);
    let height = attr_h.unwrap_or(vbh);
    let no_attr_size = attr_w.is_none() && attr_h.is_none();

    let mut shapes = Vec::new();
    let inherit = SvgStyle {
        fill: FillState::Color(current_color),
        stroke: None,
        stroke_width: 1.0,
        current_color,
    };
    walk(doc, node, &inherit, &mut shapes);
    Some(SvgEntry {
        width,
        height,
        view_box: (vbx, vby, vbw, vbh),
        shapes,
        no_attr_size,
    })
}

#[derive(Debug, Clone, Copy)]
struct SvgStyle {
    fill: FillState,
    stroke: Option<Color>,
    stroke_width: f32,
    /// CSS `color` of the SVG's host element. Used to resolve
    /// `fill="currentColor"` / `stroke="currentColor"`.
    current_color: Color,
}

#[derive(Debug, Clone, Copy)]
enum FillState {
    /// `fill="none"` — explicitly suppress the fill.
    None,
    /// Inherit from parent if any, default black at the root.
    Inherit,
    /// Explicit colour set on this element or an ancestor.
    Color(Color),
}

impl Default for SvgStyle {
    fn default() -> Self {
        Self {
            // Root `<svg>` defaults to black fill, no stroke per SVG spec.
            fill: FillState::Color(Color::BLACK),
            stroke: None,
            stroke_width: 1.0,
            current_color: Color::BLACK,
        }
    }
}

impl SvgStyle {
    fn merge_attrs(&self, attrs: &[(String, String)]) -> Self {
        let mut out = *self;
        for (k, v) in attrs {
            match k.to_ascii_lowercase().as_str() {
                "fill" => {
                    if v.eq_ignore_ascii_case("none") {
                        out.fill = FillState::None;
                    } else if v.eq_ignore_ascii_case("currentcolor") {
                        out.fill = FillState::Color(self.current_color);
                    } else if let Some(c) = parse_svg_color(v) {
                        out.fill = FillState::Color(c);
                    } else if v.trim_start().starts_with("var(") {
                        // CSS custom-property reference like
                        // `fill="var(--bbQxAb)"`. We don't carry
                        // the cascade vars into SVG parsing, so
                        // fall back to currentColor — same as
                        // browsers do when the var is undefined.
                        // Google's mic / camera icons ride this
                        // path and render in the host element's
                        // text color (dark gray) instead of black.
                        out.fill = FillState::Color(self.current_color);
                    }
                }
                "stroke" => {
                    if v.eq_ignore_ascii_case("none") {
                        out.stroke = None;
                    } else if v.eq_ignore_ascii_case("currentcolor") {
                        out.stroke = Some(self.current_color);
                    } else if let Some(c) = parse_svg_color(v) {
                        out.stroke = Some(c);
                    } else if v.trim_start().starts_with("var(") {
                        out.stroke = Some(self.current_color);
                    }
                }
                "stroke-width" => {
                    if let Some(w) = parse_dim(v) {
                        out.stroke_width = w;
                    }
                }
                _ => {}
            }
        }
        // CSS `style="..."` mini-parser — only fill / stroke / stroke-width.
        for (k, v) in attrs {
            if k.eq_ignore_ascii_case("style") {
                for decl in v.split(';') {
                    let Some((name, val)) = decl.split_once(':') else {
                        continue;
                    };
                    let pair = (name.trim().to_string(), val.trim().to_string());
                    out = out.merge_attrs(std::slice::from_ref(&pair));
                }
            }
        }
        out
    }

    fn fill_color(&self) -> Option<Color> {
        match self.fill {
            FillState::None => None,
            FillState::Color(c) => Some(c),
            FillState::Inherit => Some(self.current_color),
        }
    }
}

fn walk(doc: &Document, node: NodeId, inherit: &SvgStyle, out: &mut Vec<SvgShape>) {
    let mut child = doc.node(node).first_child;
    while let Some(c) = child {
        if let NodeKind::Element(elem) = &doc.node(c).kind {
            let style = inherit.merge_attrs(&elem.attrs);
            match elem.name.as_str() {
                "g" => walk(doc, c, &style, out),
                "path" => {
                    if let Some(d) = elem.get_attr("d") {
                        let segments = parse_path_d(d);
                        push_shape(out, segments, &style);
                    }
                }
                "rect" => {
                    let x = elem.get_attr("x").and_then(parse_dim).unwrap_or(0.0);
                    let y = elem.get_attr("y").and_then(parse_dim).unwrap_or(0.0);
                    let w = elem.get_attr("width").and_then(parse_dim).unwrap_or(0.0);
                    let h = elem.get_attr("height").and_then(parse_dim).unwrap_or(0.0);
                    let rx = elem.get_attr("rx").and_then(parse_dim).unwrap_or(0.0);
                    let ry = elem.get_attr("ry").and_then(parse_dim).unwrap_or(rx);
                    let segments = rect_path(x, y, w, h, rx, ry);
                    push_shape(out, segments, &style);
                }
                "circle" => {
                    let cx = elem.get_attr("cx").and_then(parse_dim).unwrap_or(0.0);
                    let cy = elem.get_attr("cy").and_then(parse_dim).unwrap_or(0.0);
                    let r = elem.get_attr("r").and_then(parse_dim).unwrap_or(0.0);
                    let segments = ellipse_path(cx, cy, r, r);
                    push_shape(out, segments, &style);
                }
                "ellipse" => {
                    let cx = elem.get_attr("cx").and_then(parse_dim).unwrap_or(0.0);
                    let cy = elem.get_attr("cy").and_then(parse_dim).unwrap_or(0.0);
                    let rx = elem.get_attr("rx").and_then(parse_dim).unwrap_or(0.0);
                    let ry = elem.get_attr("ry").and_then(parse_dim).unwrap_or(0.0);
                    let segments = ellipse_path(cx, cy, rx, ry);
                    push_shape(out, segments, &style);
                }
                "line" => {
                    let x1 = elem.get_attr("x1").and_then(parse_dim).unwrap_or(0.0);
                    let y1 = elem.get_attr("y1").and_then(parse_dim).unwrap_or(0.0);
                    let x2 = elem.get_attr("x2").and_then(parse_dim).unwrap_or(0.0);
                    let y2 = elem.get_attr("y2").and_then(parse_dim).unwrap_or(0.0);
                    let segments = vec![PathSegment::MoveTo(x1, y1), PathSegment::LineTo(x2, y2)];
                    push_shape(out, segments, &style);
                }
                "polyline" => {
                    if let Some(points) = elem.get_attr("points") {
                        let segments = points_path(points, false);
                        push_shape(out, segments, &style);
                    }
                }
                "polygon" => {
                    if let Some(points) = elem.get_attr("points") {
                        let segments = points_path(points, true);
                        push_shape(out, segments, &style);
                    }
                }
                "use" => {
                    // <use href="#id"> dereferences a <symbol> / <g>
                    // elsewhere in the document. Wikipedia chrome uses
                    // sprite-sheet patterns; without dereference these
                    // render blank. Use uses xlink:href in older SVG
                    // and href in SVG 2 — accept both.
                    let href = elem
                        .get_attr("href")
                        .or_else(|| elem.get_attr("xlink:href"));
                    if let Some(h) = href {
                        if let Some(id) = h.strip_prefix('#') {
                            if let Some(target) = find_element_by_id(doc, doc.root, id) {
                                walk(doc, target, &style, out);
                            }
                        }
                    }
                }
                "symbol" | "defs" => {
                    // Hidden by default — entries are referenced via
                    // <use>. Don't recurse into their shapes during the
                    // top-level walk.
                }
                _ => {} // text, image, etc.: skip for now
            }
        }
        child = doc.node(c).next_sibling;
    }
}

/// Walk the document looking for an element whose `id` attribute
/// equals `target_id`. Used by `<use href="#id">` resolution. We
/// don't have a global id index, so this is a linear walk per
/// reference — fine for the typical small number of `<use>`s on
/// a page.
fn find_element_by_id(doc: &Document, root: NodeId, target_id: &str) -> Option<NodeId> {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if let Some(elem) = doc.element(n) {
            if let Some(id) = elem.get_attr("id") {
                if id == target_id {
                    return Some(n);
                }
            }
        }
        let mut child = doc.node(n).first_child;
        while let Some(c) = child {
            stack.push(c);
            child = doc.node(c).next_sibling;
        }
    }
    None
}

fn push_shape(out: &mut Vec<SvgShape>, segments: Vec<PathSegment>, style: &SvgStyle) {
    if segments.is_empty() {
        return;
    }
    if style.fill_color().is_none() && style.stroke.is_none() {
        return;
    }
    out.push(SvgShape {
        segments,
        fill: style.fill_color().map(rgba_to_paint),
        stroke: style.stroke.map(|c| {
            // Color is already a paint::Color here.
            c
        }),
        stroke_width: style.stroke_width,
    });
}

fn rgba_to_paint(c: Color) -> Color {
    c
}

fn parse_view_box(v: &str) -> Option<(f32, f32, f32, f32)> {
    let parts: Vec<f32> = v
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse().ok())
        .collect();
    if parts.len() != 4 {
        return None;
    }
    Some((parts[0], parts[1], parts[2], parts[3]))
}

fn parse_dim(v: &str) -> Option<f32> {
    let v = v.trim();
    let v = v
        .strip_suffix("px")
        .or_else(|| v.strip_suffix("pt"))
        .unwrap_or(v);
    v.parse().ok()
}

/// SVG colour grammar. Supports `#RGB`, `#RRGGBB`, `rgb(...)` and
/// the most common named colours. Everything we don't recognise (e.g.
/// `currentColor`, gradient URLs) falls through as `None`; the caller
/// keeps the inherited fill in that case.
fn parse_svg_color(v: &str) -> Option<Color> {
    let v = v.trim();
    if let Some(rest) = v.strip_prefix('#') {
        return parse_hex(rest);
    }
    if let Some(stripped) = v.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let parts: Vec<&str> = stripped.split(',').map(str::trim).collect();
        if parts.len() != 3 {
            return None;
        }
        let r = parts[0].parse::<u8>().ok()?;
        let g = parts[1].parse::<u8>().ok()?;
        let b = parts[2].parse::<u8>().ok()?;
        return Some(Color::rgb(r, g, b));
    }
    named_color(v)
}

fn named_color(name: &str) -> Option<Color> {
    Some(match name.to_ascii_lowercase().as_str() {
        "transparent" => Color::TRANSPARENT,
        "black" => Color::BLACK,
        "white" => Color::WHITE,
        "red" => Color::rgb(255, 0, 0),
        "green" => Color::rgb(0, 128, 0),
        "blue" => Color::rgb(0, 0, 255),
        "yellow" => Color::rgb(255, 255, 0),
        "cyan" | "aqua" => Color::rgb(0, 255, 255),
        "magenta" | "fuchsia" => Color::rgb(255, 0, 255),
        "silver" => Color::rgb(192, 192, 192),
        "gray" | "grey" => Color::rgb(128, 128, 128),
        "lightgray" | "lightgrey" => Color::rgb(211, 211, 211),
        "darkgray" | "darkgrey" => Color::rgb(169, 169, 169),
        "maroon" => Color::rgb(128, 0, 0),
        "olive" => Color::rgb(128, 128, 0),
        "purple" => Color::rgb(128, 0, 128),
        "teal" => Color::rgb(0, 128, 128),
        "navy" => Color::rgb(0, 0, 128),
        "orange" => Color::rgb(255, 165, 0),
        "pink" => Color::rgb(255, 192, 203),
        // Brand-y greys common in icons.
        "currentcolor" => return None, // upstream resolves
        _ => return None,
    })
}

fn parse_hex(rest: &str) -> Option<Color> {
    fn h(b: u8) -> Option<u8> {
        Some(match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        })
    }
    let bytes = rest.as_bytes();
    match bytes.len() {
        3 => Some(Color::rgb(
            h(bytes[0])? * 17,
            h(bytes[1])? * 17,
            h(bytes[2])? * 17,
        )),
        6 => Some(Color::rgb(
            (h(bytes[0])? << 4) | h(bytes[1])?,
            (h(bytes[2])? << 4) | h(bytes[3])?,
            (h(bytes[4])? << 4) | h(bytes[5])?,
        )),
        _ => None,
    }
}

/// SVG path `d` data parser.
///
/// Implemented commands (uppercase = absolute, lowercase = relative):
///   M / L / H / V / C / S / Q / T / Z
/// `A`/`a` (elliptical arcs) are skipped: a real implementation needs
/// to convert each arc into 1–4 cubic bezier segments, which is more
/// code than the rest of this parser combined. We log nothing — just
/// drop the arc segment so parsing continues.
pub fn parse_path_d(d: &str) -> Vec<PathSegment> {
    let mut out: Vec<PathSegment> = Vec::new();
    let mut p = PathParser::new(d);
    let mut pen = (0.0_f32, 0.0_f32);
    let mut start = (0.0_f32, 0.0_f32);
    let mut last_cubic_c2: Option<(f32, f32)> = None;
    let mut last_quad_c: Option<(f32, f32)> = None;
    let mut last_cmd: Option<u8> = None;
    while let Some(cmd) = p.next_cmd() {
        match cmd {
            b'M' | b'm' => {
                // Subsequent coord pairs after a move are interpreted as
                // implicit line-tos (per SVG spec).
                let mut first = true;
                while let Some((x, y)) = p.next_pair() {
                    let (ax, ay) = if cmd == b'm' {
                        (pen.0 + x, pen.1 + y)
                    } else {
                        (x, y)
                    };
                    if first {
                        out.push(PathSegment::MoveTo(ax, ay));
                        start = (ax, ay);
                    } else {
                        out.push(PathSegment::LineTo(ax, ay));
                    }
                    pen = (ax, ay);
                    first = false;
                    if !p.peek_arg() {
                        break;
                    }
                }
                last_cubic_c2 = None;
                last_quad_c = None;
            }
            b'L' | b'l' => {
                while let Some((x, y)) = p.next_pair() {
                    let (ax, ay) = if cmd == b'l' {
                        (pen.0 + x, pen.1 + y)
                    } else {
                        (x, y)
                    };
                    out.push(PathSegment::LineTo(ax, ay));
                    pen = (ax, ay);
                    if !p.peek_arg() {
                        break;
                    }
                }
                last_cubic_c2 = None;
                last_quad_c = None;
            }
            b'H' | b'h' => {
                while let Some(x) = p.next_num() {
                    let ax = if cmd == b'h' { pen.0 + x } else { x };
                    out.push(PathSegment::LineTo(ax, pen.1));
                    pen = (ax, pen.1);
                    if !p.peek_arg() {
                        break;
                    }
                }
                last_cubic_c2 = None;
                last_quad_c = None;
            }
            b'V' | b'v' => {
                while let Some(y) = p.next_num() {
                    let ay = if cmd == b'v' { pen.1 + y } else { y };
                    out.push(PathSegment::LineTo(pen.0, ay));
                    pen = (pen.0, ay);
                    if !p.peek_arg() {
                        break;
                    }
                }
                last_cubic_c2 = None;
                last_quad_c = None;
            }
            b'C' | b'c' => {
                while let (Some(c1), Some(c2), Some(end)) =
                    (p.next_pair(), p.next_pair(), p.next_pair())
                {
                    let ac1 = if cmd == b'c' {
                        (pen.0 + c1.0, pen.1 + c1.1)
                    } else {
                        c1
                    };
                    let ac2 = if cmd == b'c' {
                        (pen.0 + c2.0, pen.1 + c2.1)
                    } else {
                        c2
                    };
                    let aend = if cmd == b'c' {
                        (pen.0 + end.0, pen.1 + end.1)
                    } else {
                        end
                    };
                    out.push(PathSegment::CurveTo {
                        c1: ac1,
                        c2: ac2,
                        end: aend,
                    });
                    pen = aend;
                    last_cubic_c2 = Some(ac2);
                    if !p.peek_arg() {
                        break;
                    }
                }
                last_quad_c = None;
            }
            b'S' | b's' => {
                while let (Some(c2), Some(end)) = (p.next_pair(), p.next_pair()) {
                    let reflected = match last_cubic_c2 {
                        Some((rx, ry)) => (2.0 * pen.0 - rx, 2.0 * pen.1 - ry),
                        None => pen,
                    };
                    let ac2 = if cmd == b's' {
                        (pen.0 + c2.0, pen.1 + c2.1)
                    } else {
                        c2
                    };
                    let aend = if cmd == b's' {
                        (pen.0 + end.0, pen.1 + end.1)
                    } else {
                        end
                    };
                    out.push(PathSegment::CurveTo {
                        c1: reflected,
                        c2: ac2,
                        end: aend,
                    });
                    pen = aend;
                    last_cubic_c2 = Some(ac2);
                    if !p.peek_arg() {
                        break;
                    }
                }
                last_quad_c = None;
            }
            b'Q' | b'q' => {
                while let (Some(c), Some(end)) = (p.next_pair(), p.next_pair()) {
                    let ac = if cmd == b'q' {
                        (pen.0 + c.0, pen.1 + c.1)
                    } else {
                        c
                    };
                    let aend = if cmd == b'q' {
                        (pen.0 + end.0, pen.1 + end.1)
                    } else {
                        end
                    };
                    out.push(PathSegment::QuadTo { c: ac, end: aend });
                    pen = aend;
                    last_quad_c = Some(ac);
                    if !p.peek_arg() {
                        break;
                    }
                }
                last_cubic_c2 = None;
            }
            b'T' | b't' => {
                while let Some(end) = p.next_pair() {
                    let reflected = match last_quad_c {
                        Some((rx, ry)) => (2.0 * pen.0 - rx, 2.0 * pen.1 - ry),
                        None => pen,
                    };
                    let aend = if cmd == b't' {
                        (pen.0 + end.0, pen.1 + end.1)
                    } else {
                        end
                    };
                    out.push(PathSegment::QuadTo {
                        c: reflected,
                        end: aend,
                    });
                    pen = aend;
                    last_quad_c = Some(reflected);
                    if !p.peek_arg() {
                        break;
                    }
                }
                last_cubic_c2 = None;
            }
            b'Z' | b'z' => {
                out.push(PathSegment::Close);
                pen = start;
                last_cubic_c2 = None;
                last_quad_c = None;
            }
            b'A' | b'a' => {
                // Elliptical arc: rx ry x-rot large-arc-flag sweep-flag x y.
                // The two flags are single 0/1 digits (possibly unseparated),
                // so they MUST use next_flag, not next_num. Each arc is
                // converted to cubic beziers — circles/rounded shapes in
                // logos & icons (DuckDuckGo's duck disc) draw with arcs.
                while let Some(rx) = p.next_num() {
                    let ry = p.next_num();
                    let rot = p.next_num();
                    let laf = p.next_flag();
                    let sf = p.next_flag();
                    let x = p.next_num();
                    let y = p.next_num();
                    if let (Some(ry), Some(rot), Some(laf), Some(sf), Some(x), Some(y)) =
                        (ry, rot, laf, sf, x, y)
                    {
                        let end = if cmd == b'a' {
                            (pen.0 + x, pen.1 + y)
                        } else {
                            (x, y)
                        };
                        push_arc(&mut out, pen, rx, ry, rot, laf, sf, end);
                        pen = end;
                    } else {
                        break;
                    }
                    if !p.peek_arg() {
                        break;
                    }
                }
                last_cubic_c2 = None;
                last_quad_c = None;
            }
            _ => {} // unknown, skip
        }
        last_cmd = Some(cmd);
        let _ = last_cmd;
    }
    out
}

struct PathParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> PathParser<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            bytes: s.as_bytes(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        while let Some(&b) = self.bytes.get(self.pos) {
            if b == b',' || b.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn next_cmd(&mut self) -> Option<u8> {
        self.skip_ws();
        let &b = self.bytes.get(self.pos)?;
        if b.is_ascii_alphabetic() {
            self.pos += 1;
            Some(b)
        } else {
            None
        }
    }

    /// True if the next token looks like a number (a digit, sign, or `.`).
    fn peek_arg(&mut self) -> bool {
        self.skip_ws();
        match self.bytes.get(self.pos) {
            Some(&b) => b.is_ascii_digit() || b == b'-' || b == b'+' || b == b'.',
            None => false,
        }
    }

    fn next_num(&mut self) -> Option<f32> {
        self.skip_ws();
        let start = self.pos;
        if let Some(&b) = self.bytes.get(self.pos) {
            if b == b'-' || b == b'+' {
                self.pos += 1;
            }
        }
        let mut saw_digit = false;
        while let Some(&b) = self.bytes.get(self.pos) {
            if b.is_ascii_digit() {
                self.pos += 1;
                saw_digit = true;
            } else {
                break;
            }
        }
        if let Some(&b'.') = self.bytes.get(self.pos) {
            self.pos += 1;
            while let Some(&b) = self.bytes.get(self.pos) {
                if b.is_ascii_digit() {
                    self.pos += 1;
                    saw_digit = true;
                } else {
                    break;
                }
            }
        }
        if let Some(&b) = self.bytes.get(self.pos) {
            if b == b'e' || b == b'E' {
                self.pos += 1;
                if let Some(&s) = self.bytes.get(self.pos) {
                    if s == b'-' || s == b'+' {
                        self.pos += 1;
                    }
                }
                while let Some(&b) = self.bytes.get(self.pos) {
                    if b.is_ascii_digit() {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
            }
        }
        if !saw_digit {
            self.pos = start;
            return None;
        }
        let s = std::str::from_utf8(&self.bytes[start..self.pos]).ok()?;
        s.parse::<f32>().ok()
    }

    fn next_pair(&mut self) -> Option<(f32, f32)> {
        let x = self.next_num()?;
        let y = self.next_num()?;
        Some((x, y))
    }

    /// Read an arc flag: a single `0` or `1`. In the arc command the
    /// large-arc and sweep flags are single digits that may run together
    /// with no separator (`...0 11,0...`), so they can't go through
    /// `next_num` (which would swallow `11` as one number).
    fn next_flag(&mut self) -> Option<bool> {
        self.skip_ws();
        match self.bytes.get(self.pos) {
            Some(&b'0') => { self.pos += 1; Some(false) }
            Some(&b'1') => { self.pos += 1; Some(true) }
            _ => None,
        }
    }
}

/// Convert one SVG elliptical arc (endpoint parameterization) into cubic
/// bezier `CurveTo` segments appended to `out`. Implements the W3C SVG
/// arc→center conversion (Implementation Notes F.6), splitting the sweep
/// into ≤90° pieces each approximated by a cubic. `start`/`end` are
/// absolute. A degenerate arc (zero radius) falls back to a line.
fn push_arc(
    out: &mut Vec<PathSegment>,
    start: (f32, f32),
    rx: f32,
    ry: f32,
    x_axis_rotation_deg: f32,
    large_arc: bool,
    sweep: bool,
    end: (f32, f32),
) {
    let (x1, y1) = start;
    let (x2, y2) = end;
    let mut rx = rx.abs();
    let mut ry = ry.abs();
    if rx < 1e-6 || ry < 1e-6 || (x1 - x2).abs() < 1e-9 && (y1 - y2).abs() < 1e-9 {
        out.push(PathSegment::LineTo(x2, y2));
        return;
    }
    let phi = x_axis_rotation_deg.to_radians();
    let (cos_p, sin_p) = (phi.cos(), phi.sin());
    // Step 1: midpoint in the rotated frame.
    let dx = (x1 - x2) / 2.0;
    let dy = (y1 - y2) / 2.0;
    let x1p = cos_p * dx + sin_p * dy;
    let y1p = -sin_p * dx + cos_p * dy;
    // Step 2: correct out-of-range radii.
    let lambda = (x1p * x1p) / (rx * rx) + (y1p * y1p) / (ry * ry);
    if lambda > 1.0 {
        let s = lambda.sqrt();
        rx *= s;
        ry *= s;
    }
    // Step 3: center in the rotated frame.
    let num = (rx * rx) * (ry * ry) - (rx * rx) * (y1p * y1p) - (ry * ry) * (x1p * x1p);
    let den = (rx * rx) * (y1p * y1p) + (ry * ry) * (x1p * x1p);
    let mut coef = if den > 0.0 { (num / den).max(0.0).sqrt() } else { 0.0 };
    if large_arc == sweep {
        coef = -coef;
    }
    let cxp = coef * (rx * y1p) / ry;
    let cyp = coef * -(ry * x1p) / rx;
    // Step 4: center in the original frame.
    let cx = cos_p * cxp - sin_p * cyp + (x1 + x2) / 2.0;
    let cy = sin_p * cxp + cos_p * cyp + (y1 + y2) / 2.0;
    // Step 5: start angle + sweep angle.
    let ang = |ux: f32, uy: f32, vx: f32, vy: f32| -> f32 {
        let dot = ux * vx + uy * vy;
        let len = ((ux * ux + uy * uy) * (vx * vx + vy * vy)).sqrt();
        let mut a = (dot / len).clamp(-1.0, 1.0).acos();
        if ux * vy - uy * vx < 0.0 {
            a = -a;
        }
        a
    };
    let ux = (x1p - cxp) / rx;
    let uy = (y1p - cyp) / ry;
    let vx = (-x1p - cxp) / rx;
    let vy = (-y1p - cyp) / ry;
    let theta1 = ang(1.0, 0.0, ux, uy);
    let mut dtheta = ang(ux, uy, vx, vy);
    if !sweep && dtheta > 0.0 {
        dtheta -= std::f32::consts::TAU;
    } else if sweep && dtheta < 0.0 {
        dtheta += std::f32::consts::TAU;
    }
    // Split into ≤90° segments; cubic-approximate each.
    let n = (dtheta.abs() / (std::f32::consts::FRAC_PI_2)).ceil().max(1.0) as usize;
    let seg = dtheta / n as f32;
    let t = (4.0 / 3.0) * (seg / 4.0).tan();
    let mut a0 = theta1;
    for _ in 0..n {
        let a1 = a0 + seg;
        let (cos0, sin0) = (a0.cos(), a0.sin());
        let (cos1, sin1) = (a1.cos(), a1.sin());
        // Points + tangent control points on the unit ellipse, then
        // scale by rx/ry and rotate by phi into the original frame.
        let p = |c: f32, s: f32| (cx + cos_p * (rx * c) - sin_p * (ry * s),
                                  cy + sin_p * (rx * c) + cos_p * (ry * s));
        let (e1x, e1y) = p(cos1, sin1);
        // Control points use the arc tangents.
        let c1lx = cos0 - t * sin0;
        let c1ly = sin0 + t * cos0;
        let c2lx = cos1 + t * sin1;
        let c2ly = sin1 - t * cos1;
        let (c1x, c1y) = p(c1lx, c1ly);
        let (c2x, c2y) = p(c2lx, c2ly);
        out.push(PathSegment::CurveTo {
            c1: (c1x, c1y),
            c2: (c2x, c2y),
            end: (e1x, e1y),
        });
        a0 = a1;
    }
}

fn rect_path(x: f32, y: f32, w: f32, h: f32, rx: f32, ry: f32) -> Vec<PathSegment> {
    if rx <= 0.0 && ry <= 0.0 {
        return vec![
            PathSegment::MoveTo(x, y),
            PathSegment::LineTo(x + w, y),
            PathSegment::LineTo(x + w, y + h),
            PathSegment::LineTo(x, y + h),
            PathSegment::Close,
        ];
    }
    let rx = rx.min(w * 0.5);
    let ry = ry.min(h * 0.5);
    let k = 0.552_284_8; // cubic-bezier circle approximation constant
    let kx = rx * k;
    let ky = ry * k;
    let mut p = Vec::new();
    p.push(PathSegment::MoveTo(x + rx, y));
    p.push(PathSegment::LineTo(x + w - rx, y));
    p.push(PathSegment::CurveTo {
        c1: (x + w - rx + kx, y),
        c2: (x + w, y + ry - ky),
        end: (x + w, y + ry),
    });
    p.push(PathSegment::LineTo(x + w, y + h - ry));
    p.push(PathSegment::CurveTo {
        c1: (x + w, y + h - ry + ky),
        c2: (x + w - rx + kx, y + h),
        end: (x + w - rx, y + h),
    });
    p.push(PathSegment::LineTo(x + rx, y + h));
    p.push(PathSegment::CurveTo {
        c1: (x + rx - kx, y + h),
        c2: (x, y + h - ry + ky),
        end: (x, y + h - ry),
    });
    p.push(PathSegment::LineTo(x, y + ry));
    p.push(PathSegment::CurveTo {
        c1: (x, y + ry - ky),
        c2: (x + rx - kx, y),
        end: (x + rx, y),
    });
    p.push(PathSegment::Close);
    p
}

fn ellipse_path(cx: f32, cy: f32, rx: f32, ry: f32) -> Vec<PathSegment> {
    let k = 0.552_284_8;
    let kx = rx * k;
    let ky = ry * k;
    vec![
        PathSegment::MoveTo(cx + rx, cy),
        PathSegment::CurveTo {
            c1: (cx + rx, cy + ky),
            c2: (cx + kx, cy + ry),
            end: (cx, cy + ry),
        },
        PathSegment::CurveTo {
            c1: (cx - kx, cy + ry),
            c2: (cx - rx, cy + ky),
            end: (cx - rx, cy),
        },
        PathSegment::CurveTo {
            c1: (cx - rx, cy - ky),
            c2: (cx - kx, cy - ry),
            end: (cx, cy - ry),
        },
        PathSegment::CurveTo {
            c1: (cx + kx, cy - ry),
            c2: (cx + rx, cy - ky),
            end: (cx + rx, cy),
        },
        PathSegment::Close,
    ]
}

fn points_path(s: &str, close: bool) -> Vec<PathSegment> {
    let nums: Vec<f32> = s
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse().ok())
        .collect();
    let mut out = Vec::with_capacity(nums.len() / 2 + 1);
    let mut first = true;
    for p in nums.chunks(2) {
        if p.len() != 2 {
            break;
        }
        if first {
            out.push(PathSegment::MoveTo(p[0], p[1]));
            first = false;
        } else {
            out.push(PathSegment::LineTo(p[0], p[1]));
        }
    }
    if close && !first {
        out.push(PathSegment::Close);
    }
    out
}

/// Suppress unused warnings for items only needed by certain features.
#[allow(dead_code)]
fn _unused(_: RgbaColor) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_path() {
        let segs = parse_path_d("M 10 10 L 20 20 Z");
        assert_eq!(segs.len(), 3);
        assert!(matches!(segs[0], PathSegment::MoveTo(10.0, 10.0)));
        assert!(matches!(segs[1], PathSegment::LineTo(20.0, 20.0)));
        assert!(matches!(segs[2], PathSegment::Close));
    }

    #[test]
    fn debug_wikipedia_wordmark_full_svg() {
        // Real wordmark from disk — checks the actual ~10 KB SVG that
        // ships with Wikipedia, including its <g clip-path> wrapper
        // and <defs><clipPath>.
        let svg_text = match std::fs::read_to_string("/tmp/wiki_wordmark.svg") {
            Ok(s) => s,
            Err(_) => return, // file only present after manual fetch; skip
        };
        let doc = bui_html::parse(&svg_text);
        let svg_node = doc.descendants(doc.root).find(|n| {
            doc.element(*n).map(|e| e.name == "svg").unwrap_or(false)
        }).expect("svg root");
        let entry = parse_svg(&doc, svg_node).expect("parse");
        eprintln!(
            "DBG full wordmark: vbox={:?} dims={}x{} shapes={}",
            entry.view_box, entry.width, entry.height, entry.shapes.len()
        );
        for (i, s) in entry.shapes.iter().enumerate() {
            eprintln!(
                "  shape[{}] segs={} fill={:?} first_seg={:?}",
                i,
                s.segments.len(),
                s.fill,
                s.segments.first()
            );
        }
        assert!(entry.shapes.len() >= 2, "expected ≥ 2 shapes");
        let total_segs: usize = entry.shapes.iter().map(|s| s.segments.len()).sum();
        assert!(total_segs > 20, "expected many path segments (got {})", total_segs);
    }

    #[test]
    fn debug_wikipedia_wordmark_shape_count() {
        // The actual Wikipedia wordmark SVG: <g clip-path="url(#a)"> wrapping
        // two <path> elements (blue + black), plus a <defs><clipPath>. The
        // wordmark is the visible logo text in the header. Without this
        // shape-extraction working, the header shows an empty 140×22 box.
        let svg_text = r##"<svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 140 22"><g clip-path="url(#a)"><path fill="#0e65c0" d="M137.8 5.465c-.445 0-.872.18-1.241.523a.47.47 0 0 1-.324.128.5.5 0 0 1-.337-.132.48.48 0 0 1-.148-.35V0h-5.61z"/><path fill="#000" d="M26.822.651a.42.42 0 0 1-.096.266q-.084.12-.192.12-.884.086-1.446.576Z"/></g><defs><clipPath id="a"><path fill="#fff" d="M0 0h140v21.42H0z"/></clipPath></defs></svg>"##;
        let doc = bui_html::parse(svg_text);
        let svg_node = doc.descendants(doc.root).find(|n| {
            doc.element(*n).map(|e| e.name == "svg").unwrap_or(false)
        }).expect("svg root");
        let entry = parse_svg(&doc, svg_node).expect("parse");
        eprintln!(
            "DBG wordmark: vbox={:?} dims={}x{} shapes={}",
            entry.view_box, entry.width, entry.height, entry.shapes.len()
        );
        for (i, s) in entry.shapes.iter().enumerate() {
            eprintln!("  shape[{}] segs={} fill={:?}", i, s.segments.len(), s.fill);
        }
        assert!(entry.shapes.len() >= 2, "expected ≥ 2 shapes from <g> children");
    }

    #[test]
    fn relative_moves_resolve_against_pen() {
        let segs = parse_path_d("M 10 10 l 5 5 z");
        assert!(matches!(segs[0], PathSegment::MoveTo(10.0, 10.0)));
        assert!(matches!(segs[1], PathSegment::LineTo(15.0, 15.0)));
    }

    #[test]
    fn h_v_lines() {
        let segs = parse_path_d("M 0 0 H 10 V 5");
        assert!(matches!(segs[1], PathSegment::LineTo(10.0, 0.0)));
        assert!(matches!(segs[2], PathSegment::LineTo(10.0, 5.0)));
    }

    #[test]
    fn cubic_bezier_absolute() {
        let segs = parse_path_d("M 0 0 C 10 0 10 10 0 10");
        assert_eq!(segs.len(), 2);
        match segs[1] {
            PathSegment::CurveTo { c1, c2, end } => {
                assert_eq!(c1, (10.0, 0.0));
                assert_eq!(c2, (10.0, 10.0));
                assert_eq!(end, (0.0, 10.0));
            }
            _ => panic!("expected CurveTo"),
        }
    }

    #[test]
    fn arc_converts_to_beziers_spanning_endpoints() {
        // A half-circle arc from (0,0) to (20,0), r=10, should produce
        // CurveTo segments (not be skipped) and reach the endpoint.
        let segs = parse_path_d("M 0 0 A 10 10 0 0 1 20 0");
        assert!(segs.len() >= 2, "arc should emit at least one curve, got {segs:?}");
        assert!(
            segs.iter().any(|s| matches!(s, PathSegment::CurveTo { .. })),
            "arc must become CurveTo segments"
        );
        // Last segment ends at (20, 0).
        match segs.last().unwrap() {
            PathSegment::CurveTo { end, .. } => {
                assert!((end.0 - 20.0).abs() < 0.5 && end.1.abs() < 0.5, "ends at {end:?}");
            }
            other => panic!("expected CurveTo at end, got {other:?}"),
        }
    }

    #[test]
    fn arc_flags_may_be_unseparated() {
        // The large-arc / sweep flags can run together with the next
        // number: `...0 11,0...`. Must parse as flags 1,1 then x=1,y=0
        // — NOT swallow `11` as one number.
        let segs = parse_path_d("M 5 5 a 5 5 0 11 1 0");
        assert!(
            segs.iter().any(|s| matches!(s, PathSegment::CurveTo { .. })),
            "unseparated arc flags should still parse to curves, got {segs:?}"
        );
    }

    #[test]
    fn implicit_repeated_lineto_after_moveto() {
        // After M 10 10, the second pair is a LineTo, per SVG spec.
        let segs = parse_path_d("M 10 10 20 20");
        assert!(matches!(segs[0], PathSegment::MoveTo(10.0, 10.0)));
        assert!(matches!(segs[1], PathSegment::LineTo(20.0, 20.0)));
    }

    #[test]
    fn rect_with_no_radius_is_polygon() {
        let p = rect_path(0.0, 0.0, 10.0, 10.0, 0.0, 0.0);
        assert_eq!(p.len(), 5);
        assert!(matches!(p[4], PathSegment::Close));
    }

    #[test]
    fn parses_view_box_and_attrs() {
        use bui_dom::Document;
        let mut doc = Document::new();
        let svg = doc.create_element("svg");
        doc.element_mut(svg)
            .unwrap()
            .set_attr("viewBox", "0 0 24 24");
        doc.element_mut(svg).unwrap().set_attr("width", "48");
        doc.append_child(doc.root, svg);
        let path = doc.create_element("path");
        doc.element_mut(path).unwrap().set_attr("d", "M 0 0 L 24 24");
        doc.element_mut(path).unwrap().set_attr("fill", "#ff0000");
        doc.append_child(svg, path);
        let entry = parse_svg(&doc, svg).unwrap();
        assert_eq!(entry.view_box, (0.0, 0.0, 24.0, 24.0));
        assert_eq!(entry.width, 48.0);
        assert_eq!(entry.height, 24.0); // from view_box height (no explicit height attr)
        assert_eq!(entry.shapes.len(), 1);
        assert_eq!(entry.shapes[0].fill, Some(Color::rgb(0xff, 0, 0)));
    }
}
