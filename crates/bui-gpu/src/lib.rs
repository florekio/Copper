//! bui-gpu — wgpu + vello compositor.
//!
//! Phase 0: owns the wgpu state and a vello renderer. Renders a
//! [`DisplayList`] from `bui-paint` into vello's intermediate Rgba8Unorm
//! storage texture, then blits to the window surface.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, OnceLock};

use bui_paint::{Color, DisplayList, PaintCommand, PathSegment, Rect as PaintRect};
use bui_text::Font;
use vello::{
    AaConfig, AaSupport, RenderParams, Renderer, RendererOptions, Scene,
    kurbo::{Affine, BezPath, Rect as KurboRect, RoundedRect as KurboRoundedRect, RoundedRectRadii, Vec2},
    peniko::{Blob, Color as VelloColor, Fill, ImageAlphaType, ImageData, ImageFormat, color::AlphaColor},
    util::{RenderContext, RenderSurface},
};
use winit::window::Window;

#[derive(Debug, thiserror::Error)]
pub enum CompositorError {
    #[error("acquire next surface texture: {0}")]
    Surface(#[from] wgpu::SurfaceError),
    #[error("vello: {0}")]
    Vello(String),
}

pub struct Compositor {
    _window: Arc<Window>,
    context: RenderContext,
    surface: RenderSurface<'static>,
    renderer: Renderer,
    scene: Scene,
    font: Font,
    images: HashMap<String, ImageData>,
    /// Per-(codepoint, color) pre-rasterized glyph image. Drawn via
    /// `Scene::draw_image` instead of emitting one rect per pixel —
    /// roughly two orders of magnitude fewer commands for dense pages.
    glyph_cache: HashMap<(u32, [u8; 4]), ImageData>,
    /// Display scale factor (1.0 standard, 2.0 retina, etc.). Applied as
    /// a global Affine to every paint command so the binary can lay out
    /// in logical pixels while the GPU surface stays at physical
    /// resolution. Set via `set_scale_factor`.
    scale_factor: f64,
}

impl Compositor {
    pub async fn new(window: Arc<Window>) -> Result<Self, CompositorError> {
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);

        let mut context = RenderContext::new();
        let surface: RenderSurface<'static> = context
            .create_surface(window.clone(), width, height, wgpu::PresentMode::AutoVsync)
            .await
            .map_err(|e| CompositorError::Vello(format!("{e:?}")))?;

        let device = &context.devices[surface.dev_id].device;
        let renderer = Renderer::new(
            device,
            RendererOptions {
                use_cpu: false,
                antialiasing_support: AaSupport::all(),
                num_init_threads: NonZeroUsize::new(1),
                pipeline_cache: None,
            },
        )
        .map_err(|e| CompositorError::Vello(format!("{e:?}")))?;

        Ok(Self {
            _window: window,
            context,
            surface,
            renderer,
            scene: Scene::new(),
            font: bui_text::shared_font().clone(),
            images: HashMap::new(),
            glyph_cache: HashMap::new(),
            scale_factor: 1.0,
        })
    }

    /// Set the display scale factor. The compositor multiplies every
    /// scene transform by this value, so the binary can stay in
    /// logical-pixel coordinates.
    pub fn set_scale_factor(&mut self, scale_factor: f64) {
        self.scale_factor = scale_factor.max(1e-6);
    }

    /// Upload (or replace) an RGBA8 image keyed by `key`. The key is
    /// typically the source URL — see `bui_image::Image` for the
    /// expected pixel layout (row-major, non-premultiplied).
    pub fn upload_image(&mut self, key: impl Into<String>, image: bui_image::Image) {
        let blob: Blob<u8> = Blob::from(image.pixels);
        let data = ImageData {
            data: blob,
            format: ImageFormat::Rgba8,
            alpha_type: ImageAlphaType::Alpha,
            width: image.width,
            height: image.height,
        };
        self.images.insert(key.into(), data);
    }

    pub fn drop_image(&mut self, key: &str) {
        self.images.remove(key);
    }

