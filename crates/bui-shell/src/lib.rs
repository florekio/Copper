//! bui-shell — winit window + event loop wrapped around `bui_gpu::Compositor`.

use std::sync::Arc;

use bui_gpu::Compositor;
use bui_paint::DisplayList;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent as WinitKeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key as WinitKey, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};
#[cfg(target_os = "macos")]
use winit::platform::macos::WindowAttributesExtMacOS;

#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    /// Last known cursor position in viewport pixels. Components are
    /// negative when the cursor isn't currently inside the window.
    pub cursor: (f32, f32),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Modifiers {
    /// macOS ⌘ / Windows+Linux Super
    pub cmd: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Tab,
    Enter,
    Escape,
    Backspace,
    Delete,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Home,
    End,
    PageUp,
    PageDown,
    Other,
}

#[derive(Debug, Clone, Copy)]
pub struct KeyPress {
    pub key: Key,
    pub modifiers: Modifiers,
    pub repeat: bool,
}

/// Subset of mouse-cursor icons the binary may request. Mirrors the
/// most common CSS `cursor` keywords; bui-shell maps each to its
/// nearest winit equivalent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorIcon {
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

pub type SceneFn = Box<dyn FnMut(Viewport) -> DisplayList>;
pub type ClickFn = Box<dyn FnMut(Viewport, f32, f32, Modifiers) -> bool>;
pub type ScrollFn = Box<dyn FnMut(f32) -> bool>;
pub type KeyFn = Box<dyn FnMut(KeyPress) -> bool>;
/// `f(viewport, x, y)` fires on every CursorMoved while the left
/// button is held down. Used by the binary to extend a drag
/// selection across page text.
pub type DragFn = Box<dyn FnMut(Viewport, f32, f32, Modifiers) -> bool>;
/// `f(viewport, x, y)` fires when the left button is released.
/// The binary uses this to finalize a drag selection so a follow-up
/// click can clear it without losing the just-completed range.
pub type MouseUpFn = Box<dyn FnMut(Viewport, f32, f32, Modifiers) -> bool>;
/// `f(viewport, x, y)` fires on a right-button press. We use it to
/// trigger Copy on the current page selection without a context menu.
pub type RightClickFn = Box<dyn FnMut(Viewport, f32, f32, Modifiers) -> bool>;
/// `f(viewport)` returns the cursor icon to show for the current
/// pointer position. Called on every CursorMoved without forcing a
/// repaint, so hover affordances can change without re-laying out.
pub type CursorFn = Box<dyn FnMut(Viewport) -> CursorIcon>;

pub struct App {
    window: Option<Arc<Window>>,
    compositor: Option<Compositor>,
    scene_fn: SceneFn,
    on_click: Option<ClickFn>,
    on_scroll: Option<ScrollFn>,
    on_key: Option<KeyFn>,
    on_cursor: Option<CursorFn>,
    on_drag: Option<DragFn>,
    on_mouse_up: Option<MouseUpFn>,
    on_right_click: Option<RightClickFn>,
    title: String,
    initial_size: (u32, u32),
    modifiers: Modifiers,
    /// Cursor position in physical pixels (raw winit). Converted to
    /// logical units only when handed to the binary.
    cursor: (f32, f32),
    /// True between a left-button press and the matching release.
    /// Drives whether `on_drag` fires on each `CursorMoved`.
    left_pressed: bool,
    /// Current display scale factor (1.0 on standard displays, 2.0 on
    /// retina, etc.). Updated on `WindowEvent::ScaleFactorChanged`.
    scale_factor: f64,
    /// Last cursor icon we asked winit to display. Cached so we only
    /// touch the OS cursor when the icon actually changes.
    current_cursor: CursorIcon,
}

