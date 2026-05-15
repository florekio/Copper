# Architecture

Copper is a Cargo workspace of focused crates. The dependency graph
flows strictly bottom-up: networking and URL handling at the base,
DOM and CSS in the middle, layout and paint above them, the
windowing layer at the top. Every crate is independently testable.

```
copper (binary)
  ├── bui-shell          winit window + event loop
  │     └── bui-gpu      wgpu + vello compositor (intermediate texture + blit)
  │           └── bui-paint    renderer-agnostic display list
  ├── bui-layout         block + inline + minimal flex/grid layout
  │     ├── bui-style    cascade, computed values, UA stylesheet
  │     │     └── bui-css      tokenizer, parser, selectors L4 subset
  │     └── bui-paint
  ├── bui-html           HTML5 tokenizer + tree builder
  │     └── bui-dom      arena-allocated DOM tree
  ├── bui-js             Zinc integration (script eval, DOM events)
  │     ├── bui-dom
  │     └── zinc         the JS engine (separate repo: ../browser)
  ├── bui-net            HTTP/1.1 over rustls + tokio, cookie jar
  │     └── bui-url      WHATWG URL parser
  ├── bui-text           Unicode segmentation, BiDi, OpenType, glyph raster
  └── bui-image          PNG / JPEG / GIF decoders
```

## End-to-end pipeline

Single navigation, from a URL the user types to pixels on screen:

```
URL bar input
    └─► bui-url::Url::parse
        └─► bui-net::Client::get        (TLS, redirects, cookies)
            └─► bui-html::parse         (tokenizer + tree builder)
                └─► bui-dom::Document
                    └─► bui-style::style_document
                        │   ├── collect_author_stylesheets → bui-css::parse
                        │   ├── bui-style/src/ua.css
                        │   └── cascade → ComputedValues per node
                        └─► bui-layout::build_with_images + layout()
                            │   ├── images preloaded via bui-image
                            │   └── inline scripts run via bui-js (Zinc)
                            └─► bui-layout::paint
                                └─► bui-paint::DisplayList
                                    └─► bui/main.rs scroll-shift pass
                                        └─► bui-gpu compositor (vello)
                                            └─► bui-shell window swap
```

The `bui/src/main.rs` binary owns the chrome (sidebar, top bar,
dev-dock, status line). The page's painted `DisplayList` is
translated into chrome coordinates inside `build_scene`'s
scroll-shift loop — that's also where `position: sticky` group
clamps land and where dev-dock paint commands are appended on top
of the page output.

## Boundaries that aren't crossed

A few deliberate boundaries keep the dependency graph clean:

- **Chrome ↔ engine.** `bui/src/main.rs` is the only place that
  combines DOM state, layout, and chrome painting. Lower crates
  (bui-layout, bui-paint, …) don't know about tabs, the sidebar,
  the dev-dock, or scroll offsets.
- **Layout ↔ paint.** `bui-layout` emits `bui-paint::DisplayList`
  commands; it does not call into a renderer. The renderer
  (`bui-gpu`) consumes the display list with no knowledge of the
  layout tree that produced it.
- **Engine ↔ JS.** `bui-js` is the *only* crate that depends on
  Zinc. The DOM is shared via `Arc<Mutex<Document>>` so script
  mutations and the next layout pass see the same tree.

## What lives where

| Crate | Purpose | See |
|---|---|---|
| `bui-url`    | WHATWG-shaped URL parser. http(s) only. | [bui-url/src/lib.rs](../crates/bui-url/src/lib.rs) |
| `bui-net`    | Hand-rolled HTTP/1.1 + TLS via rustls, RFC 6265 cookie jar. | [bui-net/src/lib.rs](../crates/bui-net/src/lib.rs) |
| `bui-dom`    | Arena-allocated DOM tree; `NodeId(u32)` indices. | [bui-dom/src/lib.rs](../crates/bui-dom/src/lib.rs) |
| `bui-html`   | HTML5 tokenizer + tree builder. | [bui-html/src/lib.rs](../crates/bui-html/src/lib.rs) |
| `bui-css`    | CSS Syntax L3 parser + Selectors L4 subset. | [bui-css/src/lib.rs](../crates/bui-css/src/lib.rs) |
| `bui-style`  | Cascade, computed values, UA stylesheet. | [bui-style/src/lib.rs](../crates/bui-style/src/lib.rs) |
| `bui-layout` | Block / inline / flex / grid layout, paint pass. | [bui-layout/src/lib.rs](../crates/bui-layout/src/lib.rs) |
| `bui-paint`  | Renderer-agnostic `DisplayList` + `PaintCommand` types. | [bui-paint/src/lib.rs](../crates/bui-paint/src/lib.rs) |
| `bui-gpu`    | wgpu + vello compositor. | [bui-gpu/src/lib.rs](../crates/bui-gpu/src/lib.rs) |
| `bui-shell`  | winit `ApplicationHandler` that pumps events. | [bui-shell/src/lib.rs](../crates/bui-shell/src/lib.rs) |
| `bui-js`     | Inline-script eval through Zinc; DOM event dispatch. | [bui-js/src/lib.rs](../crates/bui-js/src/lib.rs) |
| `bui-text`   | Font lookup, OpenType, glyph metrics + raster. | [bui-text/src/lib.rs](../crates/bui-text/src/lib.rs) |
| `bui-image`  | Hand-rolled Deflate + PNG decoder; JPEG / GIF / WebP deferred. | [bui-image/src/lib.rs](../crates/bui-image/src/lib.rs) |
| `bui`        | The binary that ties it all together. | [bui/src/main.rs](../crates/bui/src/main.rs) |

## The chrome

The chrome (the part *around* a rendered web page) is the
"IDE-Pane" design: a left sidebar with tabs grouped by hostname,
a slim top URL bar, a persistent dev-dock at the bottom (XHR /
Console / Source), and a status line.
It's not its own crate yet — every chrome paint primitive lives in
`bui/src/main.rs` and reaches into `bui-paint` for `FillRect`,
`FillRoundedRect`, `Text`, and `Image`. Major surfaces:

| Surface | Coords | Owner fn |
|---|---|---|
| Top bar    | `0..w × 0..44`              | `paint_chrome` |
| Sidebar    | `0..240 × 0..h`             | `paint_sidebar` |
| Viewport   | `sidebar..w × top..(h-status-dock)` | inline in `build_scene` |
| Dev-dock   | `sidebar..w × (h-status-dock)..(h-status)` | `paint_dock` |
| Status     | `0..w × (h-22)..h`          | `paint_status` |

See [`docs/dev-dock.md`](dev-dock.md) for what the dev-dock shows
and [`docs/keybindings.md`](keybindings.md) for the shortcut list.

## Deliberate deferrals

These are the lines that don't say "we ran out of time" — they
say "this is a separate kind of project from a browser":

- **TLS** (`rustls`). Hand-rolling cryptography is a different
  project; one bug means silent data leaks.
- **Async runtime** (`tokio`). Writing your own epoll/kqueue/IOCP
  layer is a year of orthogonal work.
- **Window + GPU surface** (`winit`, `wgpu`). The platform layer
  isn't the fun part to write yourself.
- **Compute shaders for 2D** (`vello`). We hand-roll the display-list
  commands, but the GPU pipeline that consumes them is `vello`'s job.

Hand-rolled, on the other hand: HTML parser, CSS parser, selectors,
cascade, DOM, layout, line breaking, HTTP/1.1, cookie store, event
dispatch, the entire chrome.