    /// Drain the global upload queue (populated via `enqueue_upload`)
    /// into this compositor's image cache. Called once at the top of
    /// every `render`.
    fn drain_upload_queue(&mut self) {
        if let Some(queue) = UPLOAD_QUEUE.get() {
            let drained: Vec<_> = queue.lock().unwrap().drain(..).collect();
            for (key, img) in drained {
                self.upload_image(key, img);
            }
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.context.resize_surface(&mut self.surface, width, height);
    }

    pub fn render(&mut self, list: &DisplayList) -> Result<(), CompositorError> {
        self.drain_upload_queue();
        self.scene.reset();
        let global = Affine::scale(self.scale_factor);
        for cmd in &list.commands {
            match cmd {
                PaintCommand::FillRect { rect, color } => {
                    let kr = KurboRect::new(
                        rect.x as f64,
                        rect.y as f64,
                        (rect.x + rect.w) as f64,
                        (rect.y + rect.h) as f64,
                    );
                    self.scene
                        .fill(Fill::NonZero, global, vello_color(*color), None, &kr);
                }
                PaintCommand::FillRoundedRect { rect, color, radii } => {
                    let kr = KurboRoundedRect::new(
                        rect.x as f64,
                        rect.y as f64,
                        (rect.x + rect.w) as f64,
                        (rect.y + rect.h) as f64,
                        RoundedRectRadii::new(
                            radii[0] as f64,
                            radii[1] as f64,
                            radii[2] as f64,
                            radii[3] as f64,
                        ),
                    );
                    self.scene
                        .fill(Fill::NonZero, global, vello_color(*color), None, &kr);
                }
                PaintCommand::FillPath { points, color } => {
                    if !points.is_empty() {
                        let mut path = BezPath::new();
                        path.move_to((points[0].0 as f64, points[0].1 as f64));
                        for p in &points[1..] {
                            path.line_to((p.0 as f64, p.1 as f64));
                        }
                        path.close_path();
                        self.scene.fill(
                            Fill::NonZero,
                            global,
                            vello_color(*color),
                            None,
                            &path,
                        );
                    }
                }
                PaintCommand::Image { rect, key } => {
                    if let Some(image) = self.images.get(key) {
                        let sx = rect.w as f64 / image.width.max(1) as f64;
                        let sy = rect.h as f64 / image.height.max(1) as f64;
                        let transform = global
                            * Affine::translate(Vec2::new(rect.x as f64, rect.y as f64))
                            * Affine::scale_non_uniform(sx, sy);
                        self.scene.draw_image(image, transform);
                    }
                }
                PaintCommand::Text {
                    x,
                    baseline,
                    advance: _,
                    font_size,
                    color,
                    content,
                } => {
                    paint_text_cached(
                        &mut self.scene,
                        &self.font,
                        &mut self.glyph_cache,
                        global,
                        content,
                        *x,
                        *baseline,
                        *font_size,
                        *color,
                    );
                }
                PaintCommand::PushClip { rect, radii } => {
                    let kr = KurboRoundedRect::new(
                        rect.x as f64,
                        rect.y as f64,
                        (rect.x + rect.w) as f64,
                        (rect.y + rect.h) as f64,
                        RoundedRectRadii::new(
                            radii[0] as f64,
                            radii[1] as f64,
                            radii[2] as f64,
                            radii[3] as f64,
                        ),
                    );
                    self.scene.push_clip_layer(Fill::NonZero, global, &kr);
                }
                PaintCommand::PopClip => {
                    self.scene.pop_layer();
                }
                PaintCommand::BoxShadow {
                    rect,
                    color,
                    radius,
                    blur,
                } => {
                    let kr = KurboRect::new(
                        rect.x as f64,
                        rect.y as f64,
                        (rect.x + rect.w) as f64,
                        (rect.y + rect.h) as f64,
                    );
                    // CSS blur-radius is roughly 2× the gaussian std dev.
                    let std_dev = (*blur as f64) * 0.5;
                    self.scene.draw_blurred_rounded_rect(
                        global,
                        kr,
                        vello_color(*color),
                        *radius as f64,
                        std_dev.max(0.5),
                    );
                }
                PaintCommand::Svg {
                    rect,
                    view_box,
                    segments,
                    fill,
                    stroke,
                    stroke_width,
                } => {
                    paint_svg_shape(
                        &mut self.scene,
                        global,
                        rect,
                        *view_box,
                        segments,
                        *fill,
                        *stroke,
                        *stroke_width,
                    );
                }
                // Sticky groups are resolved upstream by the scroll-shift
                // pass — they should never reach the GPU. Ignore defensively
                // so a future renderer path that bypasses the shift loop
                // still compiles instead of panicking.
                PaintCommand::PushStickyGroup { .. }
                | PaintCommand::PopStickyGroup => {}
            }
        }

        let device_handle = &self.context.devices[self.surface.dev_id];
        let device = &device_handle.device;
        let queue = &device_handle.queue;

        self.renderer
            .render_to_texture(
                device,
                queue,
                &self.scene,
                &self.surface.target_view,
                &RenderParams {
                    base_color: VelloColor::from_rgba8(0, 0, 0, 0),
                    width: self.surface.config.width,
                    height: self.surface.config.height,
                    antialiasing_method: AaConfig::Area,
                },
            )
            .map_err(|e| CompositorError::Vello(format!("{e:?}")))?;

        let frame = self.surface.surface.get_current_texture()?;
        let surface_view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("bui-gpu blit"),
        });
        self.surface
            .blitter
            .copy(device, &mut encoder, &self.surface.target_view, &surface_view);
        queue.submit([encoder.finish()]);
        frame.present();

