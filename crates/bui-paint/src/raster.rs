//! Software rasteriser for filled vector paths.
//!
//! Used to materialise CSS `background-image: url(.svg)` into a raster
//! the GPU compositor can upload like any PNG. Inline `<svg>` elements
//! still go through the GPU's vello-backed path renderer; this module
//! exists for the cases where the consumer needs RGBA8 bytes (cache
//! upload, image decode pipeline) instead of a display-list emit.
//!
//! The pipeline is intentionally cheap:
//!
//!   1. Flatten each `PathSegment` (cubic / quadratic bezier) into a
//!      sequence of straight-line edges using recursive midpoint
//!      subdivision.
//!   2. For each scanline `y` in the target raster, find every edge
//!      that straddles `y + 0.5` and compute its `x` intersection.
//!   3. Sort the intersections and fill spans pairwise (even-odd
//!      winding rule). Pixels under each span receive the shape's
//!      fill colour, premultiplied and source-over-blended onto
//!      whatever was already there.
//!
//! No anti-aliasing, no stroke (yet), no gradients — but enough to
//! turn typical icon SVGs (single-colour fills, cubic curves) into a
//! recognisable raster at button sizes.

use crate::{Color, PathSegment};

/// Maximum recursion depth for bezier flattening. 10 doublings keeps
/// even high-curvature paths from growing the edge list past a few
/// thousand segments per shape — plenty for icon-sized output.
const MAX_FLATTEN_DEPTH: u32 = 10;

/// Flatness threshold in target pixels. A bezier is "flat enough" once
/// its control points sit within this distance of the chord.
const FLATTEN_TOLERANCE: f32 = 0.5;

/// One filled vector path ready for rasterisation. The caller has
/// already chosen which shapes to fill (and skipped `fill: none`).
pub struct FilledShape<'a> {
    pub segments: &'a [PathSegment],
    pub fill: Color,
}

