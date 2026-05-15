//! bui-text — bitmap font + (optional) real TTF backend.
//!
//! Two paths now coexist:
//!
//!   * **Bitmap** — the original 6×12 antialiased bitmap (see
//!     `bitmap_data.rs`), still used as the unconditional fallback so
//!     tests and headless builds work without a font file.
//!   * **TTF** — a `peniko::Font` loaded from disk. Glyph advances and
//!     line metrics come from `skrifa`; rasterisation happens later in
//!     `bui-gpu` via `vello::Scene::draw_glyphs`. Loaded lazily by
//!     `shared_font()` from a list of well-known system paths.
//!
//! The public `Font` type wraps both backends. `measure_text` and
//! `glyph_advance` give callers width data in CSS pixels regardless of
//! backend; `glyph_id` + the underlying `peniko::Font` (via `peniko_font`)
//! lets the compositor build a proper glyph run when a TTF is loaded.

mod bitmap_data;

pub use bitmap_data::{BASELINE, CELL_H, CELL_W};

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use bitmap_data::{FIRST_CODEPOINT, GLYPHS, LAST_CODEPOINT};
use peniko::{Blob, FontData};
use skrifa::instance::{LocationRef, Size};
use skrifa::metrics::GlyphMetrics;
use skrifa::{FontRef, MetadataProvider};

/// A single glyph in the embedded bitmap font. Pixels are antialiased:
/// each byte is the coverage alpha (0..255) of one pixel, row-major.
#[derive(Debug, Clone, Copy)]
pub struct BitmapGlyph {
    pub width: u32,
    pub height: u32,
    pub baseline: u32,
    /// `width * height` bytes, row-major.
    pub pixels: &'static [u8],
}

impl BitmapGlyph {
    /// Iterate `(x, y, alpha)` for every non-zero pixel in the glyph.
    pub fn pixels(&self) -> impl Iterator<Item = (u32, u32, u8)> + '_ {
        let w = self.width;
        let h = self.height;
        (0..h).flat_map(move |y| {
            (0..w).filter_map(move |x| {
                let a = self.pixels[(y * w + x) as usize];
                if a > 0 { Some((x, y, a)) } else { None }
            })
        })
    }
}

#[derive(Clone)]
pub struct Font {
    pub family: String,
    /// Native pixel size of the bitmap data — the size at which one cell of
    /// the source bitmap maps 1:1 to one rendered pixel. For the TTF
    /// backend we use this only as a "best effort" advance fallback when
    /// callers ask for `metrics_for_size().advance_per_char` (the TTF
    /// backend reports a typical advance via `lookup_average_advance`).
    pub native_size: f32,
    backend: Backend,
}

#[derive(Clone)]
enum Backend {
    Bitmap,
    /// Primary TTF + fallback chain. Glyphs missing from the
    /// primary fall back to the first fallback that has them
    /// (CJK ↦ PingFang, Arabic ↦ GeezaPro, emoji ↦ Apple Color
    /// Emoji, etc.). The compositor splits a text run into
    /// per-font sub-runs at draw time.
    Ttf(Vec<TtfFont>),
}

#[derive(Clone)]
struct TtfFont {
    /// Owned font data. Wrapped via peniko::Blob so the same allocation
    /// can be handed to vello (`peniko::Font`) without a copy.
    blob: Blob<u8>,
    /// Index inside a `.ttc` collection — 0 for plain `.ttf` files.
    index: u32,
    /// Average advance width at native_size, used when callers want a
    /// scalar `advance_per_char` (e.g. setting an input's intrinsic
    /// width). Computed once at load.
    average_advance_native: f32,
    /// Cached upem / ascent / descent / line-gap at native_size.
    native_metrics: NativeMetrics,
    /// Cache of (codepoint, font_size) -> advance_width for hot text
    /// runs. Wrapped in Mutex because closures called from compositor /
    /// layout must be Send + Sync.
    advance_cache: std::sync::Arc<Mutex<HashMap<(u32, u32), f32>>>,
}

#[derive(Clone, Copy)]
struct NativeMetrics {
    ascent: f32,
    descent: f32,
    leading: f32,
    x_height: f32,
}

impl std::fmt::Debug for Font {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let backend = match self.backend {
            Backend::Bitmap => "bitmap",
            Backend::Ttf(_) => "ttf",
        };
        f.debug_struct("Font")
            .field("family", &self.family)
            .field("native_size", &self.native_size)
            .field("backend", &backend)
            .finish()
    }
}

