use bui_css::Declaration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    Block,
    Inline,
    InlineBlock,
    Flex,
    Grid,
    None,
    ListItem,
    TableRow,
    TableCell,
    Table,
    /// `display: contents` — the wrapper element doesn't produce a
    /// box. Its DOM children promote into the parent's flow, so a
    /// `<div display:contents>` between a flex / grid container and
    /// the real items doesn't break flex / grid item-hood. Cascade
    /// still applies to descendants normally.
    Contents,
}

/// CSS Grid track size. We resolve `min-content` / `max-content` /
/// `fit-content` to `Auto` for now — the simplified track-sizing
/// algorithm in `layout_grid` already handles the auto case from
/// child intrinsic widths.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrackSize {
    Auto,
    Length(Length),
    Fr(f32),
    /// `minmax(min, max)` — both sides are themselves track sizes; we
    /// resolve them as a clamp during grid sizing.
    MinMax(MinMaxSide, MinMaxSide),
}

/// Subset of TrackSize allowed inside `minmax()`. CSS forbids `fr` on
/// the min side and forbids nested `minmax()`; this type keeps both
/// rules at the type level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MinMaxSide {
    Auto,
    Length(Length),
    Fr(f32),
}

impl MinMaxSide {
    pub fn as_track(self) -> TrackSize {
        match self {
            MinMaxSide::Auto => TrackSize::Auto,
            MinMaxSide::Length(l) => TrackSize::Length(l),
            MinMaxSide::Fr(f) => TrackSize::Fr(f),
        }
    }
}

