# Contributing

Copper is a hobby project, but PRs and issues are welcome. The guide
below covers the conventions that make the codebase readable and
keeps CI green.

## Setup

```bash
git clone <your-fork>
cd browser-ui
cargo build --release          # first build is slow (wgpu + vello + tokio + rustls)
cargo run --release --bin copper
```

You need Rust 1.85+ (Edition 2024). The CI image uses `stable`.

### Platform notes

- **macOS**: primary platform; everything is verified here.
- **Linux**: CI builds it; manual testing is light. You'll need
  X11/Wayland dev headers (`libx11-dev libxkbcommon-dev libxcb1-dev
  libwayland-dev` on Debian/Ubuntu).
- **Windows**: not currently configured — winit needs a different
  feature subset.

## Project layout

See [`docs/architecture.md`](architecture.md) for the crate graph.
TL;DR: dependencies flow strictly bottom-up, lower crates don't
know about higher ones, and the binary in `bui/` is the only crate
that ties chrome + engine together.

## Workflow

```bash
# Run the full test suite.
cargo test --workspace

# Format check.
cargo fmt --all -- --check

# Clippy (CI runs this with -D warnings; treat warnings as errors).
cargo clippy --workspace --all-targets -- -D warnings

# Run the binary in debug mode (with backtraces).
RUST_BACKTRACE=1 cargo run --bin copper -- render https://en.wikipedia.org/wiki/Cat
```

There's also `tools/wiki_header_snapshot.sh` — a manual layout-
asserting script you can run after touching layout-related code to
catch regressions on Wikipedia's header.

## Code style

The codebase prefers prose-style comments that explain *why* a
piece of code looks the way it does, especially when it diverges
from the obvious approach. Pattern:

```rust
// CSS Flexbox §4: a flex item establishes a new block formatting
// context. We don't implement that yet, so when a Wikipedia
// `<span style="float:left">` lives inside `.mw-logo` (a flex
// item), the float escapes into the outer BFC and lands on top of
// the search box. Strip the float so the span stays inside.
.mw-logo-container { float: none !important }
```

A few conventions that recur:

- **No emojis** unless the design calls for them.
- **No backwards-compat shims** — internal callers can be updated
  in one PR. Pull legacy paths out completely.
- **No TODO/XXX/FIXME comments without context** — if the spot is
  worth marking, write what's needed to fix it.
- **Tests live next to the code** in `mod tests {}` blocks.
- **One commit per logical change**. Use HEREDOC commit messages
  so the body wraps cleanly.

## Adding a CSS property / selector

1. Parse it in `crates/bui-css/src/parser.rs` or
   `crates/bui-css/src/selector.rs`.
2. Add the value type to `crates/bui-style/src/values.rs`.
3. Apply it in `crates/bui-style/src/lib.rs` (cascade) and
   `crates/bui-layout/src/lib.rs` (layout/paint).
4. Write a unit test that builds a 2–5-line HTML fixture, calls
   `bui_layout::build`/`layout`/`paint`, and asserts the
   expected frame or paint command.

## Adding a chrome feature

The chrome lives entirely in `crates/bui/src/main.rs` (yes, it's
a big file — sorry). New surfaces follow this pattern:

1. Add the geometry constants near the top of `main.rs` (with the
   existing palette/layout block).
2. Write a `paint_<surface>` function that takes the window
   dimensions, the cursor position, and whatever state it reads
   (clone out of `BrowserState` before calling — don't hold the
   lock across paint).
3. Call it from `build_scene` after the page paint shift but
   before the status line.
4. Add hit-test branches at the top of `handle_click` and
   `cursor_for` (in that order: more specific surfaces first).
5. Wire a keybinding in `handle_key` if it's toggleable.

## Releases

Releases are triggered by pushing a `v*` tag:
`.github/workflows/release.yml` builds macOS Intel,
macOS Apple Silicon, and Linux x86_64 binaries, packages each as
a tarball with a SHA-256 checksum, and attaches them to a
generated GitHub Release.

There is no push/PR CI workflow — run `cargo test --workspace`,
`cargo fmt --all -- --check`, and `cargo clippy` locally before
submitting a PR.

## License

MIT, see [LICENSE](../LICENSE). By contributing you agree your
patch is also MIT-licensed.