impl Font {
    pub fn bitmap_default() -> Self {
        Self {
            family: "bui-bitmap".to_string(),
            native_size: CELL_H as f32,
            backend: Backend::Bitmap,
        }
    }

    /// Backwards-compat alias for callers that built against the Phase-4
    /// monospace metric.
    pub fn monospace_fallback() -> Self {
        Self::bitmap_default()
    }

    /// Try to load a TTF/TTC from `bytes` at the given collection
    /// `index` (0 for plain `.ttf`). Returns `None` if the data fails
    /// to parse.
    pub fn from_ttf_bytes(bytes: Vec<u8>, index: u32) -> Option<Self> {
        let blob = Blob::new(std::sync::Arc::new(bytes));
        // Validate by parsing once. The cached metrics also come from
        // this pass.
        let font_ref = FontRef::from_index(blob.data(), index).ok()?;
        let native_size = 16.0_f32;
        let metrics = font_ref.metrics(Size::new(native_size), LocationRef::default());
        let native_metrics = NativeMetrics {
            ascent: metrics.ascent,
            descent: -metrics.descent, // skrifa returns descent as negative
            leading: metrics.leading,
            x_height: metrics.x_height.unwrap_or(metrics.ascent * 0.5),
        };
        let avg = lookup_average_advance(&font_ref, native_size);
        let family = font_ref
            .localized_strings(skrifa::string::StringId::FAMILY_NAME)
            .english_or_first()
            .map(|s| s.chars().collect::<String>())
            .unwrap_or_else(|| "ttf".to_string());
        Some(Self {
            family,
            native_size,
            backend: Backend::Ttf(vec![TtfFont {
                blob,
                index,
                average_advance_native: avg,
                native_metrics,
                advance_cache: std::sync::Arc::new(Mutex::new(HashMap::new())),
            }]),
        })
    }

    /// Append `bytes` (and `index`) as a fallback font behind any
    /// existing primary. No-op for the bitmap backend. Used by
    /// `try_load_system_font` to chain CJK / Arabic / emoji fonts
    /// behind the Latin primary so missing glyphs route to a font
    /// that has them instead of rendering as `.notdef` boxes.
    pub fn push_fallback(&mut self, bytes: Vec<u8>, index: u32) -> bool {
        let blob = Blob::new(std::sync::Arc::new(bytes));
        let Ok(font_ref) = FontRef::from_index(blob.data(), index) else {
            return false;
        };
        let metrics = font_ref.metrics(Size::new(self.native_size), LocationRef::default());
        let native_metrics = NativeMetrics {
            ascent: metrics.ascent,
            descent: -metrics.descent,
            leading: metrics.leading,
            x_height: metrics.x_height.unwrap_or(metrics.ascent * 0.5),
        };
        let avg = lookup_average_advance(&font_ref, self.native_size);
        let entry = TtfFont {
            blob,
            index,
            average_advance_native: avg,
            native_metrics,
            advance_cache: std::sync::Arc::new(Mutex::new(HashMap::new())),
        };
        if let Backend::Ttf(chain) = &mut self.backend {
            chain.push(entry);
            true
        } else {
            // Bitmap-only mode — promote to a TTF chain so future
            // chars can use the fallback even though no primary loaded.
            self.backend = Backend::Ttf(vec![entry]);
            true
        }
    }

    /// True if this font is a TTF (vs the bitmap fallback). The
    /// compositor uses this to decide between bitmap blits and
    /// vello's glyph drawing.
    pub fn is_ttf(&self) -> bool {
        matches!(self.backend, Backend::Ttf(_))
    }

    /// Return the underlying `peniko::FontData` for vello drawing —
    /// always the primary font of the chain. `None` for the bitmap
    /// backend or an empty chain.
    pub fn peniko_font(&self) -> Option<FontData> {
        self.peniko_font_at(0)
    }

    /// Return the `peniko::FontData` at chain index `idx`. Used by
    /// the GPU painter to bind the right font when emitting a
    /// fallback-glyph sub-run.
    pub fn peniko_font_at(&self, idx: usize) -> Option<FontData> {
        match &self.backend {
            Backend::Bitmap => None,
            Backend::Ttf(chain) => chain
                .get(idx)
                .map(|t| FontData::new(t.blob.clone(), t.index)),
        }
    }