/// Rasterise a list of filled paths positioned in `view_box` user
/// space onto a `width × height` RGBA8 buffer. The buffer is
/// row-major, tightly packed (4 bytes per pixel, no padding).
///
/// `view_box` is `(min_x, min_y, vbw, vbh)`. The function maps that
/// rectangle onto the full output raster — no aspect-ratio
/// preservation; the caller is expected to have picked compatible
/// dimensions if it cares.
pub fn rasterize(
    shapes: &[FilledShape<'_>],
    view_box: (f32, f32, f32, f32),
    width: u32,
    height: u32,
) -> Vec<u8> {
    let mut out = vec![0u8; (width as usize) * (height as usize) * 4];
    if width == 0 || height == 0 {
        return out;
    }
    let (vbx, vby, vbw, vbh) = view_box;
    if vbw <= 0.0 || vbh <= 0.0 {
        return out;
    }
    let sx = width as f32 / vbw;
    let sy = height as f32 / vbh;
    let to_px = |p: (f32, f32)| ((p.0 - vbx) * sx, (p.1 - vby) * sy);

    for shape in shapes {
        let edges = build_edges(shape.segments, to_px);
        if edges.is_empty() {
            continue;
        }
        fill_edges(&edges, shape.fill, &mut out, width, height);
    }
    out
}

/// One straight-line segment of the polygon, with `y0 <= y1`. Stored
/// in target-pixel space.
#[derive(Clone, Copy)]
struct Edge {
    y0: f32,
    y1: f32,
    x0: f32,
    inv_slope: f32, // dx/dy; horizontal edges (y0 == y1) are skipped before storing.
}

fn build_edges(segments: &[PathSegment], to_px: impl Fn((f32, f32)) -> (f32, f32)) -> Vec<Edge> {
    let mut edges: Vec<Edge> = Vec::new();
    let mut cur = (0.0_f32, 0.0_f32);
    let mut subpath_start = (0.0_f32, 0.0_f32);

    let mut emit_line = |from: (f32, f32), to: (f32, f32), edges: &mut Vec<Edge>| {
        let a = to_px(from);
        let b = to_px(to);
        if (a.1 - b.1).abs() < f32::EPSILON {
            return;
        }
        let (y0, y1, x0, x1) = if a.1 < b.1 {
            (a.1, b.1, a.0, b.0)
        } else {
            (b.1, a.1, b.0, a.0)
        };
        let inv_slope = (x1 - x0) / (y1 - y0);
        edges.push(Edge { y0, y1, x0, inv_slope });
    };

    for seg in segments {
        match *seg {
            PathSegment::MoveTo(x, y) => {
                cur = (x, y);
                subpath_start = (x, y);
            }
            PathSegment::LineTo(x, y) => {
                emit_line(cur, (x, y), &mut edges);
                cur = (x, y);
            }
            PathSegment::QuadTo { c, end } => {
                flatten_quad(cur, c, end, 0, &mut edges, &|a, b, e| {
                    emit_line_helper(a, b, e, &to_px)
                });
                cur = end;
            }
            PathSegment::CurveTo { c1, c2, end } => {
                flatten_cubic(cur, c1, c2, end, 0, &mut edges, &|a, b, e| {
                    emit_line_helper(a, b, e, &to_px)
                });
                cur = end;
            }
            PathSegment::Close => {
                emit_line(cur, subpath_start, &mut edges);
                cur = subpath_start;
            }
        }
    }
    edges
}

fn emit_line_helper(
    from: (f32, f32),
    to: (f32, f32),
    edges: &mut Vec<Edge>,
    to_px: &impl Fn((f32, f32)) -> (f32, f32),
) {
    let a = to_px(from);
    let b = to_px(to);
    if (a.1 - b.1).abs() < f32::EPSILON {
        return;
    }
    let (y0, y1, x0, x1) = if a.1 < b.1 {
        (a.1, b.1, a.0, b.0)
    } else {
        (b.1, a.1, b.0, a.0)
    };
    let inv_slope = (x1 - x0) / (y1 - y0);
    edges.push(Edge { y0, y1, x0, inv_slope });
}

/// Recursive midpoint subdivision for a quadratic bezier. We bottom
/// out when the control point is close enough to the chord midpoint.
fn flatten_quad(
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    depth: u32,
    edges: &mut Vec<Edge>,
    emit: &impl Fn((f32, f32), (f32, f32), &mut Vec<Edge>),
) {
    if depth >= MAX_FLATTEN_DEPTH || quad_is_flat(p0, p1, p2) {
        emit(p0, p2, edges);
        return;
    }
    // De Casteljau split at t = 0.5.
    let q0 = mid(p0, p1);
    let q1 = mid(p1, p2);
    let r = mid(q0, q1);
    flatten_quad(p0, q0, r, depth + 1, edges, emit);
    flatten_quad(r, q1, p2, depth + 1, edges, emit);
}

fn flatten_cubic(
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    p3: (f32, f32),
    depth: u32,
    edges: &mut Vec<Edge>,
    emit: &impl Fn((f32, f32), (f32, f32), &mut Vec<Edge>),
) {
    if depth >= MAX_FLATTEN_DEPTH || cubic_is_flat(p0, p1, p2, p3) {
        emit(p0, p3, edges);
        return;
    }
    let q0 = mid(p0, p1);
    let q1 = mid(p1, p2);
    let q2 = mid(p2, p3);
    let r0 = mid(q0, q1);
    let r1 = mid(q1, q2);
    let s = mid(r0, r1);
    flatten_cubic(p0, q0, r0, s, depth + 1, edges, emit);
    flatten_cubic(s, r1, q2, p3, depth + 1, edges, emit);
}

fn mid(a: (f32, f32), b: (f32, f32)) -> (f32, f32) {
    ((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5)
}

fn quad_is_flat(p0: (f32, f32), p1: (f32, f32), p2: (f32, f32)) -> bool {
    // Distance from p1 to the chord p0-p2 — if both X and Y deltas
    // are within tolerance the curve is straight enough.
    let dx = (p1.0 - 0.5 * (p0.0 + p2.0)).abs();
    let dy = (p1.1 - 0.5 * (p0.1 + p2.1)).abs();
    dx <= FLATTEN_TOLERANCE && dy <= FLATTEN_TOLERANCE
}

fn cubic_is_flat(p0: (f32, f32), p1: (f32, f32), p2: (f32, f32), p3: (f32, f32)) -> bool {
    // Approximate flatness: each control point should lie close to
    // the chord. A real implementation would use perpendicular
    // distance; the bounding-box test above is good enough at icon
    // sizes and avoids a sqrt.
    let dx1 = (p1.0 - (p0.0 + (p3.0 - p0.0) / 3.0)).abs();
    let dy1 = (p1.1 - (p0.1 + (p3.1 - p0.1) / 3.0)).abs();
    let dx2 = (p2.0 - (p0.0 + 2.0 * (p3.0 - p0.0) / 3.0)).abs();
    let dy2 = (p2.1 - (p0.1 + 2.0 * (p3.1 - p0.1) / 3.0)).abs();
    dx1 <= FLATTEN_TOLERANCE
        && dy1 <= FLATTEN_TOLERANCE
        && dx2 <= FLATTEN_TOLERANCE
        && dy2 <= FLATTEN_TOLERANCE
}

/// Even-odd scanline fill. For each output row, walk the edges that
/// cross `y + 0.5`, gather x-intersections, sort them, and fill
/// alternating spans.
fn fill_edges(edges: &[Edge], color: Color, target: &mut [u8], w: u32, h: u32) {
    if edges.is_empty() || color.a == 0 {
        return;
    }
    let mut xs: Vec<f32> = Vec::with_capacity(8);
    for y in 0..h {
        let yc = y as f32 + 0.5;
        xs.clear();
        for e in edges {
            if yc >= e.y0 && yc < e.y1 {
                xs.push(e.x0 + (yc - e.y0) * e.inv_slope);
            }
        }
        if xs.is_empty() {
            continue;
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut i = 0;
        while i + 1 < xs.len() {
            let lx = xs[i].max(0.0).round() as i32;
            let rx = xs[i + 1].min(w as f32).round() as i32;
            if rx > lx {
                let row_start = (y as usize) * (w as usize) * 4;
                for x in lx..rx {
                    let idx = row_start + (x as usize) * 4;
                    blend(&mut target[idx..idx + 4], color);
                }
            }
            i += 2;
        }
    }
}

fn blend(dst: &mut [u8], src: Color) {
    let sa = src.a as u32;
    if sa == 255 {
        dst[0] = src.r;
        dst[1] = src.g;
        dst[2] = src.b;
        dst[3] = 255;
        return;
    }
    let inv = 255 - sa;
    let dr = dst[0] as u32;
    let dg = dst[1] as u32;
    let db = dst[2] as u32;
    let da = dst[3] as u32;
    dst[0] = ((src.r as u32 * sa + dr * inv) / 255) as u8;
    dst[1] = ((src.g as u32 * sa + dg * inv) / 255) as u8;
    dst[2] = ((src.b as u32 * sa + db * inv) / 255) as u8;
    dst[3] = (sa + da * inv / 255).min(255) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rasterizes_a_solid_square() {
        // A 10x10 viewBox with a single filled rectangle path covering
        // the whole canvas should produce an all-red output.
        let segs = vec![
            PathSegment::MoveTo(0.0, 0.0),
            PathSegment::LineTo(10.0, 0.0),
            PathSegment::LineTo(10.0, 10.0),
            PathSegment::LineTo(0.0, 10.0),
            PathSegment::Close,
        ];
        let shape = FilledShape {
            segments: &segs,
            fill: Color::rgb(200, 0, 0),
        };
        let bytes = rasterize(&[shape], (0.0, 0.0, 10.0, 10.0), 8, 8);
        // Centre pixel should be fully red.
        let mid = (4 * 8 + 4) * 4;
        assert_eq!(&bytes[mid..mid + 4], &[200, 0, 0, 255]);
    }

    #[test]
    fn empty_shapes_yields_transparent_buffer() {
        let bytes = rasterize(&[], (0.0, 0.0, 4.0, 4.0), 4, 4);
        for chunk in bytes.chunks(4) {
            assert_eq!(chunk, &[0, 0, 0, 0]);
        }
    }
}