        Ok(())
    }
}

fn vello_color(c: Color) -> VelloColor {
    AlphaColor::from_rgba8(c.r, c.g, c.b, c.a)
}

/// Build a kurbo `BezPath` from a list of `PathSegment`s and fill +
/// optionally stroke it inside `rect`. The `view_box` describes the
/// user-space rectangle the segments live in; we compose a translate +
/// scale that maps it onto the placed rect, then prepend the global
/// (HiDPI) scale.
fn paint_svg_shape(
    scene: &mut Scene,
    global: Affine,
    rect: &PaintRect,
    view_box: (f32, f32, f32, f32),
    segments: &[PathSegment],
    fill: Option<Color>,
    stroke: Option<Color>,
    stroke_width: f32,
) {
    if segments.is_empty() {
        return;
    }
    let (vbx, vby, vbw, vbh) = view_box;
    if vbw <= 0.0 || vbh <= 0.0 {
        return;
    }
    let sx = rect.w as f64 / vbw as f64;
    let sy = rect.h as f64 / vbh as f64;
    let transform = global
        * Affine::translate(Vec2::new(rect.x as f64, rect.y as f64))
        * Affine::scale_non_uniform(sx, sy)
        * Affine::translate(Vec2::new(-vbx as f64, -vby as f64));

    let mut path = BezPath::new();
    for seg in segments {
        match *seg {
            PathSegment::MoveTo(x, y) => path.move_to((x as f64, y as f64)),
            PathSegment::LineTo(x, y) => path.line_to((x as f64, y as f64)),
            PathSegment::CurveTo { c1, c2, end } => path.curve_to(
                (c1.0 as f64, c1.1 as f64),
                (c2.0 as f64, c2.1 as f64),
                (end.0 as f64, end.1 as f64),
            ),
            PathSegment::QuadTo { c, end } => {
                path.quad_to((c.0 as f64, c.1 as f64), (end.0 as f64, end.1 as f64))
            }
            PathSegment::Close => path.close_path(),
        }
    }
    if let Some(c) = fill {
        scene.fill(Fill::NonZero, transform, vello_color(c), None, &path);
    }
    if let Some(c) = stroke {
        let s = vello::kurbo::Stroke::new(stroke_width as f64);
        scene.stroke(&s, transform, vello_color(c), None, &path);
    }
}

static UPLOAD_QUEUE: OnceLock<Mutex<Vec<(String, bui_image::Image)>>> = OnceLock::new();

/// Queue an image for upload to the next-rendering compositor's cache,
/// keyed by `key` (typically the source URL). Decoded RGBA8 only;
/// `bui_image::decode` is the natural producer.
pub fn enqueue_upload(key: impl Into<String>, image: bui_image::Image) {
    UPLOAD_QUEUE
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap()
        .push((key.into(), image));
}

/// Build (or look up) a per-(codepoint, color) RGBA8 image and emit a
/// single `Scene::draw_image` per glyph. Two orders of magnitude fewer
/// vello commands than the per-pixel-rect path for dense text.
fn paint_text_cached(
    scene: &mut Scene,
    font: &Font,
    cache: &mut HashMap<(u32, [u8; 4]), ImageData>,
    global: Affine,
    text: &str,
    origin_x: f32,
    baseline: f32,
    font_size: f32,
    color: Color,
) {
    if font.is_ttf() {
        paint_text_ttf(scene, font, global, text, origin_x, baseline, font_size, color);
    } else {
        paint_text_bitmap(scene, font, cache, global, text, origin_x, baseline, font_size, color);
    }
}