    /// Look up the glyph for a codepoint in the bitmap backend. The TTF
    /// backend doesn't expose bitmap glyphs (rasterisation lives in the
    /// compositor); this returns `None` there.
    pub fn glyph_for(&self, c: char) -> Option<BitmapGlyph> {
        if !matches!(self.backend, Backend::Bitmap) {
            return None;
        }
        let cp = c as u32;
        let row = if (FIRST_CODEPOINT..=LAST_CODEPOINT).contains(&cp) {
            (cp - FIRST_CODEPOINT) as usize
        } else {
            // Out-of-range fallback: a question mark in a box.
            (b'?' as u32 - FIRST_CODEPOINT) as usize
        };
        Some(BitmapGlyph {
            width: CELL_W as u32,
            height: CELL_H as u32,
            baseline: BASELINE as u32,
            pixels: &GLYPHS[row],
        })
    }

    pub fn metrics_for_size(&self, size: f32) -> FontMetrics {
        match &self.backend {
            Backend::Bitmap => {
                let scale = size / self.native_size;
                FontMetrics {
                    ascender: (BASELINE as f32) * scale,
                    descender: ((CELL_H - BASELINE) as f32) * scale,
                    leading: 2.0 * scale,
                    x_height: 5.0 * scale,
                    advance_per_char: (CELL_W as f32) * scale,
                }
            }
            Backend::Ttf(chain) => {
                // Always report metrics from the primary so line
                // height stays consistent across runs that may dip
                // into fallback fonts mid-line.
                let Some(t) = chain.first() else {
                    let scale = size / self.native_size;
                    return FontMetrics {
                        ascender: (BASELINE as f32) * scale,
                        descender: ((CELL_H - BASELINE) as f32) * scale,
                        leading: 2.0 * scale,
                        x_height: 5.0 * scale,
                        advance_per_char: (CELL_W as f32) * scale,
                    };
                };
                let scale = size / self.native_size;
                FontMetrics {
                    ascender: t.native_metrics.ascent * scale,
                    descender: t.native_metrics.descent * scale,
                    leading: t.native_metrics.leading * scale,
                    x_height: t.native_metrics.x_height * scale,
                    advance_per_char: t.average_advance_native * scale,
                }
            }
        }
    }

    /// Width of a single glyph at the given font size. Walks the
    /// fallback chain — first font that has `ch` decides the advance.
    pub fn glyph_advance(&self, ch: char, font_size: f32) -> f32 {
        match &self.backend {
            Backend::Bitmap => self.metrics_for_size(font_size).advance_per_char,
            Backend::Ttf(_) => {
                let (idx, _gid) = self.font_for_char(ch);
                self.glyph_advance_at(idx, ch, font_size)
            }
        }
    }

    /// Same as `glyph_advance`, but pinned to chain index `idx`. Used
    /// by the GPU painter when emitting a per-font sub-run so the
    /// run's x-cursor uses the same font's advance every time.
    pub fn glyph_advance_at(&self, idx: usize, ch: char, font_size: f32) -> f32 {
        let Backend::Ttf(chain) = &self.backend else {
            return self.metrics_for_size(font_size).advance_per_char;
        };
        let Some(t) = chain.get(idx) else {
            return self.metrics_for_size(font_size).advance_per_char;
        };
        let key = (ch as u32, (font_size * 64.0) as u32);
        if let Some(v) = t.advance_cache.lock().unwrap().get(&key).copied() {
            return v;
        }
        let Ok(font_ref) = FontRef::from_index(t.blob.data(), t.index) else {
            return self.metrics_for_size(font_size).advance_per_char;
        };
        let charmap = font_ref.charmap();
        let gid = charmap.map(ch).unwrap_or(skrifa::GlyphId::NOTDEF);
        let gm: GlyphMetrics =
            font_ref.glyph_metrics(Size::new(font_size), LocationRef::default());
        let adv = gm
            .advance_width(gid)
            .unwrap_or(t.average_advance_native * font_size / self.native_size);
        t.advance_cache.lock().unwrap().insert(key, adv);
        adv
    }

    /// Total advance width of `text` at `font_size`, in CSS pixels.
    pub fn measure_text(&self, text: &str, font_size: f32) -> f32 {
        self.measure_text_with_spacing(text, font_size, 0.0)
    }

    /// Like `measure_text`, but adds `letter_spacing` between every
    /// glyph (matches CSS letter-spacing semantics — extra gap goes
    /// after each glyph including the last, but most callers only
    /// care about prefix widths so the trailing gap rarely matters).
    pub fn measure_text_with_spacing(&self, text: &str, font_size: f32, letter_spacing: f32) -> f32 {
        let mut w = 0.0;
        for c in text.chars() {
            w += self.glyph_advance(c, font_size) + letter_spacing;
        }
        w
    }