impl App {
    pub fn new<F>(scene_fn: F) -> Self
    where
        F: FnMut(Viewport) -> DisplayList + 'static,
    {
        Self {
            window: None,
            compositor: None,
            scene_fn: Box::new(scene_fn),
            on_click: None,
            on_scroll: None,
            on_key: None,
            on_cursor: None,
            on_drag: None,
            on_mouse_up: None,
            on_right_click: None,
            title: "bui".to_string(),
            initial_size: (1280, 800),
            modifiers: Modifiers::default(),
            cursor: (0.0, 0.0),
            left_pressed: false,
            scale_factor: 1.0,
            current_cursor: CursorIcon::Default,
        }
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    pub fn with_size(mut self, width: u32, height: u32) -> Self {
        self.initial_size = (width, height);
        self
    }

    /// `f(viewport, x, y)` is called with viewport-pixel coordinates on
    /// left-button click. Return `true` to request a redraw (e.g. after
    /// navigation or a tab switch).
    pub fn on_click<F>(mut self, f: F) -> Self
    where
        F: FnMut(Viewport, f32, f32, Modifiers) -> bool + 'static,
    {
        self.on_click = Some(Box::new(f));
        self
    }

    /// `f(delta_y)` is called on each wheel event. Positive `delta_y` ≈
    /// content moves down, negative ≈ content moves up. The binary owns
    /// its own scroll bookkeeping (per-tab); App just forwards the delta.
    /// Return `true` to request a redraw.
    pub fn on_scroll<F>(mut self, f: F) -> Self
    where
        F: FnMut(f32) -> bool + 'static,
    {
        self.on_scroll = Some(Box::new(f));
        self
    }

    /// `f(key_press)` runs on every keyboard *Pressed* event after modifier
    /// state is up to date. Return `true` to request a redraw.
    pub fn on_key<F>(mut self, f: F) -> Self
    where
        F: FnMut(KeyPress) -> bool + 'static,
    {
        self.on_key = Some(Box::new(f));
        self
    }

    /// `f(viewport)` returns the cursor icon to show. Called on every
    /// `CursorMoved` so hover affordances (link → pointer, input → text)
    /// update without forcing a repaint.
    pub fn on_cursor<F>(mut self, f: F) -> Self
    where
        F: FnMut(Viewport) -> CursorIcon + 'static,
    {
        self.on_cursor = Some(Box::new(f));
        self
    }

    /// `f(viewport, x, y, mods)` fires on each `CursorMoved` while the
    /// left button is held — the standard "drag" gesture. Used by the
    /// binary to extend a page-text selection in real time.
    pub fn on_drag<F>(mut self, f: F) -> Self
    where
        F: FnMut(Viewport, f32, f32, Modifiers) -> bool + 'static,
    {
        self.on_drag = Some(Box::new(f));
        self
    }

    /// `f(viewport, x, y, mods)` fires when the left button is
    /// released. The binary uses this to finalize a drag selection.
    pub fn on_mouse_up<F>(mut self, f: F) -> Self
    where
        F: FnMut(Viewport, f32, f32, Modifiers) -> bool + 'static,
    {
        self.on_mouse_up = Some(Box::new(f));
        self
    }

    /// `f(viewport, x, y, mods)` fires on a right-button press —
    /// the binary uses it to copy any current selection to the
    /// system clipboard (no context menu UI yet).
    pub fn on_right_click<F>(mut self, f: F) -> Self
    where
        F: FnMut(Viewport, f32, f32, Modifiers) -> bool + 'static,
    {
        self.on_right_click = Some(Box::new(f));
        self
    }

    pub fn run(mut self) -> Result<(), winit::error::EventLoopError> {
        let event_loop = EventLoop::new().expect("event loop");
        event_loop.run_app(&mut self)
    }
}

/// Build a `Viewport` with all dimensions (and the cursor) converted from
/// physical winit pixels to logical units, so the binary's paint + hit-test
/// code can work in the same coordinate space the GPU scales back up at
/// composition time.
///
/// Free function (rather than a method on `App`) so we don't clash with
/// the live `&mut` borrow on `self.compositor` in the event handler.
fn logical_viewport(window: &Window, scale_factor: f64, cursor: (f32, f32)) -> Viewport {
    let size = window.inner_size();
    let sf = scale_factor.max(1e-6);
    let logical_w = ((size.width as f64) / sf) as u32;
    let logical_h = ((size.height as f64) / sf) as u32;
    let logical_cursor = (cursor.0 / sf as f32, cursor.1 / sf as f32);
    Viewport {
        width: logical_w.max(1),
        height: logical_h.max(1),
        cursor: logical_cursor,
    }
}

fn map_cursor_icon(c: CursorIcon) -> winit::window::CursorIcon {
    use winit::window::CursorIcon as W;
    match c {
        CursorIcon::Default => W::Default,
        CursorIcon::Pointer => W::Pointer,
        CursorIcon::Text => W::Text,
        CursorIcon::NotAllowed => W::NotAllowed,
        CursorIcon::Wait => W::Wait,
        CursorIcon::Crosshair => W::Crosshair,
        CursorIcon::Move => W::Move,
        CursorIcon::Help => W::Help,
        CursorIcon::Progress => W::Progress,
    }
}

fn translate_key(event: &WinitKeyEvent) -> Key {
    match &event.logical_key {
        WinitKey::Character(s) => {
            // Logical character — `s` is what the OS produces given current
            // modifier state, so Shift+'a' arrives as 'A'. Hotkey routing in
            // the binary does its own case-folding.
            if let Some(c) = s.chars().next() {
                Key::Char(c)
            } else {
                Key::Other
            }
        }
        WinitKey::Named(NamedKey::Tab) => Key::Tab,
        WinitKey::Named(NamedKey::Enter) => Key::Enter,
        WinitKey::Named(NamedKey::Escape) => Key::Escape,
        WinitKey::Named(NamedKey::Backspace) => Key::Backspace,
        WinitKey::Named(NamedKey::Delete) => Key::Delete,
        WinitKey::Named(NamedKey::ArrowLeft) => Key::ArrowLeft,
        WinitKey::Named(NamedKey::ArrowRight) => Key::ArrowRight,
        WinitKey::Named(NamedKey::ArrowUp) => Key::ArrowUp,
        WinitKey::Named(NamedKey::ArrowDown) => Key::ArrowDown,
        WinitKey::Named(NamedKey::Home) => Key::Home,
        WinitKey::Named(NamedKey::End) => Key::End,
        WinitKey::Named(NamedKey::PageUp) => Key::PageUp,
        WinitKey::Named(NamedKey::PageDown) => Key::PageDown,
        _ => Key::Other,
    }
}

fn modifiers_from_state(state: ModifiersState) -> Modifiers {
    Modifiers {
        cmd: state.super_key(),
        ctrl: state.control_key(),
        alt: state.alt_key(),
        shift: state.shift_key(),
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(&self.title)
            .with_inner_size(LogicalSize::new(self.initial_size.0, self.initial_size.1))
            .with_min_inner_size(LogicalSize::new(480u32, 320u32))
            .with_resizable(true);
        // Safari/Arc/Chrome titlebar-tab pattern: the chrome paints behind
        // the titlebar, the title text disappears, the traffic lights
        // stay. We DON'T enable `movable_by_window_background` — that
        // setting makes macOS consume any body click as a window-drag,
        // which silently swallows clicks on links.  The user still has
        // a draggable region: the strip behind the traffic lights at the
        // top-left (and they can also drag tabs that aren't currently
        // hosting a click handler).
        #[cfg(target_os = "macos")]
        let attrs = attrs
            .with_fullsize_content_view(true)
            .with_titlebar_transparent(true)
            .with_title_hidden(true);
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        let mut compositor =
            pollster::block_on(Compositor::new(window.clone())).expect("compositor init");
        self.scale_factor = window.scale_factor();
        compositor.set_scale_factor(self.scale_factor);
        window.request_redraw();
        self.window = Some(window);
        self.compositor = Some(compositor);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        let (Some(window), Some(compositor)) = (self.window.as_ref(), self.compositor.as_mut())
        else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                compositor.resize(size.width, size.height);
                window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = scale_factor;
                compositor.set_scale_factor(scale_factor);
                window.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let prev = self.cursor;
                let next = (position.x as f32, position.y as f32);
                self.cursor = next;
                // Repaint when the cursor is in (or just left) the chrome
                // region so hover state can update. The 200 px threshold
                // is comfortably above CHROME_HEIGHT (84) on the binary
                // side without making bui-shell aware of chrome layout.
                const CHROME_HOVER_REGION: f32 = 200.0;
                if prev.1 < CHROME_HOVER_REGION || next.1 < CHROME_HOVER_REGION {
                    window.request_redraw();
                }
                // Ask the binary what cursor we should be showing, but
                // only push it through to winit on a *change* — the OS
                // call is cheap but there's no point doing it 60×/sec.
                if let Some(f) = self.on_cursor.as_mut() {
                    let viewport = logical_viewport(window, self.scale_factor, self.cursor);
                    let want = f(viewport);
                    if want != self.current_cursor {
                        self.current_cursor = want;
                        window.set_cursor(map_cursor_icon(want));
                    }
                }
                // Drag: fire on every cursor move while the left button
                // is down so the binary can extend a text selection.
                if self.left_pressed {
                    if let Some(f) = self.on_drag.as_mut() {
                        let viewport = logical_viewport(window, self.scale_factor, self.cursor);
                        let cursor = viewport.cursor;
                        if f(viewport, cursor.0, cursor.1, self.modifiers) {
                            window.request_redraw();
                        }
                    }
                }
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = modifiers_from_state(m.state());
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * 32.0,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32,
                };
                let mut redraw = true;
                if let Some(f) = self.on_scroll.as_mut() {
                    redraw = f(dy);
                }
                if redraw {
                    window.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let viewport = logical_viewport(window, self.scale_factor, self.cursor);
                let (cx, cy) = viewport.cursor;
                match (state, button) {
                    (ElementState::Pressed, MouseButton::Left) => {
                        self.left_pressed = true;
                        if let Some(f) = self.on_click.as_mut() {
                            if f(viewport, cx, cy, self.modifiers) {
                                window.request_redraw();
                            }
                        }
                    }
                    (ElementState::Released, MouseButton::Left) => {
                        self.left_pressed = false;
                        if let Some(f) = self.on_mouse_up.as_mut() {
                            if f(viewport, cx, cy, self.modifiers) {
                                window.request_redraw();
                            }
                        }
                    }
                    (ElementState::Pressed, MouseButton::Right) => {
                        if let Some(f) = self.on_right_click.as_mut() {
                            if f(viewport, cx, cy, self.modifiers) {
                                window.request_redraw();
                            }
                        }
                    }
                    _ => {}
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if let Some(f) = self.on_key.as_mut() {
                    let press = KeyPress {
                        key: translate_key(&event),
                        modifiers: self.modifiers,
                        repeat: event.repeat,
                    };
                    if f(press) {
                        window.request_redraw();
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                let viewport = logical_viewport(window, self.scale_factor, self.cursor);
                let dl = (self.scene_fn)(viewport);
                if let Err(e) = compositor.render(&dl) {
                    eprintln!("render error: {e}");
                }
            }
            _ => {}
        }
    }
}