/// CSS Grid line reference for `grid-column` / `grid-row`. Named
/// lines are resolved against the parent grid's line-name table at
/// placement time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridLine {
    Auto,
    /// Explicit 1-based line number. Negative numbers (counting from
    /// the end) parse to `Auto` for now — the grid usually has too
    /// few rows for negative-line resolution to be meaningful before
    /// auto-placement runs.
    Line(i32),
    /// `span <n>` — n is always >= 1.
    Span(u32),
    /// `<name>` token — looked up against the container's
    /// grid-template-{columns,rows} line-name table.
    Named(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridAutoFlow {
    Row,
    Column,
}

/// Set the viewport dimensions used by `vw` / `vh` / `vmin` / `vmax`
/// length resolution. The bui binary calls this before each layout
/// pass; a thread-local keeps the resolve() signature unchanged so
/// the dozens of existing call sites don't all need updating.
pub fn set_viewport(width: f32, height: f32) {
    VIEWPORT.with(|v| v.set((width.max(0.0), height.max(0.0))));
}

pub fn viewport() -> (f32, f32) {
    VIEWPORT.with(|v| v.get())
}

std::thread_local! {
    static VIEWPORT: std::cell::Cell<(f32, f32)> = const { std::cell::Cell::new((0.0, 0.0)) };
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Length {
    Px(f32),
    Em(f32),
    Rem(f32),
    Percent(f32),
    /// `<n>vw` — n percent of the viewport width.
    Vw(f32),
    /// `<n>vh` — n percent of the viewport height.
    Vh(f32),
    /// `<n>vmin` — n percent of `min(viewport_w, viewport_h)`.
    Vmin(f32),
    /// `<n>vmax` — n percent of `max(viewport_w, viewport_h)`.
    Vmax(f32),
    /// `calc(...)` flattened into a unit-keyed sum so each call site
    /// can resolve at the same call-shape as a regular Length leaf.
    /// `calc(100% - 30px + 0.5em)` reduces to `CalcSum { px: -30,
    /// em: 0.5, rem: 0, percent: 100 }` at parse time. Anything that
    /// can't fold (mul/div by a unitful operand) falls out to
    /// `Length::Px(0)`.
    Calc(CalcSum),
    /// `min(...)` / `max(...)` over up to 4 operands. Anything wider
    /// folds at parse time via the associative property (real CSS
    /// stylesheets rarely go past 3-4 operands anyway).
    Min(BoundedList),
    Max(BoundedList),
    /// `clamp(min, val, max)` — three operands resolved separately
    /// then combined as `max(min, min(val, max))`.
    Clamp(CalcSum, CalcSum, CalcSum),
}

/// Up to 4 operand `CalcSum` values for the `min()` / `max()`
/// length-functions. `clamp(a, b, c)` is rewritten at parse time to
/// `max(a, min(b, c))` so it doesn't need its own variant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoundedList {
    pub items: [CalcSum; 4],
    pub n: u8,
}

impl BoundedList {
    fn from_iter(it: impl IntoIterator<Item = CalcSum>) -> Self {
        let mut items = [CalcSum::default(); 4];
        let mut n = 0u8;
        for v in it {
            if (n as usize) < items.len() {
                items[n as usize] = v;
                n += 1;
            }
        }
        Self { items, n }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CalcSum {
    pub px: f32,
    pub em: f32,
    pub rem: f32,
    pub percent: f32,
    pub vw: f32,
    pub vh: f32,
}

impl CalcSum {
    pub fn resolve(self, font_size: f32, root_font_size: f32, percent_basis: f32) -> f32 {
        let (vw, vh) = viewport();
        self.px
            + self.em * font_size
            + self.rem * root_font_size
            + self.percent / 100.0 * percent_basis
            + self.vw / 100.0 * vw
            + self.vh / 100.0 * vh
    }

    fn add(mut self, other: Self, sign: f32) -> Self {
        self.px += sign * other.px;
        self.em += sign * other.em;
        self.rem += sign * other.rem;
        self.percent += sign * other.percent;
        self.vw += sign * other.vw;
        self.vh += sign * other.vh;
        self
    }

    fn scale(mut self, factor: f32) -> Self {
        self.px *= factor;
        self.em *= factor;
        self.rem *= factor;
        self.percent *= factor;
        self.vw *= factor;
        self.vh *= factor;
        self
    }
}

impl Length {
    pub fn resolve(self, font_size: f32, root_font_size: f32, percent_basis: f32) -> f32 {
        match self {
            Length::Px(v) => v,
            Length::Em(v) => v * font_size,
            Length::Rem(v) => v * root_font_size,
            Length::Percent(v) => v / 100.0 * percent_basis,
            Length::Vw(v) => {
                let (vw, _) = viewport();
                v / 100.0 * vw
            }
            Length::Vh(v) => {
                let (_, vh) = viewport();
                v / 100.0 * vh
            }
            Length::Vmin(v) => {
                let (vw, vh) = viewport();
                v / 100.0 * vw.min(vh)
            }
            Length::Vmax(v) => {
                let (vw, vh) = viewport();
                v / 100.0 * vw.max(vh)
            }
            Length::Calc(s) => s.resolve(font_size, root_font_size, percent_basis),
            Length::Min(b) => {
                let mut best = f32::INFINITY;
                for i in 0..b.n as usize {
                    let v = b.items[i].resolve(font_size, root_font_size, percent_basis);
                    if v < best {
                        best = v;
                    }
                }
                if best.is_finite() { best } else { 0.0 }
            }
            Length::Max(b) => {
                let mut best = f32::NEG_INFINITY;
                for i in 0..b.n as usize {
                    let v = b.items[i].resolve(font_size, root_font_size, percent_basis);
                    if v > best {
                        best = v;
                    }
                }
                if best.is_finite() { best } else { 0.0 }
            }
            Length::Clamp(lo, val, hi) => {
                let lo = lo.resolve(font_size, root_font_size, percent_basis);
                let val = val.resolve(font_size, root_font_size, percent_basis);
                let hi = hi.resolve(font_size, root_font_size, percent_basis);
                lo.max(val.min(hi))
            }
        }
    }

    /// Convert any leaf length to its CalcSum form. Used by the calc()
    /// parser when folding terms. Returns `None` for the bounded
    /// (min/max) variants which can't be flattened to a linear sum.
    fn to_calc_sum(self) -> Option<CalcSum> {
        Some(match self {
            Length::Px(v) => CalcSum { px: v, ..Default::default() },
            Length::Em(v) => CalcSum { em: v, ..Default::default() },
            Length::Rem(v) => CalcSum { rem: v, ..Default::default() },
            Length::Percent(v) => CalcSum { percent: v, ..Default::default() },
            Length::Vw(v) => CalcSum { vw: v, ..Default::default() },
            Length::Vh(v) => CalcSum { vh: v, ..Default::default() },
            Length::Vmin(_) | Length::Vmax(_) => return None,
            Length::Calc(s) => s,
            Length::Min(_) | Length::Max(_) | Length::Clamp(..) => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Dimension {
    Auto,
    Length(Length),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontWeight {
    Normal,
    Bold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontStyle {
    Normal,
    Italic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Right,
    Center,
    Justify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextTransform {
    None,
    Uppercase,
    Lowercase,
    Capitalize,
}

/// CSS `overflow` keywords. `Visible` (the default) means children
/// can render outside the box; everything else makes the renderer
/// clip the box's subtree to its border-box rectangle. We don't yet
/// distinguish hidden/auto/scroll visually — there's no scrollbar
/// or interaction support, only the clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
    Visible,
    Hidden,
    Scroll,
    Auto,
    Clip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerticalAlign {
    Baseline,
    Top,
    Middle,
    Bottom,
    /// `text-top` / `text-bottom` collapse to the line's font-size
    /// extents — close enough to top/bottom for most uses.
    TextTop,
    TextBottom,
    Sub,
    Super,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhiteSpace {
    Normal,
    Pre,
    Nowrap,
    PreWrap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexDirection {
    Row,
    Column,
    RowReverse,
    ColumnReverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JustifyContent {
    FlexStart,
    FlexEnd,
    Center,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignItems {
    Stretch,
    FlexStart,
    FlexEnd,
    Center,
    Baseline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexWrap {
    Nowrap,
    Wrap,
    WrapReverse,
}

/// CSS `word-break`. `BreakAll` allows line breaks at any character
/// boundary inside otherwise unbreakable words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordBreak {
    Normal,
    BreakAll,
    KeepAll,
}

/// CSS `overflow-wrap` (alias `word-wrap`). `BreakWord` allows
/// inter-char breaks ONLY when an unbreakable word would otherwise
/// overflow its line. `Anywhere` is more aggressive and contributes
/// to min-content sizing too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowWrap {
    Normal,
    BreakWord,
    Anywhere,
}

/// CSS `pointer-events` keyword. `None` makes the box transparent
/// to mouse events — clicks pass through to whatever is underneath
/// (or to a non-`none` ancestor). Used by overlay UI that should
/// not steal pointer events from content beneath.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerEvents {
    Auto,
    None,
}

/// CSS `visibility`. `Hidden` keeps the box in the layout (sibling
/// flow stays unchanged) but the painter skips its content + chrome.
/// `Collapse` is the same as `Hidden` for non-table content; the
/// table-row collapsing case isn't implemented here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Visible,
    Hidden,
    Collapse,
}

/// CSS `text-overflow` keyword. Real ellipsis only kicks in when the
/// box also has `overflow: hidden` and prevents wrapping (`white-space:
/// nowrap` or a similarly clipping mode); otherwise the value is a
/// hint that the layout currently ignores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextOverflow {
    Clip,
    Ellipsis,
}

/// CSS `background-size` keyword. Mirrors `object-fit` behaviour
/// applied to the background-image painter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BackgroundSize {
    /// Default — image renders at its intrinsic dimensions.
    Auto,
    /// `cover` — scales preserving aspect to fill the box, cropping.
    Cover,
    /// `contain` — scales preserving aspect to fit inside the box.
    Contain,
    /// Two-axis explicit dims. `Length::Px(0)` on either axis means
    /// "auto on this axis"; the painter then keeps the image's
    /// intrinsic ratio for the other.
    Length(Length, Length),
}

/// CSS `background-repeat` keyword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundRepeat {
    Repeat,
    NoRepeat,
    RepeatX,
    RepeatY,
}

/// CSS `background-position` — origin within the box. Stored as
/// fractional offsets (0..=1) for x and y. `0.5` = centre.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackgroundPosition {
    pub x: BackgroundAxisPos,
    pub y: BackgroundAxisPos,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BackgroundAxisPos {
    /// Anchor as a fraction (0..=1) of the difference between box
    /// size and image size. `0` = start, `1` = end, `0.5` = centre.
    Anchor(f32),
    /// Explicit length offset from the start edge.
    Length(Length),
}

/// CSS `object-fit` keyword for replaced elements (chiefly `<img>`).
/// Controls how the intrinsic image fits inside the box established
/// by the element's CSS width / height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectFit {
    /// Stretch to fill the box (CSS initial value).
    Fill,
    /// Scale to fit inside the box, preserving aspect ratio.
    Contain,
    /// Scale to cover the box, preserving aspect ratio (crop).
    Cover,
    /// Render at intrinsic size, centred, clipped to the box.
    None,
    /// `min(none-size, contain-size)` — never up-scales.
    ScaleDown,
}

/// CSS `caption-side` keyword. Default `Top` per CSS Tables L3 —
/// HTML's `<caption>` is rendered above its `<table>` regardless of
/// its source position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptionSide {
    Top,
    Bottom,
}

/// CSS `border-collapse` keyword. `Separate` (the initial value)
/// renders each cell's borders independently; `Collapse` merges
/// adjacent cell borders into a single shared edge — what every
/// Wikipedia infobox depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderCollapse {
    Separate,
    Collapse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Static,
    Relative,
    Absolute,
    Fixed,
    /// `position: sticky` — until the page has real scroll-aware
    /// containing blocks, we treat sticky as relative. Author CSS
    /// that uses sticky for nav strips falls back to "stays where
    /// you put it" instead of crashing or looking broken.
    Sticky,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListStyleType {
    None,
    Disc,
    Circle,
    Square,
    Decimal,
    DecimalLeadingZero,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
}

/// CSS `text-shadow`. Single layer (we keep only the first when the
/// declaration has multiple comma-separated layers — matches what
/// our `box-shadow` handling does). `blur` is parsed but the painter
/// currently ignores it (no Gaussian blur in the inline-text path);
/// the offset alone is enough to give the visible "lifted" effect
/// authors are after.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextShadow {
    pub offset_x: Length,
    pub offset_y: Length,
    pub blur: Length,
    pub color: RgbaColor,
}

/// CSS `box-shadow` (drop shadow only — `inset` not implemented).
/// Multi-shadow declarations keep only the first shadow.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoxShadow {
    pub offset_x: Length,
    pub offset_y: Length,
    pub blur: Length,
    pub spread: Length,
    pub color: RgbaColor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Float {
    None,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clear {
    None,
    Left,
    Right,
    Both,
}

/// CSS `cursor` keyword subset. Anything we don't recognise — including
/// `cursor: url(...)` — falls back to `Default`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    Default,
    Pointer,
    Text,
    NotAllowed,
    Wait,
    Crosshair,
    Move,
    Help,
    Progress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxSizing {
    /// Width / height set the *content* box (CSS default).
    ContentBox,
    /// Width / height set the *border* box — padding and border are
    /// subtracted from the declared size to derive content size.
    BorderBox,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FlexBasis {
    Auto,
    Length(Length),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RgbaColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl RgbaColor {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
    pub const TRANSPARENT: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };
    pub const BLACK: Self = Self::rgb(0, 0, 0);
    pub const WHITE: Self = Self::rgb(255, 255, 255);
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeSizes {
    pub top: Length,
    pub right: Length,
    pub bottom: Length,
    pub left: Length,
}

impl EdgeSizes {
    pub const ZERO: Self = Self {
        top: Length::Px(0.0),
        right: Length::Px(0.0),
        bottom: Length::Px(0.0),
        left: Length::Px(0.0),
    };
    pub const fn uniform(l: Length) -> Self {
        Self {
            top: l,
            right: l,
            bottom: l,
            left: l,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ComputedValues {
    pub display: Display,
    pub color: RgbaColor,
    pub background_color: RgbaColor,
    pub font_size: f32, // px
    pub font_family: String,
    pub font_weight: FontWeight,
    pub font_style: FontStyle,
    pub line_height: f32, // multiplier × font_size
    pub text_align: TextAlign,
    pub white_space: WhiteSpace,
    pub margin: EdgeSizes,
    pub padding: EdgeSizes,
    pub border: EdgeSizes,
    pub border_color: RgbaColor,
    /// True when the cascade explicitly set `border-color` (or one
    /// of its long-hands, or the `border` shorthand with a color
    /// token). False means the cv still holds the spec-default
    /// `currentcolor` — `cascade_recursive` finalizes that to
    /// `cv.color` once the element's own color is settled. Without
    /// this, every element inherited the parent's color via
    /// `inherit_into_default()` and an author rule that bumped
    /// only `color` (without re-stating border-color) painted
    /// borders in the OLD parent color rather than the new local
    /// one.
    pub border_color_explicit: bool,
    pub width: Dimension,
    pub height: Dimension,
    // Flexbox container properties (apply to elements with display: flex).
    pub flex_direction: FlexDirection,
    pub justify_content: JustifyContent,
    pub align_items: AlignItems,
    pub flex_wrap: FlexWrap,
    // Flexbox item properties (apply to children of display: flex).
    pub flex_grow: f32,
    pub flex_shrink: f32,
    pub flex_basis: FlexBasis,
    // Positioning. `position: static` is the initial value; the four
    // edge offsets are `None` for `auto` and a `Length` otherwise.
    pub position: Position,
    pub top: Option<Length>,
    pub right: Option<Length>,
    pub bottom: Option<Length>,
    pub left: Option<Length>,
    /// `margin-left: auto` / `margin-right: auto` flags. `margin.left`
    /// / `margin.right` keep their parsed `Length` (or `0`) so most
    /// callers stay length-only; only `layout_block` reads these to
    /// implement horizontal centering of fixed-width blocks.
    pub margin_left_auto: bool,
    pub margin_right_auto: bool,
    /// `margin-top: auto` / `margin-bottom: auto`. CSS Flexbox §8.1:
    /// auto margins on a flex item absorb free space on the relevant
    /// axis (e.g., `margin-top: auto` on a column-flex item pushes
    /// it to the bottom of the flex container). Block layout treats
    /// vertical auto margins as 0.
    pub margin_top_auto: bool,
    pub margin_bottom_auto: bool,
    /// CSS `z-index` for positioned boxes. `None` = `auto` (paint in
    /// source order). Otherwise the integer value sets the stacking
    /// order — higher values paint on top of lower.
    pub z_index: Option<i32>,
    /// Per-corner border radius (top-left, top-right, bottom-right,
    /// bottom-left) — the same order CSS uses for the shorthand. All
    /// zero means a sharp-cornered box.
    pub border_radius: [Length; 4],
    pub box_sizing: BoxSizing,
    /// True when `text-decoration: underline` (or the `underline`
    /// keyword anywhere in a multi-value decl). Inherited like a
    /// typical text property.
    pub text_decoration_underline: bool,
    /// `text-decoration-line: line-through` — drawn through the
    /// middle of each text run, slightly above the baseline. Common
    /// for `<s>` / `<del>` elements and price-strikethrough patterns.
    pub text_decoration_line_through: bool,
    /// `text-decoration-color`. `None` means "use the current text
    /// color" — what CSS calls the `currentColor` default.
    pub text_decoration_color: Option<RgbaColor>,
    /// Flex `row-gap` / `column-gap` in CSS pixels. The flex layout
    /// treats them as straight gaps between successive items, on top
    /// of `justify-content`'s distribution.
    pub row_gap: f32,
    pub column_gap: f32,
    /// One or more box-shadow layers, painted bottom-to-top in
    /// declaration order. Empty Vec = no shadow (the fast path).
    pub box_shadows: Vec<BoxShadow>,
    /// Extra inter-glyph spacing in CSS pixels (resolved at parse time).
    /// Inherited like `font-size` is.
    pub letter_spacing: f32,
    /// CSS `background-image: url(...)`. We currently support only a
    /// single `url()` value (no gradients, no multi-layer). The URL
    /// stored here is whatever the author wrote — the binary resolves
    /// it against the page URL at fetch time.
    pub background_image: Option<String>,
    pub cursor: Cursor,
    /// `min-width` / `max-width` clamp the resolved content width on
    /// blocks. `None` = "no constraint" — `0` for min, ∞ for max.
    pub min_width: Option<Length>,
    pub max_width: Option<Length>,
    pub min_height: Option<Length>,
    pub max_height: Option<Length>,
    /// `opacity` in 0.0..=1.0. 1.0 = fully opaque (the default and
    /// the no-op fast path in the compositor).
    pub opacity: f32,
    /// CSS custom properties — the `--name → value` map. Variables
    /// inherit through the cascade; the cascade also substitutes
    /// `var(--name)` references in non-custom property values before
    /// `apply_declaration` ever sees them.
    pub vars: std::collections::HashMap<String, String>,
    pub text_transform: TextTransform,
    pub vertical_align: VerticalAlign,
    /// Resolved `transform: translate(x, y)` shift in CSS pixels —
    /// `Some` only when the author wrote a translate-style transform.
    /// We don't parse scale/rotate yet (those need a real affine
    /// stack in the compositor); they parse to None silently.
    pub transform_translate: Option<(Length, Length)>,
    /// `outline-width`, `outline-color` (parsed from `outline`
    /// shorthand). `None` width = no outline.
    pub outline_width: Option<Length>,
    pub outline_color: RgbaColor,
    /// CSS `outline-offset` — gap between the border-box and the
    /// outline. Positive shifts the outline outward. Default 0.
    pub outline_offset: Length,
    /// `overflow-x` / `overflow-y` — derived from the `overflow`
    /// shorthand or set independently. Anything other than `Visible`
    /// causes paint to clip the subtree to the border-box.
    pub overflow_x: Overflow,
    pub overflow_y: Overflow,
    /// `text-indent` — first-line indent for the block's inline
    /// content. Negative values pull the first line left.
    pub text_indent: f32,
    /// `list-style-type` — drives the marker label for `<li>`. The
    /// build phase computes the actual marker text (e.g. "1.", "iv.")
    /// using this enum + the item's index in the list.
    pub list_style_type: ListStyleType,
    /// `aspect-ratio: <num> [/ <num>]?` — when set, layout sizes the
    /// box's height as `width / aspect_ratio` if height is auto.
    /// Stored as the numeric ratio (W : H ⇒ W/H).
    pub aspect_ratio: Option<f32>,
    /// Resolved `content: "string"` for pseudo-elements. We don't yet
    /// support `attr()`, `counter()`, or `url()` — anything that
    /// doesn't reduce to a static string at parse time silently
    /// becomes `None`.
    pub content: Option<String>,
    pub float: Float,
    pub clear: Clear,
    /// `object-fit` for replaced elements (chiefly `<img>`).
    pub object_fit: ObjectFit,
    /// CSS `background-size` keyword controlling how
    /// `background-image` fills the box. Default `Auto`.
    pub background_size: BackgroundSize,
    /// CSS `background-position` (default centre).
    pub background_position: BackgroundPosition,
    /// CSS `background-repeat` (default Repeat per spec; we render
    /// stretched-to-box by default since most modern stylesheets
    /// pair background-image with background-size: cover/contain).
    pub background_repeat: BackgroundRepeat,
    /// `line-clamp` / `-webkit-line-clamp`. When set together with
    /// `overflow: hidden`, layout truncates to this many lines and
    /// ellipsises the last surviving line.
    pub line_clamp: Option<u32>,
    /// `word-break` — inherited like other text properties.
    pub word_break: WordBreak,
    /// `overflow-wrap` / `word-wrap`. Inherits.
    pub overflow_wrap: OverflowWrap,
    /// `pointer-events` — inherited; affects hit_test only.
    pub pointer_events: PointerEvents,
    /// `visibility: hidden` — inherited; layout still reserves space.
    pub visibility: Visibility,
    pub text_overflow: TextOverflow,
    /// `text-shadow` first layer. Inherited like a typography property.
    pub text_shadow: Option<TextShadow>,
    /// `border-collapse` on tables. Inherited so cells inside a
    /// `<table>` see their parent's choice without each `<td>` having
    /// to re-declare it.
    pub border_collapse: BorderCollapse,
    /// `border-spacing` for separated tables — gap (x, y) between
    /// adjacent cells. Default 0.
    pub border_spacing_x: Length,
    pub border_spacing_y: Length,
    /// `caption-side` — Top (default) renders the table's `<caption>`
    /// above its rows; Bottom places it below.
    pub caption_side: CaptionSide,
    /// CSS Grid container properties — only meaningful when
    /// `display: grid`. Empty Vec means "no explicit tracks", i.e.
    /// the grid relies entirely on `grid-auto-{rows,columns}`.
    pub grid_template_columns: Vec<TrackSize>,
    pub grid_template_rows: Vec<TrackSize>,
    /// Names declared at each grid line — index `i` holds the names
    /// at the line *before* track `i` (so `len = tracks.len() + 1`).
    /// Empty Vec when `grid_template_columns` is empty.
    pub grid_template_column_line_names: Vec<Vec<String>>,
    pub grid_template_row_line_names: Vec<Vec<String>>,
    pub grid_auto_columns: TrackSize,
    pub grid_auto_rows: TrackSize,
    pub grid_auto_flow: GridAutoFlow,
    /// Grid item placement. `Auto` on both sides triggers auto-placement.
    pub grid_column_start: GridLine,
    pub grid_column_end: GridLine,
    pub grid_row_start: GridLine,
    pub grid_row_end: GridLine,
    /// `grid-template-areas` parsed as a 2D grid of area names. Each
    /// outer Vec is a row; each inner Vec is the named cells in that
    /// row in column order. `.` (period) tokens come through as
    /// literal `"."` and act as a no-area placeholder.
    pub grid_template_areas: Vec<Vec<String>>,
}

impl ComputedValues {
    pub fn root_default() -> Self {
        Self {
            display: Display::Inline,
            color: RgbaColor::BLACK,
            background_color: RgbaColor::TRANSPARENT,
            font_size: 16.0,
            font_family: "serif".to_string(),
            font_weight: FontWeight::Normal,
            font_style: FontStyle::Normal,
            line_height: 1.2,
            text_align: TextAlign::Left,
            white_space: WhiteSpace::Normal,
            margin: EdgeSizes::ZERO,
            padding: EdgeSizes::ZERO,
            border: EdgeSizes::ZERO,
            // CSS spec initial value for `border-color` is
            // `currentcolor` (= the element's `color`). We track
            // whether the cascade has explicitly set border-color
            // via the flag below; `cascade_recursive` finalizes
            // the value to `cv.color` post-cascade when the flag
            // is still false.
            border_color: RgbaColor::BLACK,
            border_color_explicit: false,
            width: Dimension::Auto,
            height: Dimension::Auto,
            flex_direction: FlexDirection::Row,
            justify_content: JustifyContent::FlexStart,
            align_items: AlignItems::Stretch,
            flex_wrap: FlexWrap::Nowrap,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: FlexBasis::Auto,
            position: Position::Static,
            top: None,
            right: None,
            bottom: None,
            left: None,
            margin_left_auto: false,
            margin_right_auto: false,
            margin_top_auto: false,
            margin_bottom_auto: false,
            z_index: None,
            border_radius: [Length::Px(0.0); 4],
            box_sizing: BoxSizing::ContentBox,
            text_decoration_underline: false,
            text_decoration_line_through: false,
            text_decoration_color: None,
            row_gap: 0.0,
            column_gap: 0.0,
            box_shadows: Vec::new(),
            letter_spacing: 0.0,
            background_image: None,
            cursor: Cursor::Default,
            min_width: None,
            max_width: None,
            min_height: None,
            max_height: None,
            opacity: 1.0,
            vars: std::collections::HashMap::new(),
            text_transform: TextTransform::None,
            vertical_align: VerticalAlign::Baseline,
            transform_translate: None,
            outline_width: None,
            outline_color: RgbaColor::BLACK,
            outline_offset: Length::Px(0.0),
            overflow_x: Overflow::Visible,
            overflow_y: Overflow::Visible,
            text_indent: 0.0,
            list_style_type: ListStyleType::Disc,
            aspect_ratio: None,
            content: None,
            float: Float::None,
            clear: Clear::None,
            object_fit: ObjectFit::Fill,
            background_size: BackgroundSize::Auto,
            background_position: BackgroundPosition {
                x: BackgroundAxisPos::Anchor(0.0),
                y: BackgroundAxisPos::Anchor(0.0),
            },
            background_repeat: BackgroundRepeat::Repeat,
            line_clamp: None,
            word_break: WordBreak::Normal,
            overflow_wrap: OverflowWrap::Normal,
            pointer_events: PointerEvents::Auto,
            visibility: Visibility::Visible,
            text_overflow: TextOverflow::Clip,
            text_shadow: None,
            border_collapse: BorderCollapse::Separate,
            border_spacing_x: Length::Px(0.0),
            border_spacing_y: Length::Px(0.0),
            caption_side: CaptionSide::Top,
            grid_template_columns: Vec::new(),
            grid_template_rows: Vec::new(),
            grid_template_column_line_names: Vec::new(),
            grid_template_row_line_names: Vec::new(),
            grid_auto_columns: TrackSize::Auto,
            grid_auto_rows: TrackSize::Auto,
            grid_auto_flow: GridAutoFlow::Row,
            grid_column_start: GridLine::Auto,
            grid_column_end: GridLine::Auto,
            grid_row_start: GridLine::Auto,
            grid_row_end: GridLine::Auto,
            grid_template_areas: Vec::new(),
        }
    }

    /// New ComputedValues seeded with inherited values from `self`. Reset
    /// (non-inherited) properties go back to their initial values.
    pub fn inherit_into_default(&self) -> Self {
        Self {
            display: Display::Inline,
            color: self.color,
            background_color: RgbaColor::TRANSPARENT,
            font_size: self.font_size,
            font_family: self.font_family.clone(),
            font_weight: self.font_weight,
            font_style: self.font_style,
            line_height: self.line_height,
            text_align: self.text_align,
            white_space: self.white_space,
            margin: EdgeSizes::ZERO,
            padding: EdgeSizes::ZERO,
            border: EdgeSizes::ZERO,
            // CSS spec initial value for `border-color` is
            // `currentcolor` (= the element's `color`). We track
            // whether the cascade has explicitly set border-color
            // via the flag below; `cascade_recursive` finalizes
            // the value to `cv.color` post-cascade when the flag
            // is still false.
            border_color: RgbaColor::BLACK,
            border_color_explicit: false,
            width: Dimension::Auto,
            height: Dimension::Auto,
            flex_direction: FlexDirection::Row,
            justify_content: JustifyContent::FlexStart,
            align_items: AlignItems::Stretch,
            flex_wrap: FlexWrap::Nowrap,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: FlexBasis::Auto,
            position: Position::Static,
            top: None,
            right: None,
            bottom: None,
            left: None,
            margin_left_auto: false,
            margin_right_auto: false,
            margin_top_auto: false,
            margin_bottom_auto: false,
            z_index: None,
            border_radius: [Length::Px(0.0); 4],
            box_sizing: BoxSizing::ContentBox,
            // Spec-wise text-decoration is reset, but our cascade
            // applies the inline element's own style to the text node
            // it contains, and we propagate underline to descendants
            // so a single decl on <a> reaches the rendered glyphs.
            text_decoration_underline: self.text_decoration_underline,
            text_decoration_line_through: self.text_decoration_line_through,
            text_decoration_color: self.text_decoration_color,
            row_gap: 0.0,
            column_gap: 0.0,
            // box-shadow isn't inherited.
            box_shadows: Vec::new(),
            letter_spacing: self.letter_spacing,
            background_image: None,
            cursor: self.cursor,
            min_width: None,
            max_width: None,
            min_height: None,
            max_height: None,
            opacity: 1.0,
            // Custom properties ARE inherited per the spec.
            vars: self.vars.clone(),
            text_transform: self.text_transform,
            // vertical-align technically isn't inherited, but applying
            // it consistently across an inline parent's text children
            // is what an author writing `<span style=…>icon text</span>`
            // expects — propagating gives a more predictable result.
            vertical_align: self.vertical_align,
            transform_translate: None,
            outline_width: None,
            outline_color: RgbaColor::BLACK,
            outline_offset: Length::Px(0.0),
            overflow_x: Overflow::Visible,
            overflow_y: Overflow::Visible,
            text_indent: self.text_indent,
            list_style_type: self.list_style_type,
            aspect_ratio: None,
            content: None,
            float: Float::None,
            clear: Clear::None,
            object_fit: ObjectFit::Fill,
            background_size: BackgroundSize::Auto,
            background_position: BackgroundPosition {
                x: BackgroundAxisPos::Anchor(0.0),
                y: BackgroundAxisPos::Anchor(0.0),
            },
            background_repeat: BackgroundRepeat::Repeat,
            line_clamp: None,
            word_break: self.word_break,
            overflow_wrap: self.overflow_wrap,
            pointer_events: self.pointer_events,
            // visibility inherits per CSS spec — children of a
            // hidden ancestor stay hidden unless they re-declare it.
            visibility: self.visibility,
            text_overflow: TextOverflow::Clip,
            text_shadow: self.text_shadow,
            border_collapse: self.border_collapse,
            border_spacing_x: self.border_spacing_x,
            border_spacing_y: self.border_spacing_y,
            caption_side: self.caption_side,
            grid_template_columns: Vec::new(),
            grid_template_rows: Vec::new(),
            grid_template_column_line_names: Vec::new(),
            grid_template_row_line_names: Vec::new(),
            grid_auto_columns: TrackSize::Auto,
            grid_auto_rows: TrackSize::Auto,
            grid_auto_flow: GridAutoFlow::Row,
            grid_column_start: GridLine::Auto,
            grid_column_end: GridLine::Auto,
            grid_row_start: GridLine::Auto,
            grid_row_end: GridLine::Auto,
            grid_template_areas: Vec::new(),
        }
    }
}

pub fn apply_declaration(
    cv: &mut ComputedValues,
    decl: &Declaration,
    parent: Option<&ComputedValues>,
) {
    let v = decl.value.trim();
    // CSS-wide keywords. If the value is `inherit` / `initial` /
    // `unset` / `revert`, route to the matching field instead of
    // falling through the per-property parsers (which don't
    // recognise these keywords and would silently drop the rule).
    // We handle the common typography / layout fields; less-used
    // ones still drop, which matches our existing parser coverage.
    let lc = v.to_ascii_lowercase();
    if matches!(
        lc.as_str(),
        "inherit" | "initial" | "unset" | "revert"
    ) {
        let source: ComputedValues = if lc == "inherit" {
            parent.cloned().unwrap_or_else(ComputedValues::root_default)
        } else {
            ComputedValues::root_default()
        };
        copy_field_by_name(cv, &source, decl.name.as_str());
        return;
    }
    match decl.name.as_str() {
        "display" => {
            if let Some(d) = parse_display(v) {
                cv.display = d;
            }
        }
        "color" => {
            if let Some(c) = parse_color(v) {
                cv.color = c;
            }
        }
        "background-color" => {
            if let Some(c) = parse_color(v) {
                cv.background_color = c;
            } else if let Some(c) = parse_first_gradient_stop(v) {
                // Authors increasingly write `background-color:
                // linear-gradient(...)` (or via the shorthand), which
                // browsers technically reject — but the page often
                // collapses to looking right when at least one stop's
                // color paints. Pull the first stop as a solid
                // fallback so the box gets a coloured background
                // instead of staying transparent.
                cv.background_color = c;
            }
        }
        "background-image" => {
            cv.background_image = parse_url_value(v);
        }
        "background" => {
            // The shorthand can carry colour, image, repeat, position,
            // size, attachment, origin, clip. We tokenise (respecting
            // url(...) / rgb(...) parens), pull a colour and a url out
            // of whichever tokens match, and ignore the rest. `none`
            // resets the image.
            let mut got_color = false;
            for tok in tokenize_top_level(v) {
                if let Some(c) = parse_color(tok) {
                    cv.background_color = c;
                    got_color = true;
                }
            }
            if let Some(url) = parse_url_value(v) {
                cv.background_image = Some(url);
            } else if v.split_ascii_whitespace().any(|t| t.eq_ignore_ascii_case("none")) {
                cv.background_image = None;
            }
            // No solid color found — try the first gradient stop as
            // an approximation. Real gradient rendering is deferred,
            // but a single stop's color makes buttons / cards look
            // far less broken than transparent.
            if !got_color {
                if let Some(c) = parse_first_gradient_stop(v) {
                    cv.background_color = c;
                }
            }
        }
        "font-size" => {
            // CSS resolves `em` / `%` on font-size against the PARENT's
            // computed font-size, not the box's already-set value.
            // Using `cv.font_size` here would chain multiplications
            // when several rules set font-size on the same element
            // (e.g. UA `h1 { font-size: 2em }` followed by author
            // `.firstHeading { font-size: 1.8em }` would compound to
            // 2 × 1.8 × 16 = 57.6 instead of the correct 1.8 × 16 = 28.8).
            let parent_fs = parent.map(|p| p.font_size).unwrap_or(16.0);
            if let Some(px) = parse_font_size(v, parent_fs) {
                cv.font_size = px;
            }
        }
        "font-family" => {
            cv.font_family = v.split(',').next().unwrap_or(v).trim().trim_matches(['"', '\'']).to_string();
        }
        "font-weight" => match v.to_ascii_lowercase().as_str() {
            "bold" | "bolder" => cv.font_weight = FontWeight::Bold,
            "normal" | "lighter" => cv.font_weight = FontWeight::Normal,
            other => {
                if let Ok(n) = other.parse::<u32>() {
                    cv.font_weight = if n >= 600 {
                        FontWeight::Bold
                    } else {
                        FontWeight::Normal
                    };
                }
            }
        },
        "font-style" => match v.to_ascii_lowercase().as_str() {
            "italic" | "oblique" => cv.font_style = FontStyle::Italic,
            _ => cv.font_style = FontStyle::Normal,
        },
        "line-height" => {
            if let Some(lh) = parse_line_height(v, cv.font_size) {
                cv.line_height = lh;
            }
        }
        "text-align" => match v.to_ascii_lowercase().as_str() {
            "right" => cv.text_align = TextAlign::Right,
            "center" => cv.text_align = TextAlign::Center,
            "justify" => cv.text_align = TextAlign::Justify,
            _ => cv.text_align = TextAlign::Left,
        },
        "text-transform" => match v.to_ascii_lowercase().as_str() {
            "uppercase" => cv.text_transform = TextTransform::Uppercase,
            "lowercase" => cv.text_transform = TextTransform::Lowercase,
            "capitalize" => cv.text_transform = TextTransform::Capitalize,
            _ => cv.text_transform = TextTransform::None,
        },
        "vertical-align" => {
            cv.vertical_align = match v.to_ascii_lowercase().as_str() {
                "top" => VerticalAlign::Top,
                "middle" => VerticalAlign::Middle,
                "bottom" => VerticalAlign::Bottom,
                "text-top" => VerticalAlign::TextTop,
                "text-bottom" => VerticalAlign::TextBottom,
                "sub" => VerticalAlign::Sub,
                "super" => VerticalAlign::Super,
                _ => VerticalAlign::Baseline,
            };
        }
        "transform" => {
            cv.transform_translate = parse_transform_translate(v);
        }
        "outline" => apply_outline_shorthand(v, cv),
        "outline-offset" => {
            if let Some(l) = parse_length(v) {
                cv.outline_offset = l;
            }
        }
        "flex-flow" => {
            // Two-token shorthand for flex-direction + flex-wrap; the
            // tokens are independent in the spec so order doesn't
            // matter — we route each one to the matching field
            // without caring which slot it appears in.
            for tok in v.split_ascii_whitespace() {
                let lc = tok.to_ascii_lowercase();
                match lc.as_str() {
                    "row" => cv.flex_direction = FlexDirection::Row,
                    "row-reverse" => cv.flex_direction = FlexDirection::RowReverse,
                    "column" => cv.flex_direction = FlexDirection::Column,
                    "column-reverse" => cv.flex_direction = FlexDirection::ColumnReverse,
                    "wrap" => cv.flex_wrap = FlexWrap::Wrap,
                    "wrap-reverse" => cv.flex_wrap = FlexWrap::WrapReverse,
                    "nowrap" => cv.flex_wrap = FlexWrap::Nowrap,
                    _ => {}
                }
            }
        }
        "outline-width" => {
            if let Some(l) = parse_length(v) {
                cv.outline_width = Some(l);
            }
        }
        "outline-color" => {
            if let Some(c) = parse_color(v) {
                cv.outline_color = c;
            }
        }
        "overflow" => {
            // Shorthand: `overflow: hidden` sets both axes; two values
            // give x then y per CSS Overflow Module 3.
            let parts: Vec<&str> = v.split_ascii_whitespace().collect();
            let x = parts.first().and_then(|p| parse_overflow_kw(p));
            let y = parts.get(1).and_then(|p| parse_overflow_kw(p)).or(x);
            if let Some(xv) = x {
                cv.overflow_x = xv;
            }
            if let Some(yv) = y {
                cv.overflow_y = yv;
            }
        }
        "overflow-x" => {
            if let Some(o) = parse_overflow_kw(v) {
                cv.overflow_x = o;
            }
        }
        "overflow-y" => {
            if let Some(o) = parse_overflow_kw(v) {
                cv.overflow_y = o;
            }
        }
        "text-indent" => {
            if let Some(l) = parse_length(v) {
                cv.text_indent = l.resolve(cv.font_size, 16.0, 0.0);
            }
        }
        "list-style-type" => {
            cv.list_style_type = match v.to_ascii_lowercase().as_str() {
                "none" => ListStyleType::None,
                "disc" => ListStyleType::Disc,
                "circle" => ListStyleType::Circle,
                "square" => ListStyleType::Square,
                "decimal" => ListStyleType::Decimal,
                "decimal-leading-zero" => ListStyleType::DecimalLeadingZero,
                "lower-alpha" | "lower-latin" => ListStyleType::LowerAlpha,
                "upper-alpha" | "upper-latin" => ListStyleType::UpperAlpha,
                "lower-roman" => ListStyleType::LowerRoman,
                "upper-roman" => ListStyleType::UpperRoman,
                _ => cv.list_style_type, // unknown — keep current
            };
        }
        "list-style" => {
            // Shorthand: type + position + image. We only honour the
            // type keyword; everything else is ignored.
            for tok in v.split_ascii_whitespace() {
                let lc = tok.to_ascii_lowercase();
                if let Some(t) = parse_list_style_type_kw(&lc) {
                    cv.list_style_type = t;
                }
            }
        }
        "aspect-ratio" => {
            cv.aspect_ratio = parse_aspect_ratio(v);
        }
        "content" => {
            cv.content = parse_content_string(v);
        }
        "float" => {
            cv.float = match v.to_ascii_lowercase().as_str() {
                "left" => Float::Left,
                "right" => Float::Right,
                _ => Float::None,
            };
        }
        "clear" => {
            cv.clear = match v.to_ascii_lowercase().as_str() {
                "left" => Clear::Left,
                "right" => Clear::Right,
                "both" => Clear::Both,
                _ => Clear::None,
            };
        }
        "white-space" => match v.to_ascii_lowercase().as_str() {
            "pre" => cv.white_space = WhiteSpace::Pre,
            "nowrap" => cv.white_space = WhiteSpace::Nowrap,
            "pre-wrap" => cv.white_space = WhiteSpace::PreWrap,
            _ => cv.white_space = WhiteSpace::Normal,
        },
        "width" => cv.width = parse_dimension(v).unwrap_or(cv.width),
        "height" => cv.height = parse_dimension(v).unwrap_or(cv.height),
        "min-width" => cv.min_width = parse_optional_length(v),
        "max-width" => cv.max_width = parse_optional_length(v),
        "min-height" => cv.min_height = parse_optional_length(v),
        "max-height" => cv.max_height = parse_optional_length(v),
        "opacity" => {
            // Accept either a unit-less float or a percentage.
            if let Some(p) = v.strip_suffix('%') {
                if let Ok(n) = p.trim().parse::<f32>() {
                    cv.opacity = (n / 100.0).clamp(0.0, 1.0);
                }
            } else if let Ok(n) = v.trim().parse::<f32>() {
                cv.opacity = n.clamp(0.0, 1.0);
            }
        }
        "margin" => apply_margin_shorthand(v, cv),
        "margin-top" => {
            cv.margin_top_auto = v.eq_ignore_ascii_case("auto");
            if !cv.margin_top_auto {
                set_edge(&mut cv.margin.top, v);
            } else {
                cv.margin.top = Length::Px(0.0);
            }
        }
        "margin-right" => {
            cv.margin_right_auto = v.eq_ignore_ascii_case("auto");
            if !cv.margin_right_auto {
                set_edge(&mut cv.margin.right, v);
            } else {
                cv.margin.right = Length::Px(0.0);
            }
        }
        "margin-bottom" => {
            cv.margin_bottom_auto = v.eq_ignore_ascii_case("auto");
            if !cv.margin_bottom_auto {
                set_edge(&mut cv.margin.bottom, v);
            } else {
                cv.margin.bottom = Length::Px(0.0);
            }
        }
        "margin-left" => {
            cv.margin_left_auto = v.eq_ignore_ascii_case("auto");
            if !cv.margin_left_auto {
                set_edge(&mut cv.margin.left, v);
            } else {
                cv.margin.left = Length::Px(0.0);
            }
        }
        // CSS Logical Properties (LTR + horizontal-tb mode):
        //   block-start  = top      block-end  = bottom
        //   inline-start = left     inline-end = right
        // We don't model `writing-mode` rotation, so for now this maps
        // statically to the physical sides. Stylesheets that ship
        // logical-property declarations for RTL fallback still get
        // the right physical placement in the LTR case.
        "margin-block-start" => set_edge(&mut cv.margin.top, v),
        "margin-block-end" => set_edge(&mut cv.margin.bottom, v),
        "margin-inline-start" => set_edge(&mut cv.margin.left, v),
        "margin-inline-end" => set_edge(&mut cv.margin.right, v),
        "margin-block" => apply_axis_shorthand(v, &mut cv.margin.top, &mut cv.margin.bottom),
        "margin-inline" => apply_axis_shorthand(v, &mut cv.margin.left, &mut cv.margin.right),
        "padding" => apply_edge_shorthand(v, &mut cv.padding),
        "padding-top" => set_edge(&mut cv.padding.top, v),
        "padding-right" => set_edge(&mut cv.padding.right, v),
        "padding-bottom" => set_edge(&mut cv.padding.bottom, v),
        "padding-left" => set_edge(&mut cv.padding.left, v),
        "padding-block-start" => set_edge(&mut cv.padding.top, v),
        "padding-block-end" => set_edge(&mut cv.padding.bottom, v),
        "padding-inline-start" => set_edge(&mut cv.padding.left, v),
        "padding-inline-end" => set_edge(&mut cv.padding.right, v),
        "padding-block" => apply_axis_shorthand(v, &mut cv.padding.top, &mut cv.padding.bottom),
        "padding-inline" => apply_axis_shorthand(v, &mut cv.padding.left, &mut cv.padding.right),
        "border-block-start-width" => set_edge(&mut cv.border.top, v),
        "border-block-end-width" => set_edge(&mut cv.border.bottom, v),
        "border-inline-start-width" => set_edge(&mut cv.border.left, v),
        "border-inline-end-width" => set_edge(&mut cv.border.right, v),
        "block-size" => {
            if let Some(d) = parse_dimension(v) {
                cv.height = d;
            }
        }
        "inline-size" => {
            if let Some(d) = parse_dimension(v) {
                cv.width = d;
            }
        }
        "min-block-size" => cv.min_height = parse_optional_length(v),
        "max-block-size" => cv.max_height = parse_optional_length(v),
        "min-inline-size" => cv.min_width = parse_optional_length(v),
        "max-inline-size" => cv.max_width = parse_optional_length(v),
        "border-width" => apply_edge_shorthand(v, &mut cv.border),
        "border-color" => {
            if let Some(c) = parse_color(v) {
                cv.border_color = c;
                cv.border_color_explicit = true;
            }
        }
        // `border-style` — we don't render dashed / dotted / double
        // styles, so the only meaningful keywords are `none` and
        // `hidden`, both of which suppress the border. Zero the
        // widths so paint doesn't stroke anything. Other styles
        // parse-and-ignore (the existing widths stay).
        "border-style" => {
            if v.split_ascii_whitespace().any(|t| {
                t.eq_ignore_ascii_case("none") || t.eq_ignore_ascii_case("hidden")
            }) {
                cv.border = EdgeSizes::ZERO;
            }
        }
        "border" => {
            // border: 1px solid red — pull width and color, ignore
            // style. The keyword `none` (or `hidden`, or a literal
            // `0` width) suppresses the border entirely; without
            // this Google's `.gLFyf { border: none }` was silently
            // dropped and the UA stylesheet's `textarea { border:
            // 1px gray }` kept painting a 1 px ring around the
            // search box's textarea control inside the rounded
            // pill — the user-visible "black box."
            let lower = v.to_ascii_lowercase();
            let suppressed = v.split_ascii_whitespace().any(|t| {
                let t = t.trim_end_matches(';');
                t.eq_ignore_ascii_case("none")
                    || t.eq_ignore_ascii_case("hidden")
                    || t == "0"
                    || t.eq_ignore_ascii_case("0px")
            });
            if suppressed {
                cv.border = EdgeSizes::ZERO;
                // Spec: `border: none` is shorthand for
                // `border-color: currentColor`. Reset the explicit
                // flag so the post-cascade fixup pulls cv.color in.
                cv.border_color_explicit = false;
            } else {
                let _ = lower; // reserved for future style parsing
                for tok in v.split_ascii_whitespace() {
                    if let Some(l) = parse_length(tok) {
                        cv.border = EdgeSizes::uniform(l);
                    } else if let Some(c) = parse_color(tok) {
                        cv.border_color = c;
                        cv.border_color_explicit = true;
                    }
                }
            }
        }
        // ---- Flexbox container ----
        "flex-direction" => match v.to_ascii_lowercase().as_str() {
            "column" => cv.flex_direction = FlexDirection::Column,
            "row-reverse" => cv.flex_direction = FlexDirection::RowReverse,
            "column-reverse" => cv.flex_direction = FlexDirection::ColumnReverse,
            _ => cv.flex_direction = FlexDirection::Row,
        },
        "justify-content" => match v.to_ascii_lowercase().as_str() {
            "flex-end" | "end" | "right" => cv.justify_content = JustifyContent::FlexEnd,
            "center" => cv.justify_content = JustifyContent::Center,
            "space-between" => cv.justify_content = JustifyContent::SpaceBetween,
            "space-around" => cv.justify_content = JustifyContent::SpaceAround,
            "space-evenly" => cv.justify_content = JustifyContent::SpaceEvenly,
            _ => cv.justify_content = JustifyContent::FlexStart,
        },
        "align-items" => match v.to_ascii_lowercase().as_str() {
            "flex-start" | "start" => cv.align_items = AlignItems::FlexStart,
            "flex-end" | "end" => cv.align_items = AlignItems::FlexEnd,
            "center" => cv.align_items = AlignItems::Center,
            "baseline" => cv.align_items = AlignItems::Baseline,
            _ => cv.align_items = AlignItems::Stretch,
        },
        "place-items" => {
            // Two-value shorthand for `align-items` + `justify-items`.
            // We don't model `justify-items` (only flex's
            // justify-content), so we reuse the first value for
            // align-items and let the second drift.
            let parts: Vec<&str> = v.split_ascii_whitespace().collect();
            if let Some(a) = parts.first() {
                let _ = parts; // shadow above
                cv.align_items = match a.to_ascii_lowercase().as_str() {
                    "flex-start" | "start" => AlignItems::FlexStart,
                    "flex-end" | "end" => AlignItems::FlexEnd,
                    "center" => AlignItems::Center,
                    "baseline" => AlignItems::Baseline,
                    _ => AlignItems::Stretch,
                };
            }
        }
        "place-content" => {
            // Two-value shorthand: align-content + justify-content.
            // We model justify-content; align-content isn't a separate
            // field today, so we just route the second token to
            // justify-content.
            let parts: Vec<&str> = v.split_ascii_whitespace().collect();
            let jc = parts.get(1).or(parts.first()).copied().unwrap_or("flex-start");
            cv.justify_content = match jc.to_ascii_lowercase().as_str() {
                "flex-end" | "end" | "right" => JustifyContent::FlexEnd,
                "center" => JustifyContent::Center,
                "space-between" => JustifyContent::SpaceBetween,
                "space-around" => JustifyContent::SpaceAround,
                "space-evenly" => JustifyContent::SpaceEvenly,
                _ => JustifyContent::FlexStart,
            };
        }
        "place-self" | "align-self" | "justify-self" | "justify-items" | "align-content" => {
            // Modern alignment properties parsed-and-ignored — our
            // flex / grid layouts only model the container-side
            // align-items / justify-content. Stops common stylesheets
            // from dropping rules silently.
            let _ = v;
        }
        "flex-wrap" => match v.to_ascii_lowercase().as_str() {
            "wrap" => cv.flex_wrap = FlexWrap::Wrap,
            "wrap-reverse" => cv.flex_wrap = FlexWrap::WrapReverse,
            _ => cv.flex_wrap = FlexWrap::Nowrap,
        },
        // ---- Flexbox item ----
        "flex-grow" => {
            if let Ok(n) = v.parse::<f32>() {
                cv.flex_grow = n.max(0.0);
            }
        }
        "flex-shrink" => {
            if let Ok(n) = v.parse::<f32>() {
                cv.flex_shrink = n.max(0.0);
            }
        }
        "flex-basis" => {
            if v.eq_ignore_ascii_case("auto") {
                cv.flex_basis = FlexBasis::Auto;
            } else if let Some(l) = parse_length(v) {
                cv.flex_basis = FlexBasis::Length(l);
            }
        }
        "flex" => apply_flex_shorthand(cv, v),
        "font" => apply_font_shorthand(cv, v),
        "cursor" => {
            cv.cursor = match v.split(',').next().unwrap_or(v).trim().to_ascii_lowercase().as_str() {
                "pointer" => Cursor::Pointer,
                "text" => Cursor::Text,
                "not-allowed" => Cursor::NotAllowed,
                "wait" => Cursor::Wait,
                "crosshair" => Cursor::Crosshair,
                "move" => Cursor::Move,
                "help" => Cursor::Help,
                "progress" => Cursor::Progress,
                _ => Cursor::Default,
            };
        }
        "letter-spacing" => {
            if v.eq_ignore_ascii_case("normal") {
                cv.letter_spacing = 0.0;
            } else if let Some(l) = parse_length(v) {
                cv.letter_spacing = l.resolve(cv.font_size, 16.0, 0.0);
            }
        }
        // ---- Positioning ----
        "position" => match v.to_ascii_lowercase().as_str() {
            "relative" => cv.position = Position::Relative,
            "absolute" => cv.position = Position::Absolute,
            "fixed" => cv.position = Position::Fixed,
            "sticky" => cv.position = Position::Sticky,
            _ => cv.position = Position::Static,
        },
        "top" => cv.top = parse_offset(v),
        "right" => cv.right = parse_offset(v),
        "bottom" => cv.bottom = parse_offset(v),
        "left" => cv.left = parse_offset(v),
        "inset" => {
            // CSS Logical Properties shorthand: 1, 2, 3, or 4 values
            // map to top / right / bottom / left, same convention as
            // margin / padding shorthands.
            let parts: Vec<&str> = v.split_ascii_whitespace().collect();
            let (top, right, bottom, left) = match parts.len() {
                1 => (parts[0], parts[0], parts[0], parts[0]),
                2 => (parts[0], parts[1], parts[0], parts[1]),
                3 => (parts[0], parts[1], parts[2], parts[1]),
                4 => (parts[0], parts[1], parts[2], parts[3]),
                _ => return,
            };
            cv.top = parse_offset(top);
            cv.right = parse_offset(right);
            cv.bottom = parse_offset(bottom);
            cv.left = parse_offset(left);
        }
        "inset-block" | "inset-inline" => {
            // 1- or 2-value shorthand for one axis. We don't model
            // logical-mode rotation, so block = top/bottom and
            // inline = left/right (matches LTR + horizontal-tb).
            let parts: Vec<&str> = v.split_ascii_whitespace().collect();
            let (start_v, end_v) = match parts.len() {
                1 => (parts[0], parts[0]),
                2 => (parts[0], parts[1]),
                _ => return,
            };
            if decl.name == "inset-block" {
                cv.top = parse_offset(start_v);
                cv.bottom = parse_offset(end_v);
            } else {
                cv.left = parse_offset(start_v);
                cv.right = parse_offset(end_v);
            }
        }
        "text-decoration" | "text-decoration-line" => {
            let lc = v.to_ascii_lowercase();
            let toks: Vec<&str> = lc.split_ascii_whitespace().collect();
            if toks.iter().any(|t| *t == "none") {
                cv.text_decoration_underline = false;
                cv.text_decoration_line_through = false;
            } else {
                // Multiple line keywords can appear together on the
                // shorthand (`text-decoration: underline line-through`).
                if toks.iter().any(|t| *t == "underline") {
                    cv.text_decoration_underline = true;
                }
                if toks.iter().any(|t| *t == "line-through") {
                    cv.text_decoration_line_through = true;
                }
            }
            // Pull a colour from the shorthand — first parsable token wins.
            for tok in tokenize_top_level(v) {
                if let Some(c) = parse_color(tok) {
                    cv.text_decoration_color = Some(c);
                    break;
                }
            }
        }
        // Animation / transition / will-change are parsed-and-ignored.
        // We don't drive a tick loop, so animated values don't move,
        // but accepting the declarations keeps stylesheets that gate
        // initial-frame styles on `transition: opacity 0.2s` etc.
        // from silently dropping the rule.
        "transition"
        | "transition-property"
        | "transition-duration"
        | "transition-timing-function"
        | "transition-delay"
        | "animation"
        | "animation-name"
        | "animation-duration"
        | "animation-timing-function"
        | "animation-delay"
        | "animation-iteration-count"
        | "animation-direction"
        | "animation-fill-mode"
        | "animation-play-state"
        | "will-change" => {
            let _ = v;
        }
        // Browser-paint hints with no visible effect on our compositor
        // (no Gaussian filter, no native form-control theming, no
        // scrolling chrome, no clip path).
        "appearance"
        | "-webkit-appearance"
        | "filter"
        | "backdrop-filter"
        | "clip-path"
        | "mask"
        | "mask-image"
        | "mix-blend-mode"
        | "isolation"
        | "contain"
        | "content-visibility"
        | "scrollbar-width"
        | "scrollbar-color"
        | "scrollbar-gutter"
        | "scroll-behavior"
        | "scroll-margin"
        | "scroll-margin-top"
        | "scroll-margin-bottom"
        | "scroll-margin-left"
        | "scroll-margin-right"
        | "scroll-padding"
        | "scroll-snap-type"
        | "scroll-snap-align"
        | "overscroll-behavior"
        | "touch-action"
        | "user-select"
        | "tab-size"
        | "text-rendering"
        | "-webkit-font-smoothing"
        | "-moz-osx-font-smoothing"
        | "font-feature-settings"
        | "font-variant"
        | "font-variant-caps"
        | "font-variant-ligatures"
        | "font-variant-numeric"
        | "font-stretch"
        | "font-kerning"
        | "font-display"
        | "image-rendering"
        | "transform-origin"
        | "transform-style"
        | "perspective"
        | "perspective-origin"
        | "backface-visibility"
        | "shape-outside"
        | "shape-margin"
        | "page-break-before"
        | "page-break-after"
        | "page-break-inside"
        | "break-before"
        | "break-after"
        | "break-inside"
        | "orphans"
        | "widows"
        | "hyphens" => {
            let _ = v;
        }
        "accent-color" | "caret-color" => {
            // Parsed-and-ignored modern colour properties. We don't
            // theme native form controls' accent (radio dots,
            // checkboxes) yet, and we don't render a caret in
            // page-side text inputs — the chrome address bar is the
            // only one with a caret today and it uses its own colour.
            // Stops author CSS from silently dropping these declarations
            // into the unknown-property bucket.
            let _ = v;
        }
        "text-decoration-color" => {
            if v.eq_ignore_ascii_case("currentcolor") || v.eq_ignore_ascii_case("inherit") {
                cv.text_decoration_color = None;
            } else if let Some(c) = parse_color(v) {
                cv.text_decoration_color = Some(c);
            }
        }
        "text-decoration-style" | "text-decoration-thickness" => {
            // Parsed but ignored for now — we render solid underlines
            // at a fixed thickness (font_size * 0.07). Stops the
            // declarations from silently dropping into the unknown-
            // property bucket.
            let _ = v;
        }
        // ---- Flex gap ----
        "gap" => {
            let parts: Vec<Length> = v
                .split_ascii_whitespace()
                .filter_map(parse_length)
                .collect();
            match parts.len() {
                1 => {
                    cv.row_gap = parts[0].resolve(cv.font_size, 16.0, 0.0);
                    cv.column_gap = cv.row_gap;
                }
                2 => {
                    cv.row_gap = parts[0].resolve(cv.font_size, 16.0, 0.0);
                    cv.column_gap = parts[1].resolve(cv.font_size, 16.0, 0.0);
                }
                _ => {}
            }
        }
        "row-gap" => {
            if let Some(l) = parse_length(v) {
                cv.row_gap = l.resolve(cv.font_size, 16.0, 0.0);
            }
        }
        "column-gap" => {
            if let Some(l) = parse_length(v) {
                cv.column_gap = l.resolve(cv.font_size, 16.0, 0.0);
            }
        }
        "box-shadow" => {
            cv.box_shadows = parse_box_shadows(v);
        }
        // ---- Box model ----
        "box-sizing" => match v.to_ascii_lowercase().as_str() {
            "border-box" => cv.box_sizing = BoxSizing::BorderBox,
            _ => cv.box_sizing = BoxSizing::ContentBox,
        },
        // ---- Border radius ----
        "border-radius" => apply_border_radius(v, &mut cv.border_radius),
        "border-top-left-radius" => set_radius(&mut cv.border_radius[0], v),
        "border-top-right-radius" => set_radius(&mut cv.border_radius[1], v),
        "border-bottom-right-radius" => set_radius(&mut cv.border_radius[2], v),
        "border-bottom-left-radius" => set_radius(&mut cv.border_radius[3], v),
        "text-shadow" => {
            cv.text_shadow = parse_text_shadow(v);
        }
        // ---- Replaced-element fitting ----
        "line-clamp" | "-webkit-line-clamp" => {
            let trimmed = v.trim();
            if trimmed.eq_ignore_ascii_case("none") {
                cv.line_clamp = None;
            } else if let Ok(n) = trimmed.parse::<u32>() {
                if n >= 1 {
                    cv.line_clamp = Some(n);
                }
            }
        }
        "word-break" => {
            cv.word_break = match v.to_ascii_lowercase().as_str() {
                "break-all" => WordBreak::BreakAll,
                "keep-all" => WordBreak::KeepAll,
                _ => WordBreak::Normal,
            };
        }
        "overflow-wrap" | "word-wrap" => {
            cv.overflow_wrap = match v.to_ascii_lowercase().as_str() {
                "break-word" => OverflowWrap::BreakWord,
                "anywhere" => OverflowWrap::Anywhere,
                _ => OverflowWrap::Normal,
            };
        }
        "pointer-events" => {
            cv.pointer_events = match v.to_ascii_lowercase().as_str() {
                "none" => PointerEvents::None,
                _ => PointerEvents::Auto,
            };
        }
        "visibility" => {
            cv.visibility = match v.to_ascii_lowercase().as_str() {
                "hidden" => Visibility::Hidden,
                "collapse" => Visibility::Collapse,
                _ => Visibility::Visible,
            };
        }
        "text-overflow" => {
            cv.text_overflow = match v.to_ascii_lowercase().as_str() {
                "ellipsis" => TextOverflow::Ellipsis,
                _ => TextOverflow::Clip,
            };
        }
        "z-index" => {
            cv.z_index = if v.eq_ignore_ascii_case("auto") {
                None
            } else {
                v.trim().parse::<i32>().ok()
            };
        }
        "background-size" => {
            cv.background_size = parse_background_size(v).unwrap_or(BackgroundSize::Auto);
        }
        "background-position" => {
            if let Some(p) = parse_background_position(v) {
                cv.background_position = p;
            }
        }
        "background-repeat" => {
            cv.background_repeat = match v.to_ascii_lowercase().as_str() {
                "no-repeat" => BackgroundRepeat::NoRepeat,
                "repeat-x" => BackgroundRepeat::RepeatX,
                "repeat-y" => BackgroundRepeat::RepeatY,
                _ => BackgroundRepeat::Repeat,
            };
        }
        "object-fit" => {
            cv.object_fit = match v.to_ascii_lowercase().as_str() {
                "contain" => ObjectFit::Contain,
                "cover" => ObjectFit::Cover,
                "none" => ObjectFit::None,
                "scale-down" => ObjectFit::ScaleDown,
                _ => ObjectFit::Fill,
            };
        }
        // ---- Tables ----
        "border-spacing" => {
            let parts: Vec<Length> = v
                .split_ascii_whitespace()
                .filter_map(parse_length)
                .collect();
            match parts.len() {
                1 => {
                    cv.border_spacing_x = parts[0];
                    cv.border_spacing_y = parts[0];
                }
                2 => {
                    cv.border_spacing_x = parts[0];
                    cv.border_spacing_y = parts[1];
                }
                _ => {}
            }
        }
        "caption-side" => {
            cv.caption_side = match v.to_ascii_lowercase().as_str() {
                "bottom" => CaptionSide::Bottom,
                _ => CaptionSide::Top,
            };
        }
        "border-collapse" => {
            cv.border_collapse = match v.to_ascii_lowercase().as_str() {
                "collapse" => BorderCollapse::Collapse,
                _ => BorderCollapse::Separate,
            };
        }
        // ---- CSS Grid ----
        "grid-template-columns" => {
            let (t, n) = parse_track_template(v);
            cv.grid_template_columns = t;
            cv.grid_template_column_line_names = n;
        }
        "grid-template-rows" => {
            let (t, n) = parse_track_template(v);
            cv.grid_template_rows = t;
            cv.grid_template_row_line_names = n;
        }
        "grid-template" | "grid" => {
            // CSS Grid shorthand. Common forms:
            //   `none` — clear all grid-template-* on the container.
            //   `<rows> / <columns>` — set both axes.
            //   `<rows>` (no slash) — rows only.
            // We don't yet handle the inline-areas form
            //   (`"a a a" auto / 1fr 1fr 1fr`)
            // — that would need to fold area names + row tracks
            // together; rare in practice. The slash form covers
            // Wikipedia, MDN-style resets, and most authoring.
            if v.trim().eq_ignore_ascii_case("none") {
                cv.grid_template_rows = Vec::new();
                cv.grid_template_columns = Vec::new();
                cv.grid_template_row_line_names = Vec::new();
                cv.grid_template_column_line_names = Vec::new();
                cv.grid_template_areas = Vec::new();
            } else if let Some((rows, cols)) = v.split_once('/') {
                let (r, rn) = parse_track_template(rows.trim());
                let (c, cn) = parse_track_template(cols.trim());
                cv.grid_template_rows = r;
                cv.grid_template_row_line_names = rn;
                cv.grid_template_columns = c;
                cv.grid_template_column_line_names = cn;
            } else {
                let (r, rn) = parse_track_template(v.trim());
                cv.grid_template_rows = r;
                cv.grid_template_row_line_names = rn;
            }
        }
        "grid-auto-columns" => {
            if let Some(t) = parse_track_size(v) {
                cv.grid_auto_columns = t;
            }
        }
        "grid-auto-rows" => {
            if let Some(t) = parse_track_size(v) {
                cv.grid_auto_rows = t;
            }
        }
        "grid-auto-flow" => {
            cv.grid_auto_flow = match v.to_ascii_lowercase().as_str() {
                "column" | "column dense" => GridAutoFlow::Column,
                _ => GridAutoFlow::Row,
            };
        }
        "grid-column" => {
            let (s, e) = parse_grid_axis_shorthand(v);
            cv.grid_column_start = s;
            cv.grid_column_end = e;
        }
        "grid-row" => {
            let (s, e) = parse_grid_axis_shorthand(v);
            cv.grid_row_start = s;
            cv.grid_row_end = e;
        }
        "grid-column-start" => cv.grid_column_start = parse_grid_line(v),
        "grid-column-end" => cv.grid_column_end = parse_grid_line(v),
        "grid-row-start" => cv.grid_row_start = parse_grid_line(v),
        "grid-row-end" => cv.grid_row_end = parse_grid_line(v),
        "grid-area" => {
            // Two valid shapes: 4-slash form (row-start / column-start
            // / row-end / column-end) or a single area-name reference.
            let parts: Vec<&str> = v.split('/').map(str::trim).collect();
            if parts.len() == 4 {
                cv.grid_row_start = parse_grid_line(parts[0]);
                cv.grid_column_start = parse_grid_line(parts[1]);
                cv.grid_row_end = parse_grid_line(parts[2]);
                cv.grid_column_end = parse_grid_line(parts[3]);
            } else if parts.len() == 1 {
                let name = parse_grid_line(parts[0]);
                if matches!(name, GridLine::Named(_)) {
                    cv.grid_row_start = name.clone();
                    cv.grid_column_start = name.clone();
                    cv.grid_row_end = name.clone();
                    cv.grid_column_end = name;
                }
            }
        }
        "grid-template-areas" => {
            cv.grid_template_areas = parse_template_areas(v);
        }
        _ => {}
    }
}

fn set_radius(slot: &mut Length, v: &str) {
    if let Some(l) = parse_length(v) {
        *slot = l;
    }
}

/// `border-radius` shorthand. CSS allows up to four space-separated
/// values for the four corners, plus an optional `/` followed by up
/// to four values for the vertical radii (we ignore the second half
/// for now and use the horizontal radii on both axes).
fn apply_border_radius(v: &str, radii: &mut [Length; 4]) {
    let h_part = v.split('/').next().unwrap_or(v).trim();
    let parts: Vec<Length> = h_part
        .split_ascii_whitespace()
        .filter_map(parse_length)
        .collect();
    match parts.len() {
        1 => *radii = [parts[0]; 4],
        2 => {
            radii[0] = parts[0]; // top-left
            radii[1] = parts[1]; // top-right
            radii[2] = parts[0]; // bottom-right
            radii[3] = parts[1]; // bottom-left
        }
        3 => {
            radii[0] = parts[0];
            radii[1] = parts[1];
            radii[2] = parts[2];
            radii[3] = parts[1];
        }
        4 => {
            radii[0] = parts[0];
            radii[1] = parts[1];
            radii[2] = parts[2];
            radii[3] = parts[3];
        }
        _ => {}
    }
}

/// Whitespace-tokenise a value at the top level — paren-balanced so
/// `rgb(0, 0, 0)` and `url(foo)` stay together as single tokens.
fn tokenize_top_level(v: &str) -> Vec<&str> {
    let bytes = v.as_bytes();
    let mut tokens = Vec::new();
    let mut start = 0usize;
    let mut paren = 0i32;
    let mut in_quote = 0u8;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'"' | b'\'' if in_quote == 0 => in_quote = b,
            b if b == in_quote => in_quote = 0,
            b'(' if in_quote == 0 => paren += 1,
            b')' if in_quote == 0 => paren -= 1,
            _ => {}
        }
        if (b == b' ' || b == b'\t') && paren == 0 && in_quote == 0 {
            if start < i {
                tokens.push(&v[start..i]);
            }
            start = i + 1;
        }
        i += 1;
    }
    if start < v.len() {
        tokens.push(&v[start..]);
    }
    tokens
}

/// Pull the first `url(...)` out of a value (which may be a full
/// `background:` shorthand), strip optional surrounding quotes, and
/// return the URL string. Returns `None` if no `url(...)` is present
/// or if the URL is empty / `none`.
fn parse_url_value(v: &str) -> Option<String> {
    let lower = v.to_ascii_lowercase();
    let start = lower.find("url(")?;
    let after = &v[start + 4..];
    let end = after.find(')')?;
    let inner = after[..end].trim();
    let unquoted = if (inner.starts_with('"') && inner.ends_with('"'))
        || (inner.starts_with('\'') && inner.ends_with('\''))
    {
        &inner[1..inner.len() - 1]
    } else {
        inner
    };
    if unquoted.is_empty() || unquoted.eq_ignore_ascii_case("none") {
        None
    } else {
        Some(unquoted.to_string())
    }
}

/// Parse a `box-shadow` declaration into a list of shadow layers.
/// Comma-separates the value at the top level (paren-aware so
/// `rgba(...)` survives), then parses each layer separately.
/// Parse a single-layer `text-shadow` (multi-layer falls back to the
/// first). Tokens may appear in any order: lengths fill `offset_x`
/// then `offset_y` then `blur` (optional); a colour can sit anywhere.
/// `none` clears.
fn parse_text_shadow(v: &str) -> Option<TextShadow> {
    if v.trim().eq_ignore_ascii_case("none") || v.trim().is_empty() {
        return None;
    }
    // Take only the first layer up to a top-level comma.
    let bytes = v.as_bytes();
    let mut depth = 0i32;
    let mut end = bytes.len();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    let first = &v[..end];

    // Same tokenisation as parse_one_box_shadow (paren-aware split).
    let mut tokens: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut paren = 0i32;
    for ch in first.chars() {
        if ch == '(' {
            paren += 1;
            buf.push(ch);
            continue;
        }
        if ch == ')' {
            paren -= 1;
            buf.push(ch);
            continue;
        }
        if ch.is_ascii_whitespace() && paren == 0 {
            if !buf.is_empty() {
                tokens.push(std::mem::take(&mut buf));
            }
        } else {
            buf.push(ch);
        }
    }
    if !buf.is_empty() {
        tokens.push(buf);
    }
    let mut lengths: Vec<Length> = Vec::new();
    let mut color: Option<RgbaColor> = None;
    for tok in tokens {
        if let Some(c) = parse_color(&tok) {
            color = Some(c);
        } else if let Some(l) = parse_length(&tok) {
            lengths.push(l);
        }
    }
    if lengths.len() < 2 {
        return None;
    }
    Some(TextShadow {
        offset_x: lengths[0],
        offset_y: lengths[1],
        blur: lengths.get(2).copied().unwrap_or(Length::Px(0.0)),
        color: color.unwrap_or(RgbaColor::BLACK),
    })
}

fn parse_box_shadows(v: &str) -> Vec<BoxShadow> {
    if v.trim().eq_ignore_ascii_case("none") {
        return Vec::new();
    }
    // Split on top-level commas so `rgba(0,0,0,0.4)` doesn't split.
    let mut layers: Vec<&str> = Vec::new();
    let bytes = v.as_bytes();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                layers.push(v[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    layers.push(v[start..].trim());
    layers
        .into_iter()
        .filter_map(parse_one_box_shadow)
        .collect()
}

/// Parse a single shadow layer (no comma alternation). Tokens may
/// appear in any order: lengths fill `offset_x` / `offset_y` /
/// `blur` / `spread` (in that order); a colour may appear anywhere;
/// `inset` is recognised but skipped (we render outset shadows only).
fn parse_one_box_shadow(first: &str) -> Option<BoxShadow> {
    if first.is_empty() || first.eq_ignore_ascii_case("none") {
        return None;
    }
    let mut lengths: Vec<Length> = Vec::new();
    let mut color: Option<RgbaColor> = None;
    // Tokenise on whitespace, respecting `rgb(...)` and `rgba(...)` calls.
    let mut tokens: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut paren = 0i32;
    for ch in first.chars() {
        if ch == '(' {
            paren += 1;
            buf.push(ch);
            continue;
        }
        if ch == ')' {
            paren -= 1;
            buf.push(ch);
            continue;
        }
        if ch.is_ascii_whitespace() && paren == 0 {
            if !buf.is_empty() {
                tokens.push(std::mem::take(&mut buf));
            }
            continue;
        }
        buf.push(ch);
    }
    if !buf.is_empty() {
        tokens.push(buf);
    }
    for tok in tokens {
        if tok.eq_ignore_ascii_case("inset") {
            continue;
        }
        if let Some(l) = parse_length(&tok) {
            lengths.push(l);
            continue;
        }
        if let Some(c) = parse_color(&tok) {
            color = Some(c);
            continue;
        }
    }
    if lengths.len() < 2 {
        return None;
    }
    let offset_x = lengths[0];
    let offset_y = lengths[1];
    let blur = lengths.get(2).copied().unwrap_or(Length::Px(0.0));
    let spread = lengths.get(3).copied().unwrap_or(Length::Px(0.0));
    Some(BoxShadow {
        offset_x,
        offset_y,
        blur,
        spread,
        // CSS default: `currentColor`. We don't track currentColor at
        // parse time, so a missing colour falls back to opaque-ish
        // black at 50% — a reasonable default for "card" shadows.
        color: color.unwrap_or(RgbaColor {
            r: 0,
            g: 0,
            b: 0,
            a: 128,
        }),
    })
}

/// Parse a `top` / `right` / `bottom` / `left` offset. `auto` and any
/// unrecognised value yield `None` so the resolver knows the offset is
/// not specified (CSS treats `auto` as "use the static position").
fn parse_offset(v: &str) -> Option<Length> {
    if v.eq_ignore_ascii_case("auto") {
        return None;
    }
    parse_length(v)
}

/// Pull the static-string portion out of a CSS `content` value.
/// Supports plain quoted strings (`"x"` / `'x'`), the keywords
/// `none`/`normal`/`""` (all → `None`), and concatenated strings
/// (`"a" " " "b"` → `"a b"`). `attr()` / `counter()` / `url()`
/// silently yield `None` so the synthetic pseudo-element doesn't
/// emit gibberish.
fn parse_content_string(v: &str) -> Option<String> {
    let v = v.trim();
    if v.is_empty()
        || v.eq_ignore_ascii_case("none")
        || v.eq_ignore_ascii_case("normal")
    {
        return None;
    }
    let mut out = String::new();
    // Iterate by chars (not bytes) so escape sequences inside the
    // string are interpreted at codepoint granularity.
    let chars: Vec<char> = v.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '"' | '\'' => {
                let quote = chars[i];
                i += 1;
                while i < chars.len() && chars[i] != quote {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        // CSS unicode escape: backslash followed by 1–6
                        // hex digits, optionally followed by ONE
                        // whitespace (consumed). Required for things
                        // like `content: "\200B"` (zero-width space)
                        // that Wikipedia's editsection brackets emit
                        // for screen-reader announcement.
                        let mut j = i + 1;
                        let mut hex = String::new();
                        while j < chars.len()
                            && hex.len() < 6
                            && chars[j].is_ascii_hexdigit()
                        {
                            hex.push(chars[j]);
                            j += 1;
                        }
                        if !hex.is_empty() {
                            // Skip the optional single trailing whitespace.
                            if j < chars.len() && chars[j] == ' ' {
                                j += 1;
                            }
                            if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                                if let Some(ch) = char::from_u32(cp) {
                                    out.push(ch);
                                    i = j;
                                    continue;
                                }
                            }
                        }
                        // Non-hex escape (e.g. \" / \' / \\) — push the
                        // following char literally.
                        out.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    out.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() {
                    i += 1; // skip closing quote
                }
            }
            ' ' | '\t' => {
                i += 1;
            }
            _ => {
                // Unsupported function or token — bail out without
                // partial content.
                return None;
            }
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn parse_list_style_type_kw(v: &str) -> Option<ListStyleType> {
    Some(match v {
        "none" => ListStyleType::None,
        "disc" => ListStyleType::Disc,
        "circle" => ListStyleType::Circle,
        "square" => ListStyleType::Square,
        "decimal" => ListStyleType::Decimal,
        "decimal-leading-zero" => ListStyleType::DecimalLeadingZero,
        "lower-alpha" | "lower-latin" => ListStyleType::LowerAlpha,
        "upper-alpha" | "upper-latin" => ListStyleType::UpperAlpha,
        "lower-roman" => ListStyleType::LowerRoman,
        "upper-roman" => ListStyleType::UpperRoman,
        _ => return None,
    })
}

/// Parse a CSS `aspect-ratio` value: `<num>` or `<w> / <h>`. Returns
/// the W:H ratio as a single float (W / H). `auto` and unknown
/// values yield `None`.
fn parse_aspect_ratio(v: &str) -> Option<f32> {
    let v = v.trim();
    if v.eq_ignore_ascii_case("auto") {
        return None;
    }
    if let Some((w, h)) = v.split_once('/') {
        let w: f32 = w.trim().parse().ok()?;
        let h: f32 = h.trim().parse().ok()?;
        if h == 0.0 { return None; }
        return Some(w / h);
    }
    let n: f32 = v.parse().ok()?;
    if n > 0.0 { Some(n) } else { None }
}

fn parse_overflow_kw(v: &str) -> Option<Overflow> {
    Some(match v.to_ascii_lowercase().as_str() {
        "visible" => Overflow::Visible,
        "hidden" => Overflow::Hidden,
        "scroll" => Overflow::Scroll,
        "auto" => Overflow::Auto,
        "clip" => Overflow::Clip,
        _ => return None,
    })
}

/// Parse the first translate-family function out of a `transform`
/// value. Supports `translate(x[, y])`, `translateX(x)`, `translateY(y)`.
/// Other functions (scale / rotate / matrix) are silently ignored —
/// our compositor doesn't have a transform stack yet, so we'd render
/// them wrong if we partially applied them.
fn parse_transform_translate(v: &str) -> Option<(Length, Length)> {
    let lower = v.to_ascii_lowercase();
    if let Some(start) = lower.find("translate(") {
        let after = &v[start + "translate(".len()..];
        let close = find_paren_close(after.as_bytes())?;
        let inner = &after[..close];
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        let x = parse_length(parts[0])?;
        let y = if parts.len() > 1 { parse_length(parts[1])? } else { Length::Px(0.0) };
        return Some((x, y));
    }
    if let Some(start) = lower.find("translatex(") {
        let after = &v[start + "translatex(".len()..];
        let close = find_paren_close(after.as_bytes())?;
        let x = parse_length(after[..close].trim())?;
        return Some((x, Length::Px(0.0)));
    }
    if let Some(start) = lower.find("translatey(") {
        let after = &v[start + "translatey(".len()..];
        let close = find_paren_close(after.as_bytes())?;
        let y = parse_length(after[..close].trim())?;
        return Some((Length::Px(0.0), y));
    }
    None
}

fn find_paren_close(bytes: &[u8]) -> Option<usize> {
    let mut depth = 1i32;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// `outline: <width> <style> <color>` — same shorthand grammar as
/// `border`. `outline-style` we don't track yet (everything is rendered
/// as a solid line); `none` clears the width.
fn apply_outline_shorthand(v: &str, cv: &mut ComputedValues) {
    let trimmed = v.trim();
    if trimmed.eq_ignore_ascii_case("none") || trimmed.eq_ignore_ascii_case("0") {
        cv.outline_width = None;
        return;
    }
    let mut found_width = false;
    for tok in trimmed.split_ascii_whitespace() {
        // Skip outline-style keywords.
        if matches!(
            tok.to_ascii_lowercase().as_str(),
            "none" | "hidden" | "dotted" | "dashed" | "solid" | "double" | "groove"
                | "ridge" | "inset" | "outset" | "auto"
        ) {
            continue;
        }
        if !found_width {
            if let Some(l) = parse_length(tok) {
                cv.outline_width = Some(l);
                found_width = true;
                continue;
            }
            // Width keywords.
            match tok.to_ascii_lowercase().as_str() {
                "thin" => {
                    cv.outline_width = Some(Length::Px(1.0));
                    found_width = true;
                    continue;
                }
                "medium" => {
                    cv.outline_width = Some(Length::Px(3.0));
                    found_width = true;
                    continue;
                }
                "thick" => {
                    cv.outline_width = Some(Length::Px(5.0));
                    found_width = true;
                    continue;
                }
                _ => {}
            }
        }
        if let Some(c) = parse_color(tok) {
            cv.outline_color = c;
        }
    }
}

/// CSS `font` shorthand. Per the spec the order is
///   `[font-style] [font-variant] [font-weight] [font-stretch] font-size[/line-height] font-family`.
/// We accept the common subset: optional style/weight tokens (case-
/// insensitive) followed by a size / size+line-height token, then the
/// rest of the value as the family list.
fn apply_font_shorthand(cv: &mut ComputedValues, v: &str) {
    let trimmed = v.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("inherit") {
        return;
    }
    // Split on whitespace at the top level (don't break inside quoted
    // family names or `rgb(...)`).
    let mut tokens: Vec<&str> = Vec::new();
    let bytes = trimmed.as_bytes();
    let mut start = 0usize;
    let mut paren = 0i32;
    let mut in_quote = 0u8;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'"' | b'\'' if in_quote == 0 => in_quote = b,
            b if b == in_quote => in_quote = 0,
            b'(' if in_quote == 0 => paren += 1,
            b')' if in_quote == 0 => paren -= 1,
            _ => {}
        }
        if (b == b' ' || b == b'\t') && paren == 0 && in_quote == 0 {
            if start < i {
                tokens.push(&trimmed[start..i]);
            }
            start = i + 1;
        }
        i += 1;
    }
    if start < trimmed.len() {
        tokens.push(&trimmed[start..]);
    }

    let mut idx = 0usize;
    let mut size_token: Option<&str> = None;
    let mut size_idx: Option<usize> = None;
    while idx < tokens.len() {
        let t = tokens[idx];
        let lc = t.to_ascii_lowercase();
        match lc.as_str() {
            "italic" | "oblique" => cv.font_style = FontStyle::Italic,
            "normal" => {
                // Resets style/weight when it appears here. Cheap heuristic.
            }
            "bold" | "bolder" => cv.font_weight = FontWeight::Bold,
            "lighter" => cv.font_weight = FontWeight::Normal,
            // Variant / stretch tokens we ignore.
            "small-caps" | "ultra-condensed" | "extra-condensed" | "condensed"
            | "semi-condensed" | "semi-expanded" | "expanded" | "extra-expanded"
            | "ultra-expanded" => {}
            _ => {
                // Numeric weight (100..900) before the size token.
                if let Ok(n) = lc.parse::<u32>() {
                    if size_token.is_none() && (100..=900).contains(&n) {
                        cv.font_weight = if n >= 600 {
                            FontWeight::Bold
                        } else {
                            FontWeight::Normal
                        };
                        idx += 1;
                        continue;
                    }
                }
                // Size — possibly with `/line-height`.
                if size_token.is_none() && looks_like_font_size(t) {
                    size_token = Some(t);
                    size_idx = Some(idx);
                    idx += 1;
                    continue;
                }
            }
        }
        idx += 1;
    }
    let Some(size_token) = size_token else { return };
    let size_idx = size_idx.unwrap();
    if let Some((sz, lh)) = size_token.split_once('/') {
        if let Some(px) = parse_font_size(sz, cv.font_size) {
            cv.font_size = px;
        }
        if let Some(lh_val) = parse_line_height(lh, cv.font_size) {
            cv.line_height = lh_val;
        }
    } else if let Some(px) = parse_font_size(size_token, cv.font_size) {
        cv.font_size = px;
    }
    // Everything after the size token is the family list. Take the
    // first family (matches our existing handler's behaviour).
    if size_idx + 1 < tokens.len() {
        let family = tokens[size_idx + 1..].join(" ");
        cv.font_family = family
            .split(',')
            .next()
            .unwrap_or(&family)
            .trim()
            .trim_matches(['"', '\''])
            .to_string();
    }
}

fn looks_like_font_size(t: &str) -> bool {
    let lc = t.to_ascii_lowercase();
    matches!(
        lc.as_str(),
        "xx-small"
            | "x-small"
            | "small"
            | "medium"
            | "large"
            | "x-large"
            | "xx-large"
            | "smaller"
            | "larger"
    ) || lc.contains("px")
        || lc.contains("em")
        || lc.contains("rem")
        || lc.contains('%')
        || lc.split('/').next().is_some_and(|head| {
            head.ends_with("px")
                || head.ends_with("em")
                || head.ends_with("rem")
                || head.ends_with('%')
        })
}

fn apply_flex_shorthand(cv: &mut ComputedValues, v: &str) {
    // CSS `flex` shorthand. Common forms:
    //   flex: none                 => 0 0 auto
    //   flex: auto                 => 1 1 auto
    //   flex: 1                    => 1 1 0
    //   flex: 2 3                  => 2 3 0
    //   flex: 1 1 100px            => 1 1 100px
    //   flex: 0 0 100px            => 0 0 100px
    let trimmed = v.trim();
    if trimmed.is_empty() {
        return;
    }
    if trimmed.eq_ignore_ascii_case("none") {
        cv.flex_grow = 0.0;
        cv.flex_shrink = 0.0;
        cv.flex_basis = FlexBasis::Auto;
        return;
    }
    if trimmed.eq_ignore_ascii_case("auto") {
        cv.flex_grow = 1.0;
        cv.flex_shrink = 1.0;
        cv.flex_basis = FlexBasis::Auto;
        return;
    }
    let parts: Vec<&str> = trimmed.split_ascii_whitespace().collect();
    let mut grow: Option<f32> = None;
    let mut shrink: Option<f32> = None;
    let mut basis: Option<FlexBasis> = None;
    for p in &parts {
        if p.eq_ignore_ascii_case("auto") {
            basis = Some(FlexBasis::Auto);
            continue;
        }
        // Per CSS Flex L1, a unit-less numeric token is interpreted as
        // flex-grow / flex-shrink before falling back to a length. So
        // try `parse::<f32>` *first* — that treats "0" as grow=0, not as
        // basis=0px.
        if let Ok(n) = p.parse::<f32>() {
            if grow.is_none() {
                grow = Some(n.max(0.0));
            } else if shrink.is_none() {
                shrink = Some(n.max(0.0));
            }
            continue;
        }
        if let Some(l) = parse_length(p) {
            basis = Some(FlexBasis::Length(l));
            continue;
        }
    }
    cv.flex_grow = grow.unwrap_or(1.0);
    cv.flex_shrink = shrink.unwrap_or(1.0);
    cv.flex_basis = basis.unwrap_or(FlexBasis::Length(Length::Px(0.0)));
}

/// Copy a single property field from `source` to `cv` based on the
/// CSS property name. Used by `inherit` / `initial` / `unset` /
/// `revert` keyword handling.
fn copy_field_by_name(cv: &mut ComputedValues, source: &ComputedValues, name: &str) {
    match name {
        "color" => cv.color = source.color,
        "background-color" => cv.background_color = source.background_color,
        "background" | "background-image" => {
            cv.background_color = source.background_color;
            cv.background_image = source.background_image.clone();
        }
        "font" => {
            cv.font_family = source.font_family.clone();
            cv.font_size = source.font_size;
            cv.font_weight = source.font_weight;
            cv.font_style = source.font_style;
            cv.line_height = source.line_height;
        }
        "font-family" => cv.font_family = source.font_family.clone(),
        "font-size" => cv.font_size = source.font_size,
        "font-weight" => cv.font_weight = source.font_weight,
        "font-style" => cv.font_style = source.font_style,
        "line-height" => cv.line_height = source.line_height,
        "text-align" => cv.text_align = source.text_align,
        "text-transform" => cv.text_transform = source.text_transform,
        "letter-spacing" => cv.letter_spacing = source.letter_spacing,
        "white-space" => cv.white_space = source.white_space,
        "visibility" => cv.visibility = source.visibility,
        "cursor" => cv.cursor = source.cursor,
        "list-style-type" => cv.list_style_type = source.list_style_type,
        "direction" | "unicode-bidi" => {} // no-op until rtl ships
        "border" | "border-color" => cv.border_color = source.border_color,
        "border-width" => cv.border = source.border,
        "margin" => cv.margin = source.margin,
        "padding" => cv.padding = source.padding,
        "width" => cv.width = source.width,
        "height" => cv.height = source.height,
        "display" => cv.display = source.display,
        "opacity" => cv.opacity = source.opacity,
        "text-decoration" | "text-decoration-line" => {
            cv.text_decoration_underline = source.text_decoration_underline;
            cv.text_decoration_line_through = source.text_decoration_line_through;
        }
        // Properties not covered fall through silently — matches the
        // existing parser-coverage profile.
        _ => {}
    }
}

fn parse_background_size(v: &str) -> Option<BackgroundSize> {
    let lc = v.trim().to_ascii_lowercase();
    if lc == "auto" {
        return Some(BackgroundSize::Auto);
    }
    if lc == "cover" {
        return Some(BackgroundSize::Cover);
    }
    if lc == "contain" {
        return Some(BackgroundSize::Contain);
    }
    let parts: Vec<&str> = v.split_ascii_whitespace().collect();
    let parse_axis = |t: &str| -> Length {
        if t.eq_ignore_ascii_case("auto") {
            Length::Px(0.0)
        } else {
            parse_length(t).unwrap_or(Length::Px(0.0))
        }
    };
    match parts.len() {
        1 => Some(BackgroundSize::Length(parse_axis(parts[0]), Length::Px(0.0))),
        2 => Some(BackgroundSize::Length(parse_axis(parts[0]), parse_axis(parts[1]))),
        _ => None,
    }
}

fn parse_background_position(v: &str) -> Option<BackgroundPosition> {
    let parts: Vec<&str> = v.split_ascii_whitespace().collect();
    let axis_x = |t: &str| -> BackgroundAxisPos {
        match t.to_ascii_lowercase().as_str() {
            "left" => BackgroundAxisPos::Anchor(0.0),
            "right" => BackgroundAxisPos::Anchor(1.0),
            "center" => BackgroundAxisPos::Anchor(0.5),
            other => match parse_length(other) {
                Some(Length::Percent(p)) => BackgroundAxisPos::Anchor(p / 100.0),
                Some(l) => BackgroundAxisPos::Length(l),
                None => BackgroundAxisPos::Anchor(0.0),
            },
        }
    };
    let axis_y = |t: &str| -> BackgroundAxisPos {
        match t.to_ascii_lowercase().as_str() {
            "top" => BackgroundAxisPos::Anchor(0.0),
            "bottom" => BackgroundAxisPos::Anchor(1.0),
            "center" => BackgroundAxisPos::Anchor(0.5),
            other => match parse_length(other) {
                Some(Length::Percent(p)) => BackgroundAxisPos::Anchor(p / 100.0),
                Some(l) => BackgroundAxisPos::Length(l),
                None => BackgroundAxisPos::Anchor(0.0),
            },
        }
    };
    match parts.len() {
        1 => Some(BackgroundPosition {
            x: axis_x(parts[0]),
            y: BackgroundAxisPos::Anchor(0.5),
        }),
        2 | 3 | 4 => Some(BackgroundPosition {
            x: axis_x(parts[0]),
            y: axis_y(parts[1]),
        }),
        _ => None,
    }
}

/// 1- or 2-value axis shorthand for the logical-properties pairs
/// (e.g. `margin-block: 1em` sets both top and bottom; `margin-inline:
/// 1em 2em` sets left then right). Out-of-range token counts are a
/// no-op.
fn apply_axis_shorthand(v: &str, start: &mut Length, end: &mut Length) {
    let parts: Vec<&str> = v.split_ascii_whitespace().collect();
    match parts.len() {
        1 => {
            set_edge(start, parts[0]);
            set_edge(end, parts[0]);
        }
        2 => {
            set_edge(start, parts[0]);
            set_edge(end, parts[1]);
        }
        _ => {}
    }
}

fn set_edge(slot: &mut Length, v: &str) {
    if let Some(l) = parse_length(v) {
        *slot = l;
    }
}

/// Margin-specific shorthand. Same 1/2/3/4-token CSS rules as
/// `apply_edge_shorthand`, but each token may be `auto`. Auto sides
/// get `Length::Px(0)` plus a flag in the parent ComputedValues so
/// layout can spend leftover horizontal space on them.
fn apply_margin_shorthand(v: &str, cv: &mut ComputedValues) {
    fn parse_part(tok: &str) -> Option<(Length, bool)> {
        if tok.eq_ignore_ascii_case("auto") {
            return Some((Length::Px(0.0), true));
        }
        parse_length(tok).map(|l| (l, false))
    }
    let parts: Vec<(Length, bool)> = v
        .split_ascii_whitespace()
        .filter_map(parse_part)
        .collect();
    let (top, right, bottom, left): (
        (Length, bool),
        (Length, bool),
        (Length, bool),
        (Length, bool),
    ) = match parts.len() {
        1 => (parts[0], parts[0], parts[0], parts[0]),
        2 => (parts[0], parts[1], parts[0], parts[1]),
        3 => (parts[0], parts[1], parts[2], parts[1]),
        4 => (parts[0], parts[1], parts[2], parts[3]),
        _ => return,
    };
    cv.margin.top = top.0;
    cv.margin.right = right.0;
    cv.margin.bottom = bottom.0;
    cv.margin.left = left.0;
    cv.margin_top_auto = top.1;
    cv.margin_right_auto = right.1;
    cv.margin_bottom_auto = bottom.1;
    cv.margin_left_auto = left.1;
}

fn apply_edge_shorthand(v: &str, edges: &mut EdgeSizes) {
    let parts: Vec<Length> = v
        .split_ascii_whitespace()
        .filter_map(parse_length)
        .collect();
    match parts.len() {
        1 => *edges = EdgeSizes::uniform(parts[0]),
        2 => {
            edges.top = parts[0];
            edges.bottom = parts[0];
            edges.left = parts[1];
            edges.right = parts[1];
        }
        3 => {
            edges.top = parts[0];
            edges.left = parts[1];
            edges.right = parts[1];
            edges.bottom = parts[2];
        }
        4 => {
            edges.top = parts[0];
            edges.right = parts[1];
            edges.bottom = parts[2];
            edges.left = parts[3];
        }
        _ => {}
    }
}

/// Parse the `grid-template-columns` / `grid-template-rows` track
/// list. Supports `<length>`, `<percentage>`, `<fr>`, `auto`,
/// `min-content` / `max-content` (collapsed to `auto`),
/// `minmax(min, max)`, `repeat(<n>, <track-list>)`, and named-line
/// tokens like `[col-start]`. Returns parallel `(tracks, line_names)`
/// vecs where `line_names[i]` collects every name declared at the
/// line *before* track `i` (so `line_names.len() == tracks.len() + 1`).
fn parse_track_template(v: &str) -> (Vec<TrackSize>, Vec<Vec<String>>) {
    let mut tracks: Vec<TrackSize> = Vec::new();
    let mut lines: Vec<Vec<String>> = vec![Vec::new()];
    for tok in tokenize_top_level(v) {
        if let Some(inner) = tok.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            // `[a b c]` declares multiple names at the current line.
            let slot = lines.last_mut().unwrap();
            for name in inner.split_ascii_whitespace() {
                slot.push(name.to_string());
            }
            continue;
        }
        if let Some(rest) = tok.strip_prefix("repeat(").and_then(|s| s.strip_suffix(')')) {
            let (count_str, body) = match rest.split_once(',') {
                Some(p) => p,
                None => continue,
            };
            let count_str = count_str.trim();
            let count: usize = if count_str.eq_ignore_ascii_case("auto-fit")
                || count_str.eq_ignore_ascii_case("auto-fill")
            {
                1
            } else {
                count_str.parse().unwrap_or(1)
            };
            let (inner_tracks, inner_lines) = parse_track_template(body.trim());
            for _ in 0..count {
                // Splice each repeat: line names from inner_lines[0]
                // merge into the current line slot, then alternate
                // tracks + new line slots.
                if let Some(first) = inner_lines.first() {
                    lines.last_mut().unwrap().extend(first.iter().cloned());
                }
                for (i, t) in inner_tracks.iter().copied().enumerate() {
                    tracks.push(t);
                    lines.push(inner_lines.get(i + 1).cloned().unwrap_or_default());
                }
            }
            continue;
        }
        if let Some(t) = parse_track_size(tok) {
            tracks.push(t);
            lines.push(Vec::new());
        }
    }
    (tracks, lines)
}

fn parse_track_size(tok: &str) -> Option<TrackSize> {
    let tok = tok.trim();
    if tok.eq_ignore_ascii_case("auto")
        || tok.eq_ignore_ascii_case("min-content")
        || tok.eq_ignore_ascii_case("max-content")
    {
        return Some(TrackSize::Auto);
    }
    if let Some(num) = tok.strip_suffix("fr") {
        if let Ok(n) = num.trim().parse::<f32>() {
            return Some(TrackSize::Fr(n));
        }
    }
    if let Some(rest) = tok.strip_prefix("minmax(").and_then(|s| s.strip_suffix(')')) {
        let (a, b) = rest.split_once(',')?;
        let a = parse_minmax_side(a.trim())?;
        let b = parse_minmax_side(b.trim())?;
        return Some(TrackSize::MinMax(a, b));
    }
    if let Some(rest) = tok.strip_prefix("fit-content(").and_then(|s| s.strip_suffix(')')) {
        // `fit-content(<size>)` ≈ `minmax(auto, <size>)`. Good enough
        // for the cases that show up in real stylesheets.
        let upper = parse_minmax_side(rest.trim())?;
        return Some(TrackSize::MinMax(MinMaxSide::Auto, upper));
    }
    parse_length(tok).map(TrackSize::Length)
}

fn parse_minmax_side(tok: &str) -> Option<MinMaxSide> {
    let tok = tok.trim();
    if tok.eq_ignore_ascii_case("auto")
        || tok.eq_ignore_ascii_case("min-content")
        || tok.eq_ignore_ascii_case("max-content")
    {
        return Some(MinMaxSide::Auto);
    }
    if let Some(num) = tok.strip_suffix("fr") {
        if let Ok(n) = num.trim().parse::<f32>() {
            return Some(MinMaxSide::Fr(n));
        }
    }
    parse_length(tok).map(MinMaxSide::Length)
}

/// Parse a `grid-template-areas` declaration of the form
/// `"a b" "c d"` or `"a a a" "b c d"`. Each quoted string defines
/// one row; whitespace-separated tokens inside it define the row's
/// columns. Returns an empty Vec for `none` or a malformed input;
/// rows of mismatched column counts are dropped.
fn parse_template_areas(v: &str) -> Vec<Vec<String>> {
    if v.trim().eq_ignore_ascii_case("none") {
        return Vec::new();
    }
    let bytes = v.as_bytes();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        // Find next quote.
        while i < bytes.len() && bytes[i] != b'"' && bytes[i] != b'\'' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let q = bytes[i];
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i] != q {
            i += 1;
        }
        let inner = &v[start..i];
        i += 1;
        let cols: Vec<String> = inner
            .split_ascii_whitespace()
            .map(|s| s.to_string())
            .collect();
        if !cols.is_empty() {
            rows.push(cols);
        }
    }
    if rows.is_empty() {
        return rows;
    }
    let cols = rows[0].len();
    rows.retain(|r| r.len() == cols);
    rows
}

/// Parse a `grid-column` / `grid-row` shorthand of the form
/// `<line>` or `<line> / <line>`. Auto on either side stays auto.
fn parse_grid_axis_shorthand(v: &str) -> (GridLine, GridLine) {
    if let Some((a, b)) = v.split_once('/') {
        (parse_grid_line(a.trim()), parse_grid_line(b.trim()))
    } else {
        (parse_grid_line(v.trim()), GridLine::Auto)
    }
}

fn parse_grid_line(v: &str) -> GridLine {
    let v = v.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("auto") {
        return GridLine::Auto;
    }
    if let Some(rest) = v.strip_prefix("span ").or_else(|| v.strip_prefix("span\t")) {
        if let Ok(n) = rest.trim().parse::<u32>() {
            if n >= 1 {
                return GridLine::Span(n);
            }
        }
        return GridLine::Auto;
    }
    if let Ok(n) = v.parse::<i32>() {
        if n != 0 {
            return GridLine::Line(n);
        }
    }
    // Treat any other identifier as a named line — we resolve at
    // placement time against the parent grid's line-name table. Bare
    // identifiers in CSS Grid syntax are line names.
    if v.chars().next().map(|c| c.is_ascii_alphabetic() || c == '-' || c == '_').unwrap_or(false) {
        return GridLine::Named(v.to_string());
    }
    GridLine::Auto
}

fn parse_display(v: &str) -> Option<Display> {
    Some(match v.to_ascii_lowercase().as_str() {
        "block" => Display::Block,
        "inline" => Display::Inline,
        "inline-block" => Display::InlineBlock,
        // Legacy `-webkit-box` and `-webkit-flex` are vendor-prefixed
        // earlier-spec equivalents of `flex`; many modern stylesheets
        // still ship a 3-line cascade for older Safari:
        //   display: -webkit-box;
        //   display: -webkit-flex;
        //   display: flex;
        // Returning None for the first two lines left the cv at the
        // initial `inline` / parent value and (depending on cascade
        // order vs. other display rules) sometimes ended up on
        // Grid because a previous declaration won. Treat them as
        // their unprefixed flex equivalents.
        "flex" | "inline-flex" => Display::Flex,
        "-webkit-box" | "-webkit-inline-box" => Display::Flex,
        "-webkit-flex" | "-webkit-inline-flex" => Display::Flex,
        "-moz-box" | "-moz-inline-box" => Display::Flex,
        "-ms-flexbox" | "-ms-inline-flexbox" => Display::Flex,
        "grid" | "inline-grid" => Display::Grid,
        "-ms-grid" | "-ms-inline-grid" => Display::Grid,
        "none" => Display::None,
        "list-item" => Display::ListItem,
        "table" => Display::Table,
        "table-row" => Display::TableRow,
        "table-cell" => Display::TableCell,
        "contents" => Display::Contents,
        _ => return None,
    })
}

/// Parse a length-or-keyword for `min-*` / `max-*` properties. CSS
/// uses `none` for the max-* "no constraint" case and `auto` for the
/// min-* default (= zero); we collapse both to `None`.
fn parse_optional_length(v: &str) -> Option<Length> {
    let v = v.trim();
    if v.eq_ignore_ascii_case("none") || v.eq_ignore_ascii_case("auto") {
        return None;
    }
    parse_length(v)
}

fn parse_dimension(v: &str) -> Option<Dimension> {
    if v.eq_ignore_ascii_case("auto") {
        return Some(Dimension::Auto);
    }
    // Intrinsic-sizing keywords. We don't have content-based
    // measurement, so collapse all three to `auto` (use the
    // container's available width). Better than dropping the
    // declaration entirely — Wikipedia's article-table rule
    // `.mw-parser-output table { width: fit-content }` would
    // otherwise leave the table at its previous (often-wider)
    // value. Auto here pairs with the existing min-/max-width
    // clamps to give a workable approximation.
    if v.eq_ignore_ascii_case("fit-content")
        || v.eq_ignore_ascii_case("max-content")
        || v.eq_ignore_ascii_case("min-content")
    {
        return Some(Dimension::Auto);
    }
    parse_length(v).map(Dimension::Length)
}

fn parse_length(v: &str) -> Option<Length> {
    let v = v.trim();
    if v == "0" {
        return Some(Length::Px(0.0));
    }
    if let Some(rest) = v.strip_prefix("calc(").and_then(|s| s.strip_suffix(')')) {
        return parse_calc(rest).map(Length::Calc);
    }
    if let Some(rest) = v.strip_prefix("min(").and_then(|s| s.strip_suffix(')')) {
        return parse_bounded_args(rest).map(|items| Length::Min(BoundedList::from_iter(items)));
    }
    if let Some(rest) = v.strip_prefix("max(").and_then(|s| s.strip_suffix(')')) {
        return parse_bounded_args(rest).map(|items| Length::Max(BoundedList::from_iter(items)));
    }
    if let Some(rest) = v.strip_prefix("clamp(").and_then(|s| s.strip_suffix(')')) {
        let parts = parse_bounded_args(rest)?;
        if parts.len() != 3 {
            return None;
        }
        return Some(Length::Clamp(parts[0], parts[1], parts[2]));
    }
    // Print + absolute units mapped onto Px via standard
    // conversions: 1pt = 1.333..px (96dpi), 1pc = 12pt = 16px,
    // 1in = 96px, 1cm = 37.795..px, 1mm = 3.78..px.
    let absolute_px: &[(&str, f32)] = &[
        ("pt", 96.0 / 72.0),
        ("pc", 16.0),
        ("in", 96.0),
        ("cm", 37.795_277),
        ("mm", 3.779_527_6),
        ("Q", 0.944_881_9),
    ];
    for (suffix, mul) in absolute_px {
        if let Some(num) = v.strip_suffix(*suffix) {
            if let Ok(n) = num.trim().parse::<f32>() {
                return Some(Length::Px(n * mul));
            }
        }
    }
    // Order matters: longer suffixes first so "rem" doesn't match
    // ahead of "em", "vmin"/"vmax" don't match ahead of "vh"/"vw".
    let units = [
        ("vmin", Length::Vmin as fn(f32) -> Length),
        ("vmax", Length::Vmax as fn(f32) -> Length),
        ("rem", Length::Rem as fn(f32) -> Length),
        ("vw", Length::Vw as fn(f32) -> Length),
        ("vh", Length::Vh as fn(f32) -> Length),
        ("ch", Length::Em as fn(f32) -> Length), // 1ch ~ width of "0"; approximate as em
        ("ex", Length::Em as fn(f32) -> Length), // 1ex ~ x-height; approximate as em
        ("px", Length::Px as fn(f32) -> Length),
        ("em", Length::Em as fn(f32) -> Length),
        ("%", Length::Percent as fn(f32) -> Length),
    ];
    for (suffix, ctor) in units {
        if let Some(num) = v.strip_suffix(suffix) {
            return num.trim().parse().ok().map(ctor);
        }
    }
    // Bare number → treat as px (used by CSS shortcuts in some contexts).
    None
}

/// Parse a `calc(...)` body into a `CalcSum`. Supports `+`, `-`, `*`,
/// `/` with the CSS-spec whitespace rules: `+`/`-` must have whitespace
/// on both sides (so `100%-30px` is invalid, `100% - 30px` is OK), and
/// `*`/`/` may abut their operands. We tokenise into `+`/`-`-separated
/// terms first, then evaluate each term left-to-right using `*`/`/`.
/// Multiplication / division are only valid when one side is a unitless
/// number; calc-of-calc is allowed and folded recursively.
fn parse_calc(body: &str) -> Option<CalcSum> {
    // Split into +/- separated terms, respecting parens. Each term
    // accumulates as a `(sign, str)` pair.
    let bytes = body.as_bytes();
    let mut terms: Vec<(f32, String)> = Vec::new();
    let mut sign: f32 = 1.0;
    let mut start = 0usize;
    let mut paren = 0i32;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'(' {
            paren += 1;
        } else if b == b')' {
            paren -= 1;
        }
        if paren == 0 && i + 2 < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            // Look for ` + ` or ` - ` patterns.
            let op = bytes[i + 1];
            if (op == b'+' || op == b'-') && (bytes[i + 2] == b' ' || bytes[i + 2] == b'\t') {
                let term = body[start..i].trim().to_string();
                if !term.is_empty() {
                    terms.push((sign, term));
                }
                sign = if op == b'+' { 1.0 } else { -1.0 };
                i += 3;
                start = i;
                continue;
            }
        }
        i += 1;
    }
    let term = body[start..].trim().to_string();
    if !term.is_empty() {
        terms.push((sign, term));
    }
    if terms.is_empty() {
        return None;
    }

    let mut acc = CalcSum::default();
    for (s, t) in terms {
        let v = parse_calc_term(&t)?;
        acc = acc.add(v, s);
    }
    Some(acc)
}

/// Evaluate a `*` / `/` chain like `2 * 30px` or `100% / 2`. One side
/// of each operator must be a bare number; the unit-bearing operand
/// defines the unit of the product.
fn parse_calc_term(term: &str) -> Option<CalcSum> {
    // Split on top-level `*` and `/`, respecting parens.
    let bytes = term.as_bytes();
    let mut pieces: Vec<(char, String)> = Vec::new();
    let mut start = 0usize;
    let mut paren = 0i32;
    let mut last_op = '*';
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'(' {
            paren += 1;
        } else if b == b')' {
            paren -= 1;
        }
        if paren == 0 && (b == b'*' || b == b'/') {
            let piece = term[start..i].trim().to_string();
            pieces.push((last_op, piece));
            last_op = b as char;
            start = i + 1;
        }
    }
    pieces.push((last_op, term[start..].trim().to_string()));

    let mut acc: Option<CalcSum> = None;
    for (op, piece) in pieces {
        // Try to parse as a bare number first (for the * scalar case).
        let as_number = piece.parse::<f32>().ok();
        let as_length = parse_length_no_calc(&piece)
            .or_else(|| {
                // Nested calc(...) is allowed inside terms.
                piece
                    .strip_prefix("calc(")
                    .and_then(|s| s.strip_suffix(')'))
                    .and_then(parse_calc)
                    .map(Length::Calc)
            })
            .and_then(|l| l.to_calc_sum());
        match (acc, as_number, as_length) {
            (None, _, Some(sum)) => acc = Some(sum),
            (None, Some(n), None) => acc = Some(CalcSum { px: n, ..Default::default() }),
            (Some(a), Some(n), _) if op == '*' => acc = Some(a.scale(n)),
            (Some(a), Some(n), _) if op == '/' && n != 0.0 => {
                acc = Some(a.scale(1.0 / n))
            }
            (Some(_), None, Some(b)) if op == '*' => {
                // `<length> * <length>` isn't valid CSS; bail.
                let _ = b;
                return None;
            }
            _ => return None,
        }
    }
    acc
}

/// Split a comma-separated function argument list at top-level
/// commas, then parse each piece as a length and reduce to its
/// CalcSum form. Used by `min()` / `max()` / `clamp()`.
fn parse_bounded_args(body: &str) -> Option<Vec<CalcSum>> {
    let bytes = body.as_bytes();
    let mut pieces: Vec<String> = Vec::new();
    let mut start = 0usize;
    let mut paren = 0i32;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => paren += 1,
            b')' => paren -= 1,
            b',' if paren == 0 => {
                pieces.push(body[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    pieces.push(body[start..].trim().to_string());

    let mut out = Vec::with_capacity(pieces.len());
    for p in pieces {
        let len = parse_length(&p)?;
        out.push(len.to_calc_sum()?);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Same shape as `parse_length` but without the calc() recursion —
/// used inside calc() itself to avoid infinite descent on a malformed
/// `calc(calc(...))` body.
fn parse_length_no_calc(v: &str) -> Option<Length> {
    let v = v.trim();
    if v == "0" {
        return Some(Length::Px(0.0));
    }
    let absolute_px: &[(&str, f32)] = &[
        ("pt", 96.0 / 72.0),
        ("pc", 16.0),
        ("in", 96.0),
        ("cm", 37.795_277),
        ("mm", 3.779_527_6),
        ("Q", 0.944_881_9),
    ];
    for (suffix, mul) in absolute_px {
        if let Some(num) = v.strip_suffix(*suffix) {
            if let Ok(n) = num.trim().parse::<f32>() {
                return Some(Length::Px(n * mul));
            }
        }
    }
    let units = [
        ("vmin", Length::Vmin as fn(f32) -> Length),
        ("vmax", Length::Vmax as fn(f32) -> Length),
        ("rem", Length::Rem as fn(f32) -> Length),
        ("vw", Length::Vw as fn(f32) -> Length),
        ("vh", Length::Vh as fn(f32) -> Length),
        ("ch", Length::Em as fn(f32) -> Length),
        ("ex", Length::Em as fn(f32) -> Length),
        ("px", Length::Px as fn(f32) -> Length),
        ("em", Length::Em as fn(f32) -> Length),
        ("%", Length::Percent as fn(f32) -> Length),
    ];
    for (suffix, ctor) in units {
        if let Some(num) = v.strip_suffix(suffix) {
            return num.trim().parse().ok().map(ctor);
        }
    }
    None
}

fn parse_font_size(v: &str, current: f32) -> Option<f32> {
    if let Some(l) = parse_length(v) {
        return Some(l.resolve(current, 16.0, current));
    }
    Some(match v.to_ascii_lowercase().as_str() {
        "xx-small" => 9.0,
        "x-small" => 10.0,
        "small" => 13.0,
        "medium" => 16.0,
        "large" => 18.0,
        "x-large" => 24.0,
        "xx-large" => 32.0,
        "smaller" => current * 0.83,
        "larger" => current * 1.2,
        _ => return None,
    })
}

fn parse_line_height(v: &str, font_size: f32) -> Option<f32> {
    if v.eq_ignore_ascii_case("normal") {
        return Some(1.2);
    }
    if let Ok(n) = v.parse::<f32>() {
        return Some(n);
    }
    if let Some(l) = parse_length(v) {
        let px = l.resolve(font_size, 16.0, font_size);
        return Some(px / font_size);
    }
    None
}

/// Pull the first parseable color out of a `linear-gradient(...)` /
/// `radial-gradient(...)` / `conic-gradient(...)` value (vendor-
/// prefixed variants too). Used as a degraded-render fallback so a
/// box declared with a gradient gets a solid color background
/// instead of nothing — the visual is wrong but the box at least
/// distinguishes itself from its parent. Real gradient painting
/// is a separate effort; this is the pragmatic stopgap.
fn parse_first_gradient_stop(v: &str) -> Option<RgbaColor> {
    let v = v.trim();
    let lower = v.to_ascii_lowercase();
    let prefixes = [
        "linear-gradient(",
        "radial-gradient(",
        "conic-gradient(",
        "repeating-linear-gradient(",
        "repeating-radial-gradient(",
        "repeating-conic-gradient(",
        "-webkit-linear-gradient(",
        "-webkit-radial-gradient(",
        "-moz-linear-gradient(",
        "-moz-radial-gradient(",
    ];
    let prefix = prefixes.iter().find(|p| lower.starts_with(*p))?;
    let body_start = prefix.len();
    // Walk to the matching ')'; track nested parens for `rgb(...)`
    // / `rgba(...)` color stops.
    let bytes = v.as_bytes();
    let mut depth = 1i32;
    let mut end = body_start;
    while end < bytes.len() {
        match bytes[end] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        end += 1;
    }
    let body = &v[body_start..end];
    // Split top-level commas and try each fragment as a color.
    // The first comma-fragment may be a direction (`90deg`, `to
    // bottom`, `top` legacy syntax) — skip if it doesn't parse as
    // a color and try the next.
    let mut depth = 0i32;
    let mut start = 0usize;
    let body_bytes = body.as_bytes();
    for i in 0..=body_bytes.len() {
        let at_end = i == body_bytes.len();
        let split = at_end || (depth == 0 && body_bytes[i] == b',');
        if split {
            let frag = body[start..i].trim();
            // A color stop may have a trailing percentage / length
            // (e.g. `red 20%`); strip the last whitespace-separated
            // token if it doesn't parse.
            if let Some(c) = parse_color(frag) {
                return Some(c);
            }
            // Try without the trailing position token.
            if let Some((color_part, _)) = frag.rsplit_once(char::is_whitespace) {
                if let Some(c) = parse_color(color_part.trim()) {
                    return Some(c);
                }
            }
            start = i + 1;
        } else if !at_end {
            match body_bytes[i] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
        }
    }
    None
}

fn parse_color(v: &str) -> Option<RgbaColor> {
    let v = v.trim();
    if let Some(rest) = v.strip_prefix('#') {
        return parse_hex(rest);
    }
    let lower = v.to_ascii_lowercase();
    if let Some(stripped) = lower
        .strip_prefix("rgb(")
        .and_then(|s| s.strip_suffix(')'))
    {
        return parse_rgb_args(stripped, false);
    }
    if let Some(stripped) = lower
        .strip_prefix("rgba(")
        .and_then(|s| s.strip_suffix(')'))
    {
        return parse_rgb_args(stripped, true);
    }
    if let Some(stripped) = lower
        .strip_prefix("hsl(")
        .and_then(|s| s.strip_suffix(')'))
    {
        return parse_hsl_args(stripped, false);
    }
    if let Some(stripped) = lower
        .strip_prefix("hsla(")
        .and_then(|s| s.strip_suffix(')'))
    {
        return parse_hsl_args(stripped, true);
    }
    parse_named_color(v)
}

/// Parse `hsl(H, S%, L%)` and `hsla(H, S%, L%, A)`. Hue is in degrees
/// (0..360, modulo); saturation and lightness are required percentages.
/// Alpha matches `rgb()`'s parser (number or percentage).
fn parse_hsl_args(s: &str, with_alpha: bool) -> Option<RgbaColor> {
    let parts: Vec<&str> = s
        .split(|c| c == ',' || c == '/')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    let need = if with_alpha { 4 } else { 3 };
    if parts.len() < need {
        return None;
    }
    let h_raw = parts[0];
    let h_deg = if let Some(num) = h_raw.strip_suffix("deg") {
        num.trim().parse::<f32>().ok()?
    } else if let Some(num) = h_raw.strip_suffix("rad") {
        num.trim().parse::<f32>().ok()? * (180.0 / std::f32::consts::PI)
    } else if let Some(num) = h_raw.strip_suffix("turn") {
        num.trim().parse::<f32>().ok()? * 360.0
    } else {
        h_raw.parse::<f32>().ok()?
    };
    let h = ((h_deg.rem_euclid(360.0)) / 360.0).max(0.0);
    let s_pct = parts[1].strip_suffix('%')?.trim().parse::<f32>().ok()?;
    let l_pct = parts[2].strip_suffix('%')?.trim().parse::<f32>().ok()?;
    let s_v = (s_pct / 100.0).clamp(0.0, 1.0);
    let l_v = (l_pct / 100.0).clamp(0.0, 1.0);
    let (r, g, b) = hsl_to_rgb(h, s_v, l_v);
    let a = if with_alpha {
        let v: f32 = if let Some(p) = parts[3].strip_suffix('%') {
            p.trim().parse::<f32>().ok()? / 100.0
        } else {
            parts[3].parse::<f32>().ok()?
        };
        (v.clamp(0.0, 1.0) * 255.0).round() as u8
    } else {
        255
    };
    Some(RgbaColor {
        r: (r * 255.0).round() as u8,
        g: (g * 255.0).round() as u8,
        b: (b * 255.0).round() as u8,
        a,
    })
}

/// Standard HSL → RGB. `h` is normalised to 0..1; `s` and `l` are 0..1.
fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    if s == 0.0 {
        return (l, l, l);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let to_rgb = |t: f32| {
        let t = if t < 0.0 {
            t + 1.0
        } else if t > 1.0 {
            t - 1.0
        } else {
            t
        };
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (to_rgb(h + 1.0 / 3.0), to_rgb(h), to_rgb(h - 1.0 / 3.0))
}

fn parse_hex(rest: &str) -> Option<RgbaColor> {
    let s = rest.trim();
    fn h(b: u8) -> Option<u8> {
        Some(match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        })
    }
    let bytes = s.as_bytes();
    match bytes.len() {
        3 => Some(RgbaColor {
            r: h(bytes[0])? * 17,
            g: h(bytes[1])? * 17,
            b: h(bytes[2])? * 17,
            a: 255,
        }),
        4 => Some(RgbaColor {
            r: h(bytes[0])? * 17,
            g: h(bytes[1])? * 17,
            b: h(bytes[2])? * 17,
            a: h(bytes[3])? * 17,
        }),
        6 => Some(RgbaColor {
            r: (h(bytes[0])? << 4) | h(bytes[1])?,
            g: (h(bytes[2])? << 4) | h(bytes[3])?,
            b: (h(bytes[4])? << 4) | h(bytes[5])?,
            a: 255,
        }),
        8 => Some(RgbaColor {
            r: (h(bytes[0])? << 4) | h(bytes[1])?,
            g: (h(bytes[2])? << 4) | h(bytes[3])?,
            b: (h(bytes[4])? << 4) | h(bytes[5])?,
            a: (h(bytes[6])? << 4) | h(bytes[7])?,
        }),
        _ => None,
    }
}

fn parse_rgb_args(s: &str, with_alpha: bool) -> Option<RgbaColor> {
    let parts: Vec<&str> = s
        .split(|c| c == ',' || c == '/')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    let need = if with_alpha { 4 } else { 3 };
    if parts.len() < need {
        return None;
    }
    let r = parse_color_component(parts[0])?;
    let g = parse_color_component(parts[1])?;
    let b = parse_color_component(parts[2])?;
    let a = if with_alpha {
        let v: f32 = parts[3].parse().ok()?;
        (v.clamp(0.0, 1.0) * 255.0).round() as u8
    } else {
        255
    };
    Some(RgbaColor { r, g, b, a })
}

fn parse_color_component(s: &str) -> Option<u8> {
    if let Some(p) = s.strip_suffix('%') {
        let v: f32 = p.trim().parse().ok()?;
        return Some((v.clamp(0.0, 100.0) / 100.0 * 255.0).round() as u8);
    }
    let v: f32 = s.parse().ok()?;
    Some(v.clamp(0.0, 255.0).round() as u8)
}

fn parse_named_color(v: &str) -> Option<RgbaColor> {
    let lc = v.to_ascii_lowercase();
    Some(match lc.as_str() {
        "transparent" => RgbaColor::TRANSPARENT,
        "black" => RgbaColor::rgb(0, 0, 0),
        "white" => RgbaColor::rgb(255, 255, 255),
        "red" => RgbaColor::rgb(255, 0, 0),
        "green" => RgbaColor::rgb(0, 128, 0),
        "blue" => RgbaColor::rgb(0, 0, 255),
        "yellow" => RgbaColor::rgb(255, 255, 0),
        "cyan" | "aqua" => RgbaColor::rgb(0, 255, 255),
        "magenta" | "fuchsia" => RgbaColor::rgb(255, 0, 255),
        "silver" => RgbaColor::rgb(192, 192, 192),
        "gray" | "grey" => RgbaColor::rgb(128, 128, 128),
        "lightgray" | "lightgrey" => RgbaColor::rgb(211, 211, 211),
        "darkgray" | "darkgrey" => RgbaColor::rgb(169, 169, 169),
        "maroon" => RgbaColor::rgb(128, 0, 0),
        "olive" => RgbaColor::rgb(128, 128, 0),
        "purple" => RgbaColor::rgb(128, 0, 128),
        "teal" => RgbaColor::rgb(0, 128, 128),
        "navy" => RgbaColor::rgb(0, 0, 128),
        "orange" => RgbaColor::rgb(255, 165, 0),
        "pink" => RgbaColor::rgb(255, 192, 203),
        _ => return None,
    })
}