    /// Map a codepoint to its glyph id, resolved through the
    /// fallback chain. Returns 0 for the bitmap backend.
    pub fn glyph_id(&self, ch: char) -> u32 {
        self.font_for_char(ch).1
    }

    /// Walk the TTF fallback chain to find the first font with a
    /// non-`.notdef` glyph for `ch`. Returns `(font_index, gid)`.
    /// Falls back to `(0, 0)` (the primary's notdef) when nothing
    /// in the chain has the glyph — keeping callers' bookkeeping
    /// uniform.
    pub fn font_for_char(&self, ch: char) -> (usize, u32) {
        let Backend::Ttf(chain) = &self.backend else {
            return (0, 0);
        };
        for (idx, t) in chain.iter().enumerate() {
            let Ok(font_ref) = FontRef::from_index(t.blob.data(), t.index) else {
                continue;
            };
            if let Some(gid) = font_ref.charmap().map(ch) {
                let g = gid.to_u32();
                if g != 0 {
                    return (idx, g);
                }
            }
        }
        (0, 0)
    }

    /// Number of fonts in the chain (1 for a single-TTF setup, more
    /// when fallbacks are loaded). Returns 0 for the bitmap backend.
    pub fn chain_len(&self) -> usize {
        match &self.backend {
            Backend::Bitmap => 0,
            Backend::Ttf(chain) => chain.len(),
        }
    }
}

/// Pick a representative advance width (used as `advance_per_char` for
/// callers that still expect a scalar). We sample uppercase 'M' / 'x' /
/// '0' and take the average — this matches what a "ch" unit roughly
/// resolves to and avoids being skewed by very narrow letters like 'i'.
fn lookup_average_advance(font_ref: &FontRef<'_>, size: f32) -> f32 {
    let charmap = font_ref.charmap();
    let gm = font_ref.glyph_metrics(Size::new(size), LocationRef::default());
    let mut samples: Vec<f32> = Vec::new();
    for ch in ['M', 'x', '0', 'a'] {
        if let Some(gid) = charmap.map(ch) {
            if let Some(adv) = gm.advance_width(gid) {
                samples.push(adv);
            }
        }
    }
    if samples.is_empty() {
        return size * 0.55; // last-resort heuristic
    }
    samples.iter().sum::<f32>() / samples.len() as f32
}

/// Process-wide cached font. The first call probes a list of known
/// system paths for a TTF/TTC; if none load, we fall back to the
/// bitmap. Subsequent calls are O(1).
pub fn shared_font() -> &'static Font {
    static FONT: OnceLock<Font> = OnceLock::new();
    FONT.get_or_init(|| try_load_system_font().unwrap_or_else(Font::bitmap_default))
}

fn try_load_system_font() -> Option<Font> {
    // Honour an env override first — mostly useful for tests / CI.
    if let Ok(path) = std::env::var("BUI_FONT_PATH") {
        if let Ok(bytes) = std::fs::read(&path) {
            if let Some(f) = Font::from_ttf_bytes(bytes, 0) {
                return Some(f);
            }
        }
    }
    // Primary preference: a broad-Unicode-coverage system sans.
    // macOS SFNS.ttf has Latin + Greek + Cyrillic + extended Latin
    // diacritics out of the box, which fixes the box-glyph fallback
    // we get from plain Helvetica on multilingual content (Wikipedia
    // language strips, etc.). We still keep Helvetica / DejaVu /
    // Segoe UI as fallbacks for systems without SFNS.
    let candidates: &[(&str, u32)] = &[
        ("/System/Library/Fonts/SFNS.ttf", 0),
        ("/System/Library/Fonts/Helvetica.ttc", 0),
        ("/System/Library/Fonts/HelveticaNeue.ttc", 0),
        ("/System/Library/Fonts/Supplemental/Arial.ttf", 0),
        ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 0),
        ("/usr/share/fonts/dejavu/DejaVuSans.ttf", 0),
        ("/usr/share/fonts/TTF/DejaVuSans.ttf", 0),
        ("/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf", 0),
        ("C:\\Windows\\Fonts\\segoeui.ttf", 0),
        ("C:\\Windows\\Fonts\\arial.ttf", 0),
    ];
    let mut primary: Option<Font> = None;
    for (path, index) in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            if let Some(f) = Font::from_ttf_bytes(bytes, *index) {
                primary = Some(f);
                break;
            }
        }
    }
    let mut font = primary?;

    // Stack fallbacks behind the primary so missing-glyph chars
    // route to a font that has them. Order matters — the first
    // fallback that has a glyph wins. CJK takes priority over
    // Apple Symbols since most missing-glyph chars on Wikipedia
    // are CJK / Cyrillic-extended.
    let fallbacks: &[(&str, u32)] = &[
        // CJK ideographs + many extended scripts in one TTC.
        ("/System/Library/Fonts/PingFang.ttc", 0),
        ("/System/Library/Fonts/STHeiti Light.ttc", 0),
        ("/System/Library/Fonts/Hiragino Sans GB.ttc", 0),
        // Arabic / Hebrew dedicated fonts on macOS.
        ("/System/Library/Fonts/SFArabic.ttf", 0),
        ("/System/Library/Fonts/GeezaPro.ttc", 0),
        ("/System/Library/Fonts/SFHebrew.ttf", 0),
        ("/System/Library/Fonts/Apple Symbols.ttf", 0),
        // Last-ditch: Apple's "LastResort" font carries placeholder
        // glyphs for every script — visually marks "this script
        // exists" rather than "char missing".
        ("/System/Library/Fonts/LastResort.otf", 0),
        // Linux fallbacks (Noto, DejaVu) — broad coverage if available.
        ("/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc", 0),
        ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 0),
        ("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", 0),
    ];
    for (path, index) in fallbacks {
        if let Ok(bytes) = std::fs::read(path) {
            font.push_fallback(bytes, *index);
        }
    }
    Some(font)
}

