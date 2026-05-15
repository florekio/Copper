//! bui-paint — renderer-agnostic display list.
//!
//! Phase 4 added `Text` commands. Real glyph rasterization lands in Phase 6;
//! until then `bui-gpu` renders text as solid-coloured rectangles.

pub mod raster;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const TRANSPARENT: Self = Self::rgba(0, 0, 0, 0);
    pub const WHITE: Self = Self::rgb(255, 255, 255);
    pub const BLACK: Self = Self::rgb(0, 0, 0);
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
}

/// A single segment of an arbitrary 2D path. Path commands are placed
/// in user space and transformed (translate + scale) to fit a target
/// rect at render time. Used for inline SVG today; longer term any
/// renderer that wants curve fills can target this command.
#[derive(Debug, Clone, Copy)]
pub enum PathSegment {
    MoveTo(f32, f32),
    LineTo(f32, f32),
    CurveTo {
        c1: (f32, f32),
        c2: (f32, f32),
        end: (f32, f32),
    },
    QuadTo {
        c: (f32, f32),
        end: (f32, f32),
    },
    Close,
}

#[derive(Debug, Clone)]
pub enum PaintCommand {
    FillRect {
        rect: Rect,
        color: Color,
    },
    /// Per-corner radii: `[top_left, top_right, bottom_right, bottom_left]`.
    /// Used for tabs (rounded top, square bottom), the address bar pill,
    /// hovered chrome buttons, etc.
    FillRoundedRect {
        rect: Rect,
        color: Color,
        radii: [f32; 4],
    },
    /// Closed polygon with per-vertex screen coordinates. Used for chrome
    /// icons (back/forward triangles, the reload arc), favicons, and
    /// anything else that needs an arbitrary outline.
    FillPath {
        points: Vec<(f32, f32)>,
        color: Color,
    },
    /// Reference an uploaded image by stable key (typically its source URL)
    /// and stretch it to fit `rect`. The renderer's image cache resolves
    /// the key to an actual texture.
    Image {
        rect: Rect,
        key: String,
    },
    /// Phase 4 placeholder: a run of text laid out at `(x, baseline_y)` with
    /// known `font_size` and total `advance` width. Renderers may use the
    /// `content` for real shaping later, or fall back to rectangles.
    Text {
        x: f32,
        baseline: f32,
        advance: f32,
        font_size: f32,
        color: Color,
        content: String,
    },
    /// SVG-style 2D path with optional fill and stroke. The `segments`
    /// list lives in the user-space coordinate system described by
    /// `view_box` (`x, y, w, h`); the renderer scales it into `rect`.
    /// Used by inline SVG.
    Svg {
        rect: Rect,
        view_box: (f32, f32, f32, f32),
        segments: Vec<PathSegment>,
        fill: Option<Color>,
        stroke: Option<Color>,
        stroke_width: f32,
    },
    /// Drop shadow under a box. The renderer convolves a rounded rect
    /// of size `rect` with a Gaussian filter of standard deviation
    /// `blur / 2.0` (matching CSS box-shadow's blur-radius convention).
    BoxShadow {
        rect: Rect,
        color: Color,
        radius: f32,
        blur: f32,
    },
    /// Push a rounded-rect clip onto the renderer's clip stack. All
    /// subsequent draws apply only inside `rect`; pop with
    /// `PopClip`. Used by `overflow: hidden | auto | scroll | clip`.
    PushClip {
        rect: Rect,
        radii: [f32; 4],
    },
    PopClip,
    /// Mark the start of a `position: sticky` group. Commands between
    /// this and the matching `PopStickyGroup` should be shifted by an
    /// "effective scroll" that pins the box at `natural_y - top_edge`
    /// from the viewport top once the user scrolls past it. The actual
    /// scroll resolution happens in the post-paint shift pass that
    /// owns the current scroll offset — bui-layout emits the static
    /// parameters and stays renderer-agnostic.
    ///
    /// `natural_y` is the y where the box would paint at scroll=0.
    /// `top_edge` is the CSS `top:` resolved to px (default 0).
    /// `range_bottom` is the y past which the box stops being sticky
    /// (typically its containing block's bottom).
    PushStickyGroup {
        natural_y: f32,
        top_edge: f32,
        range_bottom: f32,
    },
    PopStickyGroup,
}

#[derive(Debug, Default, Clone)]
pub struct DisplayList {
    pub commands: Vec<PaintCommand>,
}

impl DisplayList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn fill_rect(&mut self, rect: Rect, color: Color) {
        self.commands.push(PaintCommand::FillRect { rect, color });
    }

    pub fn fill_rounded_rect(&mut self, rect: Rect, color: Color, radii: [f32; 4]) {
        self.commands
            .push(PaintCommand::FillRoundedRect { rect, color, radii });
    }

    pub fn fill_path(&mut self, points: Vec<(f32, f32)>, color: Color) {
        self.commands.push(PaintCommand::FillPath { points, color });
    }

    pub fn image(&mut self, rect: Rect, key: impl Into<String>) {
        self.commands.push(PaintCommand::Image {
            rect,
            key: key.into(),
        });
    }

    pub fn text(
        &mut self,
        x: f32,
        baseline: f32,
        advance: f32,
        font_size: f32,
        color: Color,
        content: impl Into<String>,
    ) {
        self.commands.push(PaintCommand::Text {
            x,
            baseline,
            advance,
            font_size,
            color,
            content: content.into(),
        });
    }

    pub fn extend(&mut self, other: DisplayList) {
        self.commands.extend(other.commands);
    }

    pub fn box_shadow(&mut self, rect: Rect, color: Color, radius: f32, blur: f32) {
        self.commands
            .push(PaintCommand::BoxShadow { rect, color, radius, blur });
    }

    pub fn push_clip(&mut self, rect: Rect, radii: [f32; 4]) {
        self.commands.push(PaintCommand::PushClip { rect, radii });
    }

    pub fn pop_clip(&mut self) {
        self.commands.push(PaintCommand::PopClip);
    }

    pub fn svg(
        &mut self,
        rect: Rect,
        view_box: (f32, f32, f32, f32),
        segments: Vec<PathSegment>,
        fill: Option<Color>,
        stroke: Option<Color>,
        stroke_width: f32,
    ) {
        self.commands.push(PaintCommand::Svg {
            rect,
            view_box,
            segments,
            fill,
            stroke,
            stroke_width,
        });
    }
}