fn paint_text_ttf(
    scene: &mut Scene,
    font: &Font,
    global: Affine,
    text: &str,
    origin_x: f32,
    baseline: f32,
    font_size: f32,
    color: Color,
) {
    if font.chain_len() == 0 {
        return;
    }
    let transform = global * Affine::translate(Vec2::new(origin_x as f64, baseline as f64));
    let brush = AlphaColor::from_rgba8(color.r, color.g, color.b, color.a);
    // Resolve each char to its (font_index, glyph_id) via the chain
    // and accumulate the per-glyph x using the resolving font's
    // advance. This matches what layout used to size the run, so
    // the painted glyphs land exactly where the line layout expects
    // them.
    let mut entries: Vec<(usize, u32, f32)> = Vec::with_capacity(text.chars().count());
    let mut x = 0.0f32;
    for ch in text.chars() {
        let (idx, gid) = font.font_for_char(ch);
        entries.push((idx, gid, x));
        x += font.glyph_advance_at(idx, ch, font_size);
    }
    if entries.is_empty() {
        return;
    }
    // Group consecutive chars sharing the same font index into a
    // single draw_glyphs call. This keeps per-font batches tight
    // while still preserving the original char-order x-positions.
    let mut i = 0;
    while i < entries.len() {
        let cur_idx = entries[i].0;
        let mut batch: Vec<vello::Glyph> = Vec::new();
        while i < entries.len() && entries[i].0 == cur_idx {
            batch.push(vello::Glyph {
                id: entries[i].1,
                x: entries[i].2,
                y: 0.0,
            });
            i += 1;
        }
        let Some(font_data) = font.peniko_font_at(cur_idx) else {
            continue;
        };
        scene
            .draw_glyphs(&font_data)
            .font_size(font_size)
            .brush(brush)
            .transform(transform)
            .draw(Fill::NonZero, batch.into_iter());
    }
}

fn paint_text_bitmap(
    scene: &mut Scene,
    font: &Font,
    cache: &mut HashMap<(u32, [u8; 4]), ImageData>,
    global: Affine,
    text: &str,
    origin_x: f32,
    baseline: f32,
    font_size: f32,
    color: Color,
) {
    let scale = font_size / font.native_size;
    let metrics = font.metrics_for_size(font_size);
    let baseline_row = bui_text::BASELINE as f32;
    let key_color = [color.r, color.g, color.b, color.a];
    let mut pen_x = origin_x;
    for ch in text.chars() {
        if !ch.is_whitespace() {
            let cp = ch as u32;
            let key = (cp, key_color);
            let img = cache
                .entry(key)
                .or_insert_with(|| build_glyph_image(font, ch, color));
            // Place the cached glyph image at (pen_x, baseline - baseline_row*scale),
            // scaled up to physical pixels by the global transform.
            let glyph_top = baseline - baseline_row * scale;
            let transform = global
                * Affine::translate(Vec2::new(pen_x as f64, glyph_top as f64))
                * Affine::scale(scale as f64);
            scene.draw_image(img as &ImageData, transform);
        }
        pen_x += metrics.advance_per_char;
    }
}

fn build_glyph_image(font: &Font, ch: char, color: Color) -> ImageData {
    let glyph = font.glyph_for(ch).expect("glyph_for fallback");
    let w = glyph.width;
    let h = glyph.height;
    let mut bytes = vec![0u8; (w * h * 4) as usize];
    for (gx, gy, alpha) in glyph.pixels() {
        let i = ((gy * w + gx) * 4) as usize;
        let final_alpha = ((color.a as u32 * alpha as u32) / 255) as u8;
        bytes[i] = color.r;
        bytes[i + 1] = color.g;
        bytes[i + 2] = color.b;
        bytes[i + 3] = final_alpha;
    }
    ImageData {
        data: Blob::from(bytes),
        format: ImageFormat::Rgba8,
        alpha_type: ImageAlphaType::Alpha,
        width: w,
        height: h,
    }
}