#[derive(Debug, Clone, Copy)]
pub struct FontMetrics {
    pub ascender: f32,
    pub descender: f32,
    pub leading: f32,
    pub x_height: f32,
    pub advance_per_char: f32,
}

impl FontMetrics {
    /// Backwards-compat: callers built before the rename used `line_gap`.
    pub fn line_gap(&self) -> f32 {
        self.leading
    }
}

/// Result of shaping a string. Each glyph carries its codepoint plus an
/// `x_offset` from the run origin — once we ship a real shaper this is
/// where kerning + GPOS lands.
#[derive(Debug, Clone)]
pub struct ShapedRun {
    pub glyphs: Vec<ShapedGlyph>,
    pub advance: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct ShapedGlyph {
    pub codepoint: char,
    pub x_offset: f32,
    pub advance: f32,
}

pub fn shape_text(font: &Font, text: &str, font_size: f32) -> ShapedRun {
    let metrics = font.metrics_for_size(font_size);
    let advance_per = metrics.advance_per_char;
    let mut glyphs = Vec::with_capacity(text.chars().count());
    let mut x = 0.0f32;
    for c in text.chars() {
        glyphs.push(ShapedGlyph {
            codepoint: c,
            x_offset: x,
            advance: advance_per,
        });
        x += advance_per;
    }
    ShapedRun {
        glyphs,
        advance: x,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_letters_have_pixels() {
        let f = Font::bitmap_default();
        let g = f.glyph_for('M').unwrap();
        let lit: Vec<_> = g.pixels().collect();
        assert!(!lit.is_empty(), "M should have lit pixels");
        let (min_y, max_y) = lit
            .iter()
            .fold((u32::MAX, 0u32), |(lo, hi), &(_, y, _)| (lo.min(y), hi.max(y)));
        assert!(max_y - min_y >= 5, "M cap height is at least 5 rows");
    }

    #[test]
    fn space_is_blank() {
        let f = Font::bitmap_default();
        let g = f.glyph_for(' ').unwrap();
        assert_eq!(g.pixels().count(), 0);
    }

    #[test]
    fn glyphs_have_partial_coverage() {
        // AA glyphs include sub-255 alpha pixels at the edges.
        let f = Font::bitmap_default();
        let g = f.glyph_for('o').unwrap();
        let has_partial = g.pixels().any(|(_, _, a)| a > 0 && a < 255);
        assert!(has_partial, "expected antialiased edges on 'o'");
    }

    #[test]
    fn shape_advances_match_metrics() {
        let f = Font::bitmap_default();
        let m = f.metrics_for_size(24.0);
        let run = shape_text(&f, "abc", 24.0);
        assert_eq!(run.glyphs.len(), 3);
        assert!((run.advance - 3.0 * m.advance_per_char).abs() < 1e-3);
        assert!((run.glyphs[1].x_offset - m.advance_per_char).abs() < 1e-3);
    }

    #[test]
    fn out_of_range_falls_back_to_question_mark() {
        let f = Font::bitmap_default();
        let g_q = f.glyph_for('?').unwrap();
        let g_emoji = f.glyph_for('🦀').unwrap();
        // Same lit-pixel count means the fallback is rendering '?'.
        assert_eq!(g_q.pixels().count(), g_emoji.pixels().count());
    }
}
