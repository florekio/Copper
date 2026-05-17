//! Copper — a from-scratch Rust browser, hosted on the Zinc JS engine.
//!
//! Modes:
//!   copper                  — open the window with a fresh tab on the home page
//!   copper render <url>     — open window with the URL as the first tab
//!   copper <https-url>      — fetch URL, dump body to stdout, exit
//!   copper fetch <url>      — same as above, explicit subcommand form
//!   copper --parse <url>    — fetch + parse, dump DOM tree
//!   copper parse <file>     — parse local HTML file, dump DOM tree
//!   copper --headers <url>  — also print response status + headers to stderr
//!
//! Tabs (Chrome hotkeys, ⌘ on macOS = super):
//!     ⌘T          new tab (loads HOME_URL)
//!     ⌘W          close active tab (last tab → quit)
//!     ⌘1..⌘8      jump to tab N
//!     ⌘9          jump to last tab
//!     ⌃⇥          next tab (wraps)
//!     ⌃⇧⇥         previous tab (wraps)
//!     ⌘R          reload active tab
//!     ⌘[          back   (uses per-tab history)
//!     ⌘]          forward
//!     ⌘⇧T         reopen most recently closed tab

mod address_input;
mod start_page;

use std::io::Write;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, OnceLock};

use address_input::{AddressInput, Step};
use bui_layout::LayoutBox;
use bui_net::{Client, Url};
use bui_paint::{Color, DisplayList, PaintCommand, Rect};
use bui_shell::{App, CursorIcon, Key, KeyPress, Viewport};

// ----- Copper paper palette (see design/Design Plan.html §03) -----
// Warm paper background, dark ink, single copper accent. The names match
// the design doc's CSS custom properties so audit-by-eye against the
// mockups is straightforward.
const PAPER: Color = Color::rgb(0xFA, 0xF7, 0xF0);
const PAPER_2: Color = Color::rgb(0xF0, 0xEB, 0xDE);
const INK: Color = Color::rgb(0x1A, 0x18, 0x15);
const INK_2: Color = Color::rgb(0x4A, 0x46, 0x3F);
const INK_3: Color = Color::rgb(0x7A, 0x74, 0x68);
const RULE: Color = Color::rgb(0xD8, 0xD1, 0xBF);
const COPPER: Color = Color::rgb(0xC9, 0x64, 0x42);
const COPPER_DEEP: Color = Color::rgb(0x8A, 0x3F, 0x24);
const COPPER_SOFT: Color = Color::rgb(0xF3, 0xDC, 0xCF);

// ----- Chrome surface colors (mapped to palette tokens) -----
const BG: Color = PAPER;
const VIEWPORT_BG: Color = Color::WHITE;
const SIDEBAR_BG: Color = PAPER_2;
const SIDEBAR_RULE: Color = RULE;
const TOP_BAR_BG: Color = PAPER;
const TOP_BAR_RULE: Color = RULE;
const DOCK_BG: Color = PAPER;
const DOCK_RULE: Color = RULE;
const STATUS_BG: Color = PAPER_2;
const STATUS_INK: Color = INK_2;
const ADDR_BG: Color = Color::WHITE;
const URL_TEXT: Color = INK_2;
const BORDER: Color = RULE;

// ----- Legacy tab-strip colors (Phase 1 moves tabs to the sidebar; until
//       then the existing tabs-on-top renderer reads these). Wire them
//       to the palette so we don't have a visual jump while migrating.
const TAB_STRIP_BG: Color = PAPER_2;
const TAB_INACTIVE: Color = PAPER;
const TAB_INACTIVE_HOVER: Color = COPPER_SOFT;
const TAB_ACTIVE: Color = Color::WHITE;
const TAB_HOVER: Color = COPPER_SOFT;
const CLOSE_X_HOVER_BG: Color = COPPER;
const CLOSE_X_HOVER_FG: Color = Color::WHITE;
const CHROME_BG: Color = PAPER;
const TAB_TITLE: Color = INK;
const TAB_TITLE_INACTIVE: Color = INK_2;
const CLOSE_X: Color = INK_3;

// ----- IDE-Pane layout constants (design §03 geometry, §04 inventory) -----
// The window splits into:
//
//      ┌────────────────── TOP_BAR (44 px) ──────────────────┐
//      │ SIDEBAR │           VIEWPORT                          │
//      │ (240px) │ ─── DOCK (~180 px, toggle ⌘J) ───────────── │
//      ├─────────┴───────────────────────────────────────────┤
//      │                    STATUS (22 px)                    │
//      └─────────────────────────────────────────────────────┘
//
// All five surfaces are paint targets the chrome renderer slices the
// window into. Heights collapse to 0 when toggled off.
const TOP_BAR_HEIGHT: f32 = 44.0;
const SIDEBAR_WIDTH: f32 = 240.0;
const DOCK_HEIGHT: f32 = 180.0;
const STATUS_HEIGHT: f32 = 22.0;

// In the IDE-Pane shell the top strip is just nav + URL — tabs live in
// the sidebar. CHROME_HEIGHT is kept as an alias so the page paint
// shift (`dy = CHROME_HEIGHT - scroll_y`) doesn't need rewiring.
const ADDR_BAR_HEIGHT: f32 = TOP_BAR_HEIGHT;
const CHROME_HEIGHT: f32 = TOP_BAR_HEIGHT;
const ADDR_INSET: f32 = 16.0;
// Vestigial — the legacy `paint_chrome` referenced this when tabs were
// at the top of the window. Kept (= 0) so any unmoved test code linking
// against it still builds; remove once those sites are cleaned up.
const TAB_STRIP_HEIGHT: f32 = 0.0;

// Nav buttons (back / forward / reload) to the left of the address pill.
const NAV_BTN_SIZE: f32 = 28.0;
const NAV_BTN_GAP: f32 = 4.0;
const NAV_BTN_COUNT: usize = 3;
const NAV_BTN_AREA_WIDTH: f32 =
    NAV_BTN_COUNT as f32 * NAV_BTN_SIZE + (NAV_BTN_COUNT as f32 - 1.0) * NAV_BTN_GAP;
const NAV_BTN_AFTER_GAP: f32 = 8.0;
const ADDR_BG_HEIGHT: f32 = 32.0;
const VIEWPORT_PADDING: f32 = 16.0;
const URL_FONT_SIZE: f32 = 14.0;
const TAB_TITLE_SIZE: f32 = 13.0;
const NEW_TAB_BTN_WIDTH: f32 = 36.0;
const TAB_MIN_WIDTH: f32 = 140.0;
const TAB_MAX_WIDTH: f32 = 240.0;
const TAB_GAP: f32 = 1.0;
const TAB_TOP_PAD: f32 = 4.0;
const TAB_CORNER_RADIUS: f32 = 8.0;
const TAB_INACTIVE_CORNER_RADIUS: f32 = 6.0;
const ADDR_PILL_RADIUS: f32 = 16.0;
const CLOSE_BTN_WIDTH: f32 = 28.0;
const CLOSE_BTN_INNER: f32 = 18.0;

/// Leading inset for the tab strip on macOS so the traffic-light buttons
/// (close / minimize / zoom) at top-left don't overlap with tabs. Big Sur+
/// places them roughly at x ∈ [10, 70], y ∈ [4, 22]. We give them a 78 px
/// reserve. On other platforms the strip starts at x=0.
#[cfg(target_os = "macos")]
const TAB_STRIP_LEADING: f32 = 78.0;
#[cfg(not(target_os = "macos"))]
const TAB_STRIP_LEADING: f32 = 0.0;

const HOME_URL: &str = "copper://start";

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut show_headers = false;
    let mut parse_html = false;
    let mut layout_grep: Option<String> = None;
    let mut layout_width: f32 = 1400.0;
    let mut positional: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let arg = raw[i].as_str();
        match arg {
            "--headers" | "-i" => show_headers = true,
            "--parse" => parse_html = true,
            "--grep" => {
                i += 1;
                if let Some(g) = raw.get(i) {
                    layout_grep = Some(g.clone());
                }
            }
            "--width" => {
                i += 1;
                if let Some(w) = raw.get(i).and_then(|s| s.parse::<f32>().ok()) {
                    layout_width = w;
                }
            }
            other => positional.push(other),
        }
        i += 1;
    }

    match positional.as_slice() {
        [] => run_browser(HOME_URL),
        ["render", url] => run_browser(url),
        ["fetch", url] => run_fetch(url, show_headers, parse_html),
        ["layout", url] => run_layout_debug(url, layout_width, layout_grep.as_deref()),
        [url] if url.starts_with("http://") || url.starts_with("https://") => {
            run_fetch(url, show_headers, parse_html)
        }
        ["parse", path] => run_parse_file(path),
        _ => {
            eprintln!(
                "usage:\n  copper                  open window\n  copper render <url>     fetch + render in window\n  copper <https-url>      fetch URL, dump body\n  copper --parse <url>    fetch URL, dump DOM tree\n  copper parse <file>     parse local file, dump DOM tree\n  copper --headers <url>  print response headers to stderr\n  copper layout <url>     fetch + build layout tree, dump per-box frames\n     [--width N]          set viewport width (default 1400)\n     [--grep TOKEN]       only show boxes whose element id/class contains TOKEN (and ancestors)"
            );
            ExitCode::from(2)
        }
    }
}

// ---- shared runtime ----

static SHARED_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static SHARED_CLIENT: OnceLock<Client> = OnceLock::new();

fn shared_runtime() -> &'static tokio::runtime::Runtime {
    SHARED_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime")
    })
}

fn shared_client() -> &'static Client {
    SHARED_CLIENT.get_or_init(|| {
        let client = Client::new();
        seed_google_consent(&client);
        client
    })
}

/// Pre-seed the cookie jar with Google's consent cookies so the
/// modern consent gate (`https://consent.google.com/ml?...`) is
/// skipped entirely.
///
/// Why this is necessary: when a user types a query on the
/// google.com homepage and presses Enter, our chrome submits the
/// form as a `GET` to `/search?q=…`. Without a CONSENT cookie
/// Google's edge replies with a 302 to a consent page whose
/// "Accept" form is POST-only. Our chrome only knows how to
/// submit `GET` forms today, so the user clicks Accept and lands
/// on `/save?...` via GET, which returns HTTP 405 "Method Not
/// Allowed". Seeding the cookies bypasses that flow — every
/// /search request from here on lands directly on the (`gbv=1`)
/// basic-HTML results page.
///
/// The two cookie names + value shapes are what real Chrome /
/// Firefox set after the user accepts. The expiry is a far-future
/// date so we don't need to refresh; the path is `/` and the
/// domain is `.google.com` so every subdomain inherits.
fn seed_google_consent(client: &Client) {
    let url = match Url::parse("https://www.google.com/") {
        Ok(u) => u,
        Err(_) => return,
    };
    let jar = client.jar();
    if let Ok(mut jar) = jar.lock() {
        jar.store(
            "CONSENT=YES+cb.20210720-07-p0.en+FX+667; \
             Domain=.google.com; Path=/; Expires=Thu, 31 Dec 2099 23:59:59 GMT",
            &url,
        );
        jar.store(
            "SOCS=CAESHAgBEhJnd3NfMjAyMzA0MTQtMF9SQzIaAmRlIAEaBgiAuPaiBg; \
             Domain=.google.com; Path=/; Expires=Thu, 31 Dec 2099 23:59:59 GMT",
            &url,
        );
    }
}

// ---- non-window modes ----

fn run_parse_file(path: &str) -> ExitCode {
    let html = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let doc = bui_html::parse(&html);
    print!("{}", doc.pretty_print());
    ExitCode::SUCCESS
}

fn run_fetch(url_str: &str, show_headers: bool, parse_html: bool) -> ExitCode {
    let url = match Url::parse(url_str) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("invalid URL: {e}");
            return ExitCode::from(2);
        }
    };
    match shared_runtime().block_on(shared_client().get(&url)) {
        Ok(resp) => {
            if show_headers {
                eprintln!("HTTP/1.1 {} {}", resp.status, resp.reason);
                for (k, v) in &resp.headers {
                    eprintln!("{k}: {v}");
                }
                eprintln!();
            }
            if parse_html {
                let html = std::str::from_utf8(&resp.body).unwrap_or_default();
                let doc = bui_html::parse(html);
                print!("{}", doc.pretty_print());
            } else {
                std::io::stdout().write_all(&resp.body).ok();
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("fetch error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Layout-debug CLI: fetch the URL, build the layout tree at the
/// given viewport width, and dump each LayoutBox to stdout with a
/// depth indent + element tag/id/class + computed display + frame.
/// When `grep` is set, only print boxes whose element id or class
/// contains the substring, plus their ancestor chain (so frame y
/// pin-points are visible in context).
fn run_layout_debug(url_str: &str, viewport_w: f32, grep: Option<&str>) -> ExitCode {
    let url = match Url::parse(url_str) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("invalid URL: {e}");
            return ExitCode::from(2);
        }
    };
    let tab = match TabState::fetch(&url) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("layout-debug fetch failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    bui_style::set_viewport(viewport_w, 900.0);
    let dlocked = tab.doc.lock().unwrap();
    let mut bx = bui_layout::build_with_images(
        &dlocked,
        &tab.style,
        &tab.images,
        &tab.svgs,
        tab.body_node,
    );
    drop(dlocked);
    bui_layout::layout(&mut bx, 0.0, 0.0, viewport_w);

    // First pass: collect node→entry. Second pass: walk + emit
    // matching this node OR its ancestors of a matching node.
    let mut lines: Vec<(usize, String, bool)> = Vec::new();
    fn walk(
        bx: &bui_layout::LayoutBox,
        doc: &bui_dom::Document,
        depth: usize,
        grep: Option<&str>,
        out: &mut Vec<(usize, String, bool)>,
    ) -> bool {
        let display = format!("{:?}", bx.style.display);
        let label = match bx.node.and_then(|n| doc.element(n)) {
            Some(e) => {
                let id = e.get_attr("id").map(|s| format!("#{s}")).unwrap_or_default();
                let class = e
                    .get_attr("class")
                    .map(|s| format!(".{}", s.split_whitespace().next().unwrap_or("")))
                    .unwrap_or_default();
                format!("{}{}{}", e.name, id, class)
            }
            None => format!("{:?}", bx.kind).split('(').next().unwrap_or("").to_string(),
        };
        let frame = bx.frame;
        let fs = bx.style.font_size;
        let lh = bx.style.line_height;
        let fl = match bx.style.float {
            bui_style::Float::Left => " float=L",
            bui_style::Float::Right => " float=R",
            bui_style::Float::None => "",
        };
        let pos = match bx.style.position {
            bui_style::Position::Static => "",
            bui_style::Position::Relative => " pos=rel",
            bui_style::Position::Absolute => " pos=abs",
            bui_style::Position::Fixed => " pos=fix",
            bui_style::Position::Sticky => " pos=sticky",
        };
        let ov = |o| match o {
            bui_style::Overflow::Visible => "vis",
            bui_style::Overflow::Hidden => "hid",
            bui_style::Overflow::Scroll => "scr",
            bui_style::Overflow::Auto => "aut",
            bui_style::Overflow::Clip => "clp",
        };
        let ovs = if !matches!(bx.style.overflow_x, bui_style::Overflow::Visible)
            || !matches!(bx.style.overflow_y, bui_style::Overflow::Visible)
        {
            format!(" ov={}/{}", ov(bx.style.overflow_x), ov(bx.style.overflow_y))
        } else {
            String::new()
        };
        let op = if bx.style.opacity < 1.0 {
            format!(" op={:.2}", bx.style.opacity)
        } else {
            String::new()
        };
        let vis = match bx.style.visibility {
            bui_style::Visibility::Visible => "",
            bui_style::Visibility::Hidden => " vis=hid",
            bui_style::Visibility::Collapse => " vis=col",
        };
        let line = format!(
            "[{:>5.0},{:>5.0} {:>6.0}×{:>4.0}] {:<10} {} (fs={:.0} lh={:.2}{}{}{}{}{})",
            frame.x, frame.y, frame.width, frame.height, display, label, fs, lh, fl, pos, ovs, op, vis
        );
        let matches = grep
            .map(|g| {
                bx.node
                    .and_then(|n| doc.element(n))
                    .map(|e| {
                        e.get_attr("id").map_or(false, |s| s.contains(g))
                            || e.get_attr("class").map_or(false, |s| s.contains(g))
                            || e.name.contains(g)
                    })
                    .unwrap_or(false)
            })
            .unwrap_or(true);
        let idx = out.len();
        out.push((depth, line, matches));
        // Surface inline content the layout pass left in `bx.lines`
        // (block boxes' anonymous children turn into line boxes here).
        // Each LineBox prints as a single bracketed entry under its
        // owning box; line items inline below it summarise the run.
        for line in &bx.lines {
            let preview: String = line.items.iter().take(3).map(|it| match it {
                bui_layout::LineItem::Text(r) => {
                    let t: String = r.text.chars().take(40).collect();
                    format!("\"{}\"", t)
                }
                bui_layout::LineItem::Image { .. } => "<img>".to_string(),
                bui_layout::LineItem::Control { .. } => "<ctrl>".to_string(),
                bui_layout::LineItem::Svg { .. } => "<svg>".to_string(),
                bui_layout::LineItem::InlineBlock { .. } => "<inline-block>".to_string(),
            }).collect::<Vec<_>>().join(" ");
            let line_str = format!(
                "[{:>5.0},{:>5.0} {:>6.0}×{:>4.0}] Line       {}",
                line.frame.x, line.frame.y, line.frame.width, line.frame.height, preview
            );
            out.push((depth + 1, line_str, matches));
        }
        let mut any_descendant_matches = false;
        for child in &bx.children {
            if walk(child, doc, depth + 1, grep, out) {
                any_descendant_matches = true;
            }
        }
        // Ancestor visibility: if any descendant matched, this box's
        // line should also be marked so the chain prints.
        if any_descendant_matches {
            out[idx].2 = true;
        }
        matches || any_descendant_matches
    }
    walk(&bx, &tab.doc.lock().unwrap(), 0, grep, &mut lines);

    for (depth, line, visible) in &lines {
        if grep.is_some() && !*visible {
            continue;
        }
        println!("{}{}", "  ".repeat(*depth), line);
    }
    ExitCode::SUCCESS
}

/// Walk the document in tree order, picking up author stylesheets from
/// both `<style>` blocks (parsed inline) and `<link rel="stylesheet">`
/// elements (fetched + parsed). Tree order matters for the cascade:
/// later sheets override earlier ones at equal specificity.
fn collect_author_stylesheets(
    base: &Url,
    doc: &bui_dom::Document,
) -> Vec<bui_css::Stylesheet> {
    fn has_noscript_ancestor(doc: &bui_dom::Document, nid: bui_dom::NodeId) -> bool {
        let mut cur = doc.node(nid).parent;
        while let Some(p) = cur {
            if let Some(e) = doc.element(p) {
                if e.name == "noscript" {
                    return true;
                }
            }
            cur = doc.node(p).parent;
        }
        false
    }

    // Coarse check for whether a `<link media="...">` value should
    // apply to screen rendering. Recognises the common shapes:
    //   - "all"          → match
    //   - "screen"       → match
    //   - "print"        → no
    //   - "screen and …" → match
    //   - "(prefers-…)"  → match (treat unguarded media query as screen)
    //   - "not print"    → match
    //   - missing/empty  → match (caller already handled empty)
    // Comma-separated lists match if ANY entry matches.
    fn media_matches_screen(media: &str) -> bool {
        for clause in media.split(',') {
            let c = clause.trim().to_ascii_lowercase();
            if c.is_empty() || c == "all" {
                return true;
            }
            // "not print" / "not screen": invert.
            if let Some(rest) = c.strip_prefix("not ") {
                let inner = rest.trim();
                if inner == "print" || inner == "speech" {
                    return true;
                }
                if inner == "screen" || inner == "all" {
                    continue;
                }
            }
            // "print", "speech", "tty" etc. don't apply to screen.
            if c == "print" || c == "speech" || c == "tty" || c == "tv"
                || c == "embossed" || c == "handheld" || c == "projection"
                || c == "braille"
            {
                continue;
            }
            // "screen", "screen and …", or pure media-feature
            // expressions like "(min-width: 600px)" — treat as screen-
            // applicable.
            return true;
        }
        false
    }
    let mut sheets = Vec::new();
    let internal = base.is_internal();
    for nid in doc.descendants(doc.root) {
        let Some(elem) = doc.element(nid) else {
            continue;
        };
        match elem.name.as_str() {
            "style" => {
                // Skip <style> inside <noscript>: those carry the
                // page's no-JS fallback rules (often a "hide
                // everything" nuke like `table,div,span,p{display:none}`
                // — google.com's homepage does exactly this). We
                // render as if scripting is enabled, so the noscript
                // sheet should be ignored.
                if has_noscript_ancestor(doc, nid) {
                    continue;
                }
                // <style media="print"> only applies to printing — same
                // rule as for <link>.
                let media = elem.get_attr("media").unwrap_or("").trim();
                if !media.is_empty() && !media_matches_screen(media) {
                    continue;
                }
                let mut text = String::new();
                let mut child = doc.node(nid).first_child;
                while let Some(c) = child {
                    if let bui_dom::NodeKind::Text(t) = &doc.node(c).kind {
                        text.push_str(t);
                    }
                    child = doc.node(c).next_sibling;
                }
                if !text.trim().is_empty() {
                    let mut sheet = bui_css::Stylesheet::parse(&text);
                    if !internal {
                        inline_css_imports(base, &mut sheet);
                    }
                    sheets.push(sheet);
                }
            }
            "link" if !internal => {
                // Accept rel="stylesheet" and rel="stylesheet preload" etc.
                let rel = elem.get_attr("rel").unwrap_or("");
                let is_stylesheet = rel
                    .split_ascii_whitespace()
                    .any(|t| t.eq_ignore_ascii_case("stylesheet"));
                if !is_stylesheet {
                    continue;
                }
                // `media="..."` filters which media types the sheet
                // applies to. The default is "all"; any value that
                // explicitly excludes screen (e.g., "print" — Google's
                // og.asy.css imported with media="print" is a real
                // case in the wild) means we should skip the load.
                // We only do a coarse check — full Media Queries L4
                // evaluation isn't here yet — but recognising the
                // common print-only sheets prevents them from
                // overriding screen rules with their print-only
                // declarations (which override .gb_Q's display: none
                // on Google's homepage and break the chrome strip).
                let media = elem.get_attr("media").unwrap_or("").trim();
                if !media.is_empty() && !media_matches_screen(media) {
                    continue;
                }
                let Some(href) = elem.get_attr("href") else {
                    continue;
                };
                let url = match base.join(href) {
                    Ok(u) => u,
                    Err(e) => {
                        eprintln!("css {href:?}: bad href: {e}");
                        continue;
                    }
                };
                if let Some(sheet) = fetch_and_parse_stylesheet(&url) {
                    sheets.push(sheet);
                }
            }
            _ => {}
        }
    }
    sheets
}

/// True for Google's apex / `www` search domains across all locale
/// ccTLDs — `google.com`, `google.de`, `www.google.co.uk`,
/// `google.com.br`, …. Rejects subdomained Google products
/// (`scholar.google.com`, `news.google.com`, `maps.google.com`)
/// because those aren't search and don't honour `gbv=1`. Also
/// rejects lookalike hosts whose suffix labels are longer than
/// any real (e)TLD — `google.evilattacker.com` has a 12-char
/// label, so we drop it. Real Google suffixes top out at 4 chars
/// per label (`com`, `info`, `co.uk`, `com.br`, `com.au`, …).
fn is_google_search_host(host: &str) -> bool {
    let h = host.strip_prefix("www.").unwrap_or(host);
    let Some(rest) = h.strip_prefix("google.") else {
        return false;
    };
    let labels: Vec<&str> = rest.split('.').collect();
    // 1 or 2 labels, each non-empty, ASCII-alphanumeric, and short
    // (real (e)TLDs are 2-4 chars: "de", "com", "info", "co.uk").
    (labels.len() == 1 || labels.len() == 2)
        && labels.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 4
                && p.chars().all(|c| c.is_ascii_alphanumeric())
        })
}

/// No-op pass-through. Historically we rewrote
/// `google.<tld>/search?q=…` to append `&gbv=1` and land on
/// Google's basic-HTML results page. As of 2026 that endpoint
/// is dead — it serves the same `<noscript><meta refresh>`
/// "enable JS" shell as the unrewritten URL, just with a tiny
/// stub document and a meta-refresh to `/httpservice/retry/
/// enablejs`. Either way the user lands on a page whose only
/// visible content sits inside `<noscript>` (and is hidden by
/// our UA stylesheet because we *do* have a JS engine, just
/// not one that runs Google's Closure-library bundle).
///
/// Until we either shim enough Closure runtime to boot the
/// modern shell or route Google search through a different
/// backend, this stays a pass-through and the search-results
/// fetch falls through to an empty render. The form submit
/// path still works — the user lands on the real Google URL,
/// just with no visible results.
fn maybe_rewrite_google_search(url: &Url) -> Url {
    url.clone()
}

/// Fetch a CSS URL, parse it, and inline any `@import` rules in
/// place so the cascade sees a single flattened rule list.
fn fetch_and_parse_stylesheet(url: &Url) -> Option<bui_css::Stylesheet> {
    // 8-second timeout — Wikipedia loads many <link> CSS references;
    // a single slow / 429-stuck mirror can stall the whole page if
    // we wait indefinitely. Treat timeout as parse-error so the rest
    // of the cascade still applies.
    let started = std::time::Instant::now();
    let fetch = shared_runtime().block_on(async {
        tokio::time::timeout(
            std::time::Duration::from_secs(8),
            shared_client().get(url),
        )
        .await
    });
    let elapsed_ms = started.elapsed().as_millis() as u32;
    let resp = match fetch {
        Err(_) => {
            net_record("GET", url, 0, elapsed_ms, 0);
            eprintln!("css {url}: timed out after 8s, dropping");
            return None;
        }
        Ok(Err(e)) => {
            net_record("GET", url, 0, elapsed_ms, 0);
            eprintln!("css {url}: fetch failed: {e}");
            return None;
        }
        Ok(Ok(r)) => r,
    };
    net_record("GET", url, resp.status, elapsed_ms, resp.body.len());
    if !(200..300).contains(&resp.status) {
        eprintln!("css {url}: HTTP {} (skipping)", resp.status);
        return None;
    }
    let css = String::from_utf8_lossy(&resp.body);
    let mut sheet = bui_css::Stylesheet::parse(&css);
    inline_css_imports(url, &mut sheet);
    Some(sheet)
}

/// Replace every `@import url(...)` rule in `sheet` with the rules
/// of the imported stylesheet (recursively). Imports preserve source
/// order so the cascade priority stays intuitive. We track a small
/// recursion depth to avoid pathological circular imports DoS-ing
/// the page.
fn inline_css_imports(base: &Url, sheet: &mut bui_css::Stylesheet) {
    inline_imports_with_depth(base, sheet, 0, &mut std::collections::HashSet::new());
}

fn inline_imports_with_depth(
    base: &Url,
    sheet: &mut bui_css::Stylesheet,
    depth: usize,
    seen: &mut std::collections::HashSet<String>,
) {
    if depth > 8 {
        return;
    }
    let mut new_rules: Vec<bui_css::Rule> = Vec::with_capacity(sheet.rules.len());
    for rule in std::mem::take(&mut sheet.rules) {
        match rule {
            bui_css::Rule::At { ref name, ref prelude, .. } if name.eq_ignore_ascii_case("import") => {
                if let Some(href) = parse_import_href(prelude) {
                    let target = match base.join(&href) {
                        Ok(u) => u,
                        Err(_) => continue,
                    };
                    let key = target.to_string();
                    if !seen.insert(key) {
                        continue;
                    }
                    let resp = match shared_runtime().block_on(shared_client().get(&target)) {
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("css @import {target}: fetch failed: {e}");
                            continue;
                        }
                    };
                    let css = String::from_utf8_lossy(&resp.body);
                    let mut imported = bui_css::Stylesheet::parse(&css);
                    inline_imports_with_depth(&target, &mut imported, depth + 1, seen);
                    new_rules.extend(imported.rules);
                }
            }
            other => new_rules.push(other),
        }
    }
    sheet.rules = new_rules;
}

/// Pull the URL out of an `@import` prelude. CSS allows
///   `@import url("foo.css")` / `@import url(foo.css)`
///   `@import "foo.css"`
/// optionally followed by media queries (which we ignore — assume
/// the import always applies).
fn parse_import_href(prelude: &str) -> Option<String> {
    let trimmed = prelude.trim();
    // url(...) form.
    if let Some(start) = trimmed.to_ascii_lowercase().find("url(") {
        let after = &trimmed[start + 4..];
        let close = after.find(')')?;
        let inner = after[..close].trim();
        let unquoted = if (inner.starts_with('"') && inner.ends_with('"'))
            || (inner.starts_with('\'') && inner.ends_with('\''))
        {
            &inner[1..inner.len() - 1]
        } else {
            inner
        };
        return Some(unquoted.to_string());
    }
    // bare-string form.
    if (trimmed.starts_with('"') && trimmed.contains('"'))
        || (trimmed.starts_with('\'') && trimmed.contains('\''))
    {
        let q = trimmed.as_bytes()[0] as char;
        let rest = &trimmed[1..];
        if let Some(close) = rest.find(q) {
            return Some(rest[..close].to_string());
        }
    }
    None
}

/// Walk the document for `<img src="...">`, fetch each, decode it, push the
/// pixels into the global GPU upload queue, and return an
/// `ImageRegistry` keyed by NodeId so layout can give those nodes their
/// intrinsic size and a paint reference. Skips `<img>`s without src,
/// with relative paths that fail to resolve, with non-PNG bodies (until
/// JPEG ships), or with any fetch / decode failure.
fn preload_images(
    base: &Url,
    doc: &bui_dom::Document,
) -> (bui_layout::ImageRegistry, bui_layout::SvgRegistry) {
    let mut images = bui_layout::ImageRegistry::new();
    let mut svgs = bui_layout::SvgRegistry::new();
    if base.is_internal() {
        // copper:// pages don't load remote resources.
        return (images, svgs);
    }

    // Pass 1: collect all <img> nodes + their resolved URL. Build a
    // URL → Vec<NodeId> map so the same image referenced by multiple
    // <img> tags only triggers one network fetch.
    let mut by_url: std::collections::HashMap<String, Vec<bui_dom::NodeId>> =
        std::collections::HashMap::new();
    let mut url_for_key: std::collections::HashMap<String, Url> =
        std::collections::HashMap::new();
    for node in doc.descendants(doc.root) {
        let Some(elem) = doc.element(node) else { continue };
        if elem.name != "img" {
            continue;
        }
        let Some(src) = best_image_url(doc, node, elem) else { continue };
        let img_url = match base.join(&src) {
            Ok(u) => u,
            Err(_) => continue,
        };
        let key = img_url.to_string();
        by_url.entry(key.clone()).or_default().push(node);
        url_for_key.entry(key).or_insert(img_url);
    }
    if by_url.is_empty() {
        return (images, svgs);
    }
    let unique_urls: Vec<Url> = url_for_key.into_values().collect();

    // Pass 2: fetch all unique URLs concurrently, with a small
    // semaphore so a 80-image page doesn't slam Wikimedia all at once
    // (which is exactly what was triggering the 429 cliff before).
    let results: Vec<(String, Option<ImageResource>)> = shared_runtime().block_on(async {
        const MAX_INFLIGHT: usize = 6;
        let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT));
        let mut set = tokio::task::JoinSet::new();
        for url in unique_urls {
            let permits = permits.clone();
            set.spawn(async move {
                let _permit = permits.acquire_owned().await.ok();
                let res = fetch_image_resource_async(&url).await;
                (url.to_string(), res)
            });
        }
        let mut out = Vec::new();
        while let Some(joined) = set.join_next().await {
            if let Ok(pair) = joined {
                out.push(pair);
            }
        }
        out
    });

    // Pass 3: distribute results to every node that referenced the URL.
    for (key, res) in results {
        let Some(nodes) = by_url.get(&key) else { continue };
        match res {
            Some(ImageResource::Raster(image)) => {
                let entry = bui_layout::ImageEntry {
                    width: image.width as f32,
                    height: image.height as f32,
                    key: key.clone(),
                };
                for &n in nodes {
                    images.insert(n, entry.clone());
                }
                bui_gpu::enqueue_upload(key, image);
            }
            Some(ImageResource::Vector(entry)) => {
                for &n in nodes {
                    svgs.insert(n, entry.clone());
                }
            }
            None => {}
        }
    }
    (images, svgs)
}

enum ImageResource {
    Raster(bui_image::Image),
    Vector(bui_layout::SvgEntry),
}

/// Choose the URL for an `<img>`, considering siblings + own
/// `srcset`. Preference order:
///
///   1. If the img's parent is a `<picture>`, walk preceding
///      `<source>` siblings and use the first one whose `srcset`
///      yields a URL. That covers Wikipedia thumbs which wrap the
///      base img in a `<picture>` to deliver retina alternates.
///   2. The img's own `srcset` first descriptor (we don't pick by
///      DPR since we render at 1x).
///   3. The img's `src` attribute.
fn best_image_url(
    doc: &bui_dom::Document,
    img_node: bui_dom::NodeId,
    elem: &bui_dom::Element,
) -> Option<String> {
    if let Some(parent_id) = doc.node(img_node).parent {
        if let Some(parent) = doc.element(parent_id) {
            if parent.name == "picture" {
                let mut child = doc.node(parent_id).first_child;
                while let Some(c) = child {
                    if c == img_node {
                        break;
                    }
                    if let Some(e) = doc.element(c) {
                        if e.name == "source" {
                            if let Some(srcset) = e.get_attr("srcset") {
                                if let Some(u) = first_srcset_url(srcset) {
                                    return Some(u);
                                }
                            }
                        }
                    }
                    child = doc.node(c).next_sibling;
                }
            }
        }
    }
    if let Some(srcset) = elem.get_attr("srcset") {
        if let Some(u) = first_srcset_url(srcset) {
            return Some(u);
        }
    }
    elem.get_attr("src").map(|s| s.to_string())
}

/// Pull the leftmost URL out of a CSS-style srcset descriptor list:
/// `"url-1 1x, url-2 2x"` → `Some("url-1")`. Returns None for empty
/// / malformed input.
fn first_srcset_url(srcset: &str) -> Option<String> {
    let first = srcset.split(',').next()?.trim();
    let url = first.split_ascii_whitespace().next()?;
    if url.is_empty() {
        None
    } else {
        Some(url.to_string())
    }
}

/// Async fetch + decode. The sync `fetch_and_decode_image` is still
/// around for the background-image preload path, which runs outside
/// a Tokio context and can afford to block per-URL.
///
/// Multi-retry on 429: Wikimedia (and most CDNs that rate-limit
/// per-IP) emit `Retry-After: <seconds>` indicating exactly how long
/// to wait. We try up to four times in total with exponential backoff
/// (200ms / 500ms / 1500ms), upgrading the next delay if the server
/// hands us a larger Retry-After value. The 6-permit semaphore the
/// caller holds keeps retries from stampeding.
async fn fetch_image_resource_async(url: &Url) -> Option<ImageResource> {
    const BACKOFFS_MS: [u64; 3] = [200, 500, 1500];
    let started = std::time::Instant::now();
    let mut resp = shared_client().get(url).await.ok()?;
    for &base_ms in BACKOFFS_MS.iter() {
        if resp.status != 429 {
            break;
        }
        // Honour Retry-After when it's larger than our planned
        // backoff — the server told us when to come back. Cap at
        // 3 seconds so a bad header can't stall page load.
        let header_ms = retry_after_seconds(&resp).map(|s| (s as u64) * 1000);
        let delay_ms = match header_ms {
            Some(h) if h > base_ms => h.min(3000),
            _ => base_ms,
        };
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        if let Ok(r2) = shared_client().get(url).await {
            resp = r2;
        } else {
            return None;
        }
    }
    net_record(
        "GET",
        url,
        resp.status,
        started.elapsed().as_millis() as u32,
        resp.body.len(),
    );
    if !(200..300).contains(&resp.status) {
        eprintln!("image {url}: HTTP {} (skipping decode)", resp.status);
        return None;
    }
    let fmt = bui_image::detect_format(&resp.body);
    if matches!(fmt, bui_image::Format::Svg) {
        return parse_svg_bytes(&resp.body).map(ImageResource::Vector);
    }
    match bui_image::decode(&resp.body) {
        Ok(img) => Some(ImageResource::Raster(img)),
        Err(e) => {
            eprintln!("image {url}: decode failed: {e}");
            None
        }
    }
}

/// Parse a `Retry-After` header into seconds. CSS allows either an
/// integer-seconds delay or an HTTP-date; we only handle the integer
/// case (CDNs almost always send the simpler form, and waiting until
/// a wall-clock date is uncommon on rate-limit responses).
fn retry_after_seconds(resp: &bui_net::Response) -> Option<u32> {
    let raw = resp.header("retry-after")?.trim();
    raw.parse::<u32>().ok()
}

/// Parse SVG bytes into the shape list the inline-SVG paint path
/// already understands. We feed the bytes through the HTML5 parser —
/// it doesn't know SVG-the-XML-grammar but it does build the right
/// element tree for `<svg><path/></svg>` style markup, which is
/// exactly what `bui_layout::svg::parse_svg` walks.
fn parse_svg_bytes(bytes: &[u8]) -> Option<bui_layout::SvgEntry> {
    let text = std::str::from_utf8(bytes).ok()?;
    let doc = bui_html::parse(text);
    let svg_node = doc.descendants(doc.root).find(|n| {
        doc.element(*n).map(|e| e.name == "svg").unwrap_or(false)
    })?;
    bui_layout::svg::parse_svg(&doc, svg_node)
}

/// Walk the style tree, replace each `background-image` URL with its
/// fully-resolved form (so paint emits the same cache key the
/// compositor was given at upload), and pre-fetch each unique image
/// once. Sites that point at a non-existent / non-image URL just
/// silently miss the cache and paint without a background.
fn resolve_and_preload_background_images(base: &Url, style: &mut bui_style::StyleTree) {
    if base.is_internal() {
        return;
    }
    let mut to_fetch: Vec<Url> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for cv in style.values.values_mut() {
        let Some(raw) = cv.background_image.as_deref() else {
            continue;
        };
        match base.join(raw) {
            Ok(u) => {
                let key = u.to_string();
                if seen.insert(key.clone()) {
                    to_fetch.push(u);
                }
                cv.background_image = Some(key);
            }
            Err(_) => {
                cv.background_image = None;
            }
        }
    }
    for url in to_fetch {
        if let Some(image) = fetch_and_decode_image(&url) {
            bui_gpu::enqueue_upload(url.to_string(), image);
        }
    }
}

fn fetch_and_decode_image(url: &Url) -> Option<bui_image::Image> {
    let started = std::time::Instant::now();
    let resp = match shared_runtime().block_on(shared_client().get(url)) {
        Ok(r) => r,
        Err(e) => {
            net_record("GET", url, 0, started.elapsed().as_millis() as u32, 0);
            eprintln!("image {url}: fetch failed: {e}");
            return None;
        }
    };
    net_record(
        "GET",
        url,
        resp.status,
        started.elapsed().as_millis() as u32,
        resp.body.len(),
    );
    // Don't try to decode error responses — Wikimedia's CDN returns
    // 429 ("Too Many Requests") with an HTML body when we hammer it,
    // and previously that printed misleading "unknown image format"
    // errors on every retry.
    if !(200..300).contains(&resp.status) {
        eprintln!("image {url}: HTTP {} (skipping decode)", resp.status);
        return None;
    }
    if matches!(bui_image::detect_format(&resp.body), bui_image::Format::Svg) {
        return parse_svg_bytes(&resp.body).map(rasterize_svg_for_background);
    }
    match bui_image::decode(&resp.body) {
        Ok(img) => Some(img),
        Err(e) => {
            eprintln!("image {url}: decode failed: {e}");
            None
        }
    }
}

/// Rasterise an SVG into a small RGBA8 buffer suitable for the
/// background-image cache. Real browsers re-rasterise at the box's
/// painted size each time; we cache once at the SVG's intrinsic
/// dimensions (clamped to a reasonable icon range) and let the GPU
/// stretch from there. Quality at button sizes is fine; quality at
/// hero-image sizes would be poor — but background SVGs in the wild
/// are mostly icons.
fn rasterize_svg_for_background(entry: bui_layout::SvgEntry) -> bui_image::Image {
    // Clamp intrinsic dimensions to a sensible icon-cache range. SVGs
    // declared without a width/height pick up a 24x24 default in
    // parse_svg, which is exactly what we want here.
    let w = entry.width.clamp(8.0, 256.0).round() as u32;
    let h = entry.height.clamp(8.0, 256.0).round() as u32;
    let shapes: Vec<bui_paint::raster::FilledShape> = entry
        .shapes
        .iter()
        .filter_map(|s| s.fill.map(|fill| bui_paint::raster::FilledShape {
            segments: &s.segments,
            fill,
        }))
        .collect();
    let pixels = bui_paint::raster::rasterize(&shapes, entry.view_box, w, h);
    bui_image::Image {
        width: w,
        height: h,
        pixels,
    }
}

// ---- per-tab state ----

/// One HTTP fetch recorded for the dev-dock XHR panel. Captures
/// enough to render a single waterfall row: method, full URL, final
/// status, elapsed milliseconds, response byte count.
#[derive(Debug, Clone)]
struct NetEntry {
    method: String,
    url: String,
    status: u16,
    ms: u32,
    bytes: usize,
}

/// Per-fetch capture buffer. The page fetch (TabState::fetch) clears
/// this at entry, every fetch site appends as it completes, and the
/// outer call drains it into the new TabState's `net_log` after the
/// page is built. Lives in a global Mutex because the fetch sites
/// (collect_author_stylesheets, fetch_image_resource_async,
/// fetch_and_decode_image, preload_images) span the chrome and pre-
/// date the chrome ↔ engine boundary the design plan calls for.
static NET_CAPTURE: OnceLock<Mutex<Vec<NetEntry>>> = OnceLock::new();

fn net_capture() -> &'static Mutex<Vec<NetEntry>> {
    NET_CAPTURE.get_or_init(|| Mutex::new(Vec::new()))
}

fn net_record(method: &str, url: &Url, status: u16, ms: u32, bytes: usize) {
    if let Ok(mut log) = net_capture().lock() {
        log.push(NetEntry {
            method: method.to_string(),
            url: url.to_string(),
            status,
            ms,
            bytes,
        });
    }
}

fn drain_net_capture() -> Vec<NetEntry> {
    net_capture()
        .lock()
        .map(|mut g| std::mem::take(&mut *g))
        .unwrap_or_default()
}

fn clear_net_capture() {
    if let Ok(mut log) = net_capture().lock() {
        log.clear();
    }
}

struct TabState {
    title: String,
    url: Url,
    /// HTTP fetches recorded during this tab's load — drained from
    /// the global NET_CAPTURE at the end of `TabState::fetch`. Used
    /// by the dev-dock XHR tab.
    net_log: Vec<NetEntry>,
    /// JS console messages + style/parse warnings captured during
    /// fetch. Used by the dev-dock Console tab.
    console_log: Vec<String>,
    /// Raw HTML body (lossy UTF-8) for the dev-dock Source tab.
    /// Truncated to a sensible cap if the page is huge so we don't
    /// blow up the dock's text-run count.
    source_html: String,
    /// Shared with the bui-js bindings so JS mutations during
    /// inline-script execution land in the same Document the
    /// subsequent style + layout pass reads. Locked briefly per
    /// access; never held across user-input handling.
    doc: Arc<Mutex<bui_dom::Document>>,
    style: bui_style::StyleTree,
    body_node: bui_dom::NodeId,
    images: bui_layout::ImageRegistry,
    svgs: bui_layout::SvgRegistry,
    layout: Option<LayoutBox>,
    last_width: u32,
    /// URLs visited *before* the current page; pop to go back.
    history: Vec<Url>,
    /// URLs popped via "back"; pop to go forward.
    forward: Vec<Url>,
    scroll_y: f32,
    /// Per-`<input>` editable buffers, lazily populated on first focus.
    /// Reuses `AddressInput` from the chrome.
    page_inputs: std::collections::HashMap<bui_dom::NodeId, AddressInput>,
    /// The currently focused page input, if any. Mutually exclusive with
    /// the chrome's `address_input.focused`.
    focused_input: Option<bui_dom::NodeId>,
    /// Active drag-selection over page text. Both endpoints are in
    /// absolute layout coordinates (the same coords as text-run
    /// frames). `None` means no selection. While the user is
    /// actively dragging, `dragging` is true and `end` updates on
    /// each `on_drag` event; mouse-up freezes it.
    page_selection: Option<PageSelection>,
    /// Live JS engine for this page. `Some` after a successful
    /// fetch on a page with at least one binding install; user-
    /// input handlers dispatch `submit` / `click` events through
    /// it so JS-registered listeners observe the same engine
    /// state they set up during the initial script pass. Cleared
    /// on every navigation (each new page gets a fresh engine).
    js_ctx: Option<bui_js::JsContext>,
}

#[derive(Default, Debug)]
struct DispatchOutcome {
    default_prevented: bool,
    pending_nav: Option<String>,
}

/// Drag selection across page text. Coordinates are in absolute
/// layout pixels (i.e., the same coords as `LayoutBox::frame.x` /
/// `frame.y`), so we don't need to re-resolve on scroll. The paint
/// pass adds the chrome offset and subtracts scroll_y when drawing
/// the highlight rect.
#[derive(Debug, Clone, Copy)]
struct PageSelection {
    start: (f32, f32),
    end: (f32, f32),
    /// True while the user is still holding the mouse button. The
    /// drag handler keeps updating `end`; mouse-up flips this off so
    /// further `on_drag` events (driven by other reasons) don't
    /// extend a finished selection.
    dragging: bool,
}

impl TabState {
    fn fetch(url: &Url) -> Result<Self, String> {
        // Google's modern /search page is a JS-required shell:
        // without scripts the body is wiped by a `<noscript>`
        // stylesheet that hides every table/div/span/p, plus a
        // meta-refresh to a JS-only retry endpoint. Append
        // `gbv=1` (Google's "basic HTML" version) so we land on
        // the legacy results page that still works without JS.
        // Same trick for `/imghp` etc isn't worth it; only the
        // text-search submit path matters.
        let url = maybe_rewrite_google_search(url);
        let url = &url;
        // Fresh net-capture for this navigation; descendant fetches
        // (stylesheets, images) append into the same global buffer.
        clear_net_capture();
        let mut console_log: Vec<String> = Vec::new();
        let html = if url.is_internal() {
            start_page::html_for(&url.host)
                .ok_or_else(|| format!("no internal page for {url}"))?
                .to_string()
        } else {
            let started = std::time::Instant::now();
            let resp = shared_runtime()
                .block_on(shared_client().get(url))
                .map_err(|e| {
                    net_record("GET", url, 0, started.elapsed().as_millis() as u32, 0);
                    format!("{e}")
                })?;
            let ms = started.elapsed().as_millis() as u32;
            net_record("GET", url, resp.status, ms, resp.body.len());
            // Servers like google.com still send ISO-8859-1 / Windows-1252;
            // declare charset support in the Content-Type but emit non-UTF-8
            // bytes mid-stream. We use lossy decoding so a single weird byte
            // doesn't take down the whole navigation. A proper Content-Type
            // charset → decoder mapping is the right follow-up.
            String::from_utf8_lossy(&resp.body).into_owned()
        };
        let doc = Arc::new(Mutex::new(bui_html::parse(&html)));
        // Run inline <script> against a live, populated DOM. Bindings
        // can now mutate (setAttribute, appendChild, createElement,
        // classList, …) — the second tuple element is a `dirty`
        // hint signalling whether any binding actually changed the
        // tree. We always rebuild style + layout from `doc`
        // immediately below so the hint is informational only at
        // fetch time; it'll be load-bearing once timers / events
        // fire scripts AFTER the initial layout.
        // Synchronous fetcher backing JS-side `fetch(url, opts)`.
        // Resolves the URL against the page's base (so a script
        // calling `fetch('/api/x')` lands on the same host), then
        // does a blocking GET via the shared client. Recorded into
        // NET_CAPTURE so the dev-dock XHR tab shows the request.
        let base_url = url.clone();
        let fetcher: bui_js::Fetcher = std::sync::Arc::new(move |raw: &str| {
            let resolved = base_url.join(raw).ok()?;
            let started = std::time::Instant::now();
            let resp = shared_runtime()
                .block_on(shared_client().get(&resolved))
                .ok()?;
            let ms = started.elapsed().as_millis() as u32;
            net_record("GET", &resolved, resp.status, ms, resp.body.len());
            Some(bui_js::FetchResponse {
                status: resp.status,
                url: resolved.to_string(),
                body: resp.body,
            })
        });
        let (mut js_ctx, outcomes) = bui_js::JsContext::install_and_run(
            doc.clone(),
            url.to_string(),
            Some(fetcher),
        );
        for outcome in outcomes {
            for line in &outcome.output {
                eprintln!("[js] {line}");
                console_log.push(line.clone());
            }
        }
        // If a script set `window.location.href`, resolve it against
        // the current URL and follow the redirect by recursing once
        // into `TabState::fetch`. One-shot: the recursion's own
        // pending-nav drain handles a chain, capped only by the
        // server / cookie state (we don't loop forever in this
        // function — recursion bounds itself by call depth).
        if let Some(target) = js_ctx.take_pending_navigation() {
            if let Ok(next_url) = url.join(&target) {
                if next_url.to_string() != url.to_string() {
                    return TabState::fetch(&next_url);
                }
            }
        }
        // Clear the dirty flag too — bindings tripped it during
        // the script pass, but the layout we're about to build
        // already reflects every mutation. The next dispatch
        // (from user input) will set it again if a handler
        // mutates the DOM and the orchestrator will re-layout.
        let _ = js_ctx.take_dirty();
        let dlocked = doc.lock().unwrap();
        let sheets = collect_author_stylesheets(url, &dlocked);
        let mut style = bui_style::style_document(&dlocked, &sheets);
        // Resolve and pre-fetch any author-declared background-image
        // URLs. This rewrites style.values in place so paint can use
        // each background_image string directly as the upload-cache
        // key.
        resolve_and_preload_background_images(url, &mut style);
        let body_node = dlocked
            .descendants(dlocked.root)
            .find(|id| {
                dlocked.element(*id)
                    .map(|e| e.name == "body")
                    .unwrap_or(false)
            })
            .unwrap_or(dlocked.root);
        let title = dlocked
            .descendants(dlocked.root)
            .find(|id| {
                dlocked.element(*id)
                    .map(|e| e.name == "title")
                    .unwrap_or(false)
            })
            .and_then(|id| {
                let t = dlocked.node(id).first_child?;
                if let bui_dom::NodeKind::Text(s) = &dlocked.node(t).kind {
                    Some(s.trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| url.host.clone());
        let (images, svgs) = preload_images(&url, &dlocked);

        // HTML autofocus: if any <input>/<textarea> on the page
        // declares the autofocus attribute, focus it on first
        // render so the user can start typing immediately. This
        // matches what real browsers do and lights up Google's
        // search box (`<textarea name="q" autofocus="">`) without
        // requiring a click. Only the FIRST autofocus element wins.
        let mut autofocus_target: Option<bui_dom::NodeId> = None;
        for nid in dlocked.descendants(dlocked.root) {
            let Some(elem) = dlocked.element(nid) else { continue };
            if !matches!(elem.name.as_str(), "input" | "textarea") {
                continue;
            }
            if elem.get_attr("autofocus").is_some() {
                autofocus_target = Some(nid);
                break;
            }
        }
        let mut page_inputs = std::collections::HashMap::new();
        if let Some(nid) = autofocus_target {
            let initial = if let Some(elem) = dlocked.element(nid) {
                if elem.name == "textarea" {
                    let mut text = String::new();
                    let mut child = dlocked.node(nid).first_child;
                    while let Some(c) = child {
                        if let bui_dom::NodeKind::Text(t) = &dlocked.node(c).kind {
                            text.push_str(t);
                        }
                        child = dlocked.node(c).next_sibling;
                    }
                    text.trim().to_string()
                } else {
                    elem.get_attr("value").unwrap_or("").to_string()
                }
            } else {
                String::new()
            };
            let mut input = AddressInput::default();
            input.text = initial.chars().collect();
            input.cursor = input.text.len();
            input.selection_anchor = Some(0);
            input.focused = true;
            page_inputs.insert(nid, input);
        }
        // Drop the lock before moving the Arc into Self — the next
        // layout pass will re-acquire it.
        drop(dlocked);

        // Cap the source-view buffer so the dev-dock doesn't blow up
        // on a 5 MB page. 64 KB is plenty for "read the head + first
        // few hundred lines of body".
        const SOURCE_CAP: usize = 65_536;
        let source_html = if html.len() > SOURCE_CAP {
            let mut t: String = html.chars().take(SOURCE_CAP / 4).collect();
            t.push_str("\n…(truncated)");
            t
        } else {
            html.clone()
        };
        let net_log = drain_net_capture();

        Ok(Self {
            title,
            url: url.clone(),
            net_log,
            console_log,
            source_html,
            doc,
            style,
            body_node,
            images,
            svgs,
            layout: None,
            last_width: 0,
            history: Vec::new(),
            forward: Vec::new(),
            scroll_y: 0.0,
            page_inputs,
            focused_input: autofocus_target,
            page_selection: None,
            js_ctx: Some(js_ctx),
        })
    }

    /// Replace the page contents with `new_url`, pushing the previous URL
    /// onto the history stack and clearing forward.
    /// Move page-load artifacts (DOM, style, registries, dev-dock
    /// captures) from a freshly-fetched `next` onto `self`. Used by
    /// navigate_to / go_back / go_forward / reload so the dev-dock
    /// always reflects the *current* page's fetch log, console
    /// output, and HTML source instead of whatever the tab was
    /// showing when it first opened.
    fn replace_page_artifacts(&mut self, next: TabState) {
        self.title = next.title;
        self.url = next.url;
        self.doc = next.doc;
        self.style = next.style;
        self.body_node = next.body_node;
        self.images = next.images;
        self.svgs = next.svgs;
        self.net_log = next.net_log;
        self.console_log = next.console_log;
        self.source_html = next.source_html;
        self.layout = None;
        self.last_width = 0;
        self.page_inputs.clear();
        self.focused_input = None;
        self.page_selection = None;
        // Drop the old page's JS engine before installing the new
        // one — listener closures captured Arc clones of the
        // outgoing document, so holding both would keep the old
        // DOM alive for no reason.
        self.js_ctx = next.js_ctx;
    }

    fn navigate_to(&mut self, new_url: &Url) -> Result<(), String> {
        let next = TabState::fetch(new_url)?;
        let prev_url = std::mem::replace(&mut self.url, next.url.clone());
        let prev_history = std::mem::take(&mut self.history);
        self.replace_page_artifacts(next);
        self.history = prev_history;
        self.history.push(prev_url);
        self.forward = Vec::new();
        self.scroll_y = 0.0;
        Ok(())
    }

    /// Outcome of a JS event dispatch fired before a chrome
    /// default action runs. The chrome inspects these bits to
    /// decide what to do next.
    fn dispatch_input_event(&mut self, kind: &str, target: bui_dom::NodeId) -> DispatchOutcome {
        let Some(ctx) = self.js_ctx.as_mut() else {
            return DispatchOutcome::default();
        };
        let event = ctx.dispatch(bui_js::Event::new(kind, target));
        let pending = ctx.take_pending_navigation();
        // We don't act on the dirty flag here — the orchestrator
        // calls layout on every frame anyway, and the dispatch
        // never mutates anything the current paint depends on
        // (it'll show up on the next frame). If a handler did
        // touch the DOM, `last_width = 0` below forces a re-
        // layout next frame.
        let dirty = ctx.take_dirty();
        if dirty {
            self.last_width = 0;
        }
        DispatchOutcome {
            default_prevented: event.flags.default_prevented,
            pending_nav: pending,
        }
    }

    fn go_back(&mut self) -> bool {
        let Some(prev) = self.history.pop() else { return false };
        let Ok(next) = TabState::fetch(&prev) else { return false };
        let now = std::mem::replace(&mut self.url, next.url.clone());
        self.forward.push(now);
        self.replace_page_artifacts(next);
        self.scroll_y = 0.0;
        true
    }

    fn go_forward(&mut self) -> bool {
        let Some(next_url) = self.forward.pop() else { return false };
        let Ok(next) = TabState::fetch(&next_url) else { return false };
        let now = std::mem::replace(&mut self.url, next.url.clone());
        self.history.push(now);
        self.replace_page_artifacts(next);
        self.scroll_y = 0.0;
        true
    }

    fn reload(&mut self) -> bool {
        let Ok(next) = TabState::fetch(&self.url) else { return false };
        self.replace_page_artifacts(next);
        // keep scroll_y so reload doesn't bounce the user to the top
        true
    }
}

// ---- browser state ----

struct BrowserState {
    tabs: Vec<TabState>,
    active: usize,
    /// Most-recent first.
    closed: Vec<TabState>,
    /// Set when the binary wants the App to exit (last tab closed).
    quit_requested: bool,
    address_input: AddressInput,
    last_click: Option<(std::time::Instant, f32, f32, u32)>,
    /// Sidebar visible? Toggled via ⌘B. When false the viewport spans
    /// the full window width.
    sidebar_open: bool,
    /// Dev-dock visible? Toggled via ⌘J. When true the bottom dock
    /// strip claims DOCK_HEIGHT px from the viewport.
    dock_open: bool,
    /// Currently-active dock viewer.
    active_dock_tab: DockTab,
}

/// Dev-dock panel selector. The dock is always one of three live
/// views: the XHR waterfall, the JS/CSS console log, or the raw HTML
/// source of the active page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DockTab {
    Xhr,
    Console,
    Source,
}

impl DockTab {
    fn label(self) -> &'static str {
        match self {
            DockTab::Xhr => "XHR",
            DockTab::Console => "Console",
            DockTab::Source => "Source",
        }
    }
}

const DOCK_TAB_ORDER: [DockTab; 3] = [DockTab::Xhr, DockTab::Console, DockTab::Source];

impl BrowserState {
    fn new(initial: TabState) -> Self {
        Self {
            tabs: vec![initial],
            active: 0,
            closed: Vec::new(),
            quit_requested: false,
            address_input: AddressInput::default(),
            last_click: None,
            sidebar_open: true,
            dock_open: false,
            active_dock_tab: DockTab::Xhr,
        }
    }

    fn active_tab(&self) -> &TabState {
        &self.tabs[self.active]
    }

    fn active_tab_mut(&mut self) -> &mut TabState {
        &mut self.tabs[self.active]
    }

    /// Effective sidebar width — 0 when toggled off, else the const.
    /// Used by chrome layout / hit-test code so the top bar fills the
    /// freed space when ⌘B hides the sidebar.
    fn sidebar_w(&self) -> f32 {
        if self.sidebar_open { SIDEBAR_WIDTH } else { 0.0 }
    }

    fn open_tab(&mut self, url_str: &str) {
        let Ok(url) = Url::parse(url_str) else { return };
        let Ok(tab) = TabState::fetch(&url) else { return };
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    fn close_active(&mut self) {
        if self.tabs.is_empty() {
            self.quit_requested = true;
            return;
        }
        let removed = self.tabs.remove(self.active);
        self.closed.push(removed);
        if self.closed.len() > 32 {
            self.closed.remove(0);
        }
        if self.tabs.is_empty() {
            self.quit_requested = true;
            return;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
    }

    fn reopen_closed(&mut self) {
        let Some(tab) = self.closed.pop() else { return };
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    fn switch_to(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.active = idx;
        }
    }

    fn cycle(&mut self, forward: bool) {
        if self.tabs.len() < 2 {
            return;
        }
        let n = self.tabs.len();
        self.active = if forward {
            (self.active + 1) % n
        } else {
            (self.active + n - 1) % n
        };
    }
}

fn run_browser(initial_url: &str) -> ExitCode {
    let url = match Url::parse(initial_url) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("invalid URL: {e}");
            return ExitCode::from(2);
        }
    };
    let initial = match TabState::fetch(&url) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("fetch error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let app_title = format!("Copper — {}", initial.title);
    let state = Arc::new(Mutex::new(BrowserState::new(initial)));

    let render_state = state.clone();
    let click_state = state.clone();
    let scroll_state = state.clone();
    let key_state = state.clone();
    let cursor_state = state.clone();
    let drag_state = state.clone();
    let mouse_up_state = state.clone();
    let right_click_state = state.clone();

    App::new(move |viewport| build_scene(viewport, &render_state))
        .with_title(app_title)
        .with_size(1280, 800)
        .on_click(move |viewport, x, y, mods| handle_click(&click_state, viewport, x, y, mods))
        .on_drag(move |viewport, x, y, mods| handle_drag(&drag_state, viewport, x, y, mods))
        .on_mouse_up(move |viewport, x, y, mods| handle_mouse_up(&mouse_up_state, viewport, x, y, mods))
        .on_right_click(move |viewport, x, y, mods| handle_right_click(&right_click_state, viewport, x, y, mods))
        .on_scroll(move |dy| {
            let mut st = scroll_state.lock().unwrap();
            let tab = st.active_tab_mut();
            tab.scroll_y = (tab.scroll_y - dy).max(0.0);
            true
        })
        .on_key(move |press| handle_key(&key_state, press))
        .on_cursor(move |viewport| cursor_for(&cursor_state, viewport))
        .run()
        .expect("event loop");
    ExitCode::SUCCESS
}

// ---- rendering ----

fn build_scene(viewport: Viewport, state: &Arc<Mutex<BrowserState>>) -> DisplayList {
    let mut dl = DisplayList::new();
    let w = viewport.width as f32;
    let h = viewport.height as f32;
    // Window-wide warm background. The sidebar (paper-2) paints over the
    // left band; the viewport (white) paints over the middle.
    dl.fill_rect(Rect::new(0.0, 0.0, w, h), BG);

    let st_for_layout = state.lock().unwrap();
    let sidebar_w = if st_for_layout.sidebar_open { SIDEBAR_WIDTH } else { 0.0 };
    let dock_h = if st_for_layout.dock_open { DOCK_HEIGHT } else { 0.0 };
    drop(st_for_layout);

    // Viewport surface — between sidebar and status, below the top bar
    // and above the dock (when open).
    let viewport_x = sidebar_w;
    let viewport_y = CHROME_HEIGHT;
    let viewport_w_px = (w - sidebar_w).max(0.0);
    let viewport_h_px = (h - CHROME_HEIGHT - STATUS_HEIGHT - dock_h).max(0.0);
    dl.fill_rect(
        Rect::new(viewport_x, viewport_y, viewport_w_px, viewport_h_px),
        VIEWPORT_BG,
    );

    let mut st = state.lock().unwrap();
    if st.quit_requested {
        // Window close is handled by winit on next event; nothing to paint.
        return dl;
    }
    let active = st.active;
    let tab_count = st.tabs.len();
    let tabs_meta: Vec<(String, bool)> = st
        .tabs
        .iter()
        .enumerate()
        .map(|(i, t)| (t.title.clone(), i == active))
        .collect();
    let tabs_for_sidebar: Vec<(String, Url, bool)> = st
        .tabs
        .iter()
        .enumerate()
        .map(|(i, t)| (t.title.clone(), t.url.clone(), i == active))
        .collect();
    let active_url = st.active_tab().url.to_string();

    // Page content lives between the sidebar (left), top bar (above),
    // status line (below) and the optional dev-dock. Subtract those
    // plus a small inner padding.
    let viewport_w = (viewport_w_px - VIEWPORT_PADDING * 2.0).max(0.0);
    // `viewport_w_px` and `viewport_h_px` were computed earlier from
    // `sidebar_w` / `dock_h`; they are the live geometry whether the
    // panels are open or not.
    {
        let tab = st.active_tab_mut();
        if tab.last_width != viewport.width || tab.layout.is_none() {
            // Push edited input buffers back into the DOM so layout
            // paints with what the user has typed. The HTML parser
            // never wrote a `value` attribute for inputs that didn't
            // declare one — `set_attr` adds it on the fly.
            {
                let mut d = tab.doc.lock().unwrap();
                for (nid, input) in &tab.page_inputs {
                    if let Some(elem) = d.element_mut(*nid) {
                        elem.set_attr("value", &input.text_string());
                    }
                }
            }
            // Make viewport-derived lengths (vw / vh / vmin / vmax)
            // resolve against the current PAGE rendering area, not
            // the full window size. Subtracting CHROME_HEIGHT keeps
            // 100vh content (Google's L3eUgb min-height: 100vh) from
            // overflowing past the chrome strip — without this, the
            // search bar landed below the visible area on full
            // screen Mac because the page tried to fill more pixels
            // than were actually paintable.
            let page_h = (viewport.height as f32
                - CHROME_HEIGHT
                - STATUS_HEIGHT
                - dock_h)
                .max(0.0);
            bui_style::set_viewport(viewport_w, page_h);
            let dlocked = tab.doc.lock().unwrap();
            let mut bx =
                bui_layout::build_with_images(
                    &dlocked,
                    &tab.style,
                    &tab.images,
                    &tab.svgs,
                    tab.body_node,
                );
            drop(dlocked);
            // Layout origin sits inside the viewport surface, offset by
            // the (live) sidebar width so the page renders to the right
            // of the sidebar instead of behind it.
            bui_layout::layout(
                &mut bx,
                sidebar_w + VIEWPORT_PADDING,
                VIEWPORT_PADDING,
                viewport_w,
            );
            tab.layout = Some(bx);
            tab.last_width = viewport.width;
        }
    }

    let scroll_y = st.active_tab().scroll_y;
    if let Some(bx) = &st.active_tab().layout {
        let mut page_dl = DisplayList::new();
        bui_layout::paint(bx, &mut page_dl);
        // Empty-body fallback. Some pages (Google /search,
        // Discord, Twitter, …) ship a fully JS-built UI — the
        // raw HTML body has no rendered text, only a sea of
        // <script> tags that assume a runtime we don't fully
        // emulate yet (Closure-library, React with proper
        // hydration, …). Detecting that *after* the script
        // pass and *before* paint lets us replace a white
        // viewport with a one-paragraph explanation pointing
        // to the roadmap, so the user knows why nothing
        // rendered instead of suspecting Copper crashed.
        //
        // Heuristic: zero Text commands emitted by the page
        // means "no visible glyphs anywhere". Pages with even
        // a single rendered character keep their natural
        // paint.
        let has_text = page_dl
            .commands
            .iter()
            .any(|c| matches!(c, PaintCommand::Text { .. }));
        if !has_text {
            page_dl = build_empty_body_fallback(viewport_w_px, &active_url);
        }
        let base_dy = CHROME_HEIGHT - scroll_y;
        // `position: sticky` defers its shift to here: bui-layout
        // emitted PushStickyGroup / PopStickyGroup bracket commands
        // with the box's natural_y, top_edge, and range_bottom. For
        // each group we replace the default `dy = chrome - scroll`
        // with a per-group `dy` that clamps the effective scroll so
        // the box pins to (chrome_top + top_edge) once the user
        // scrolls past it. The stack supports nested stickies; pop
        // returns to the parent's dy.
        let mut sticky_dy_stack: Vec<f32> = Vec::new();
        for cmd in page_dl.commands {
            let dy = *sticky_dy_stack.last().unwrap_or(&base_dy);
            match cmd {
                PaintCommand::FillRect { rect, color } => {
                    dl.commands.push(PaintCommand::FillRect {
                        rect: Rect::new(rect.x, rect.y + dy, rect.w, rect.h),
                        color,
                    });
                }
                PaintCommand::FillRoundedRect { rect, color, radii } => {
                    dl.commands.push(PaintCommand::FillRoundedRect {
                        rect: Rect::new(rect.x, rect.y + dy, rect.w, rect.h),
                        color,
                        radii,
                    });
                }
                PaintCommand::FillPath { points, color } => {
                    let translated = points
                        .into_iter()
                        .map(|(x, y)| (x, y + dy))
                        .collect();
                    dl.commands.push(PaintCommand::FillPath {
                        points: translated,
                        color,
                    });
                }
                PaintCommand::Image { rect, key } => {
                    dl.commands.push(PaintCommand::Image {
                        rect: Rect::new(rect.x, rect.y + dy, rect.w, rect.h),
                        key,
                    });
                }
                PaintCommand::Text {
                    x,
                    baseline,
                    advance,
                    font_size,
                    color,
                    content,
                } => {
                    dl.commands.push(PaintCommand::Text {
                        x,
                        baseline: baseline + dy,
                        advance,
                        font_size,
                        color,
                        content,
                    });
                }
                PaintCommand::BoxShadow { rect, color, radius, blur } => {
                    dl.commands.push(PaintCommand::BoxShadow {
                        rect: Rect::new(rect.x, rect.y + dy, rect.w, rect.h),
                        color,
                        radius,
                        blur,
                    });
                }
                PaintCommand::PushClip { rect, radii } => {
                    dl.commands.push(PaintCommand::PushClip {
                        rect: Rect::new(rect.x, rect.y + dy, rect.w, rect.h),
                        radii,
                    });
                }
                PaintCommand::PopClip => {
                    dl.commands.push(PaintCommand::PopClip);
                }
                PaintCommand::Svg {
                    rect,
                    view_box,
                    segments,
                    fill,
                    stroke,
                    stroke_width,
                } => {
                    dl.commands.push(PaintCommand::Svg {
                        rect: Rect::new(rect.x, rect.y + dy, rect.w, rect.h),
                        view_box,
                        segments,
                        fill,
                        stroke,
                        stroke_width,
                    });
                }
                PaintCommand::PushStickyGroup {
                    natural_y,
                    top_edge,
                    range_bottom,
                } => {
                    // Effective scroll: scroll normally while the box's
                    // pinned position would still be at or below
                    // `natural_y`; once `scroll_y > natural_y - top_edge`,
                    // freeze the box at `top_edge` from the chrome top.
                    // Once scroll exceeds the containing-block bottom,
                    // start scrolling again (the box is pushed off).
                    let pin_threshold = (natural_y - top_edge).max(0.0);
                    let push_threshold =
                        (range_bottom - natural_y - top_edge).max(pin_threshold);
                    let effective_scroll = scroll_y
                        .min(push_threshold)
                        .min(pin_threshold)
                        .max(0.0);
                    // Guard against the parent's dy being smaller than
                    // ours (nested stickies): a child sticky inside an
                    // already-pinned ancestor should not "un-pin" by
                    // dropping below the ancestor's offset.
                    let group_dy = (CHROME_HEIGHT - effective_scroll).max(dy);
                    sticky_dy_stack.push(group_dy);
                }
                PaintCommand::PopStickyGroup => {
                    sticky_dy_stack.pop();
                }
            }
        }
        // Overlays painted after the page (input caret, selection
        // highlight) live outside any sticky group, so they shift by
        // the plain scroll delta — never pinned. Restore the original
        // `dy` name for code below.
        let dy = base_dy;

        // Cursor overlay for the focused page <input>. We post-paint
        // here rather than threading state into bui-layout's paint —
        // the layout tree is shared across paint frames and the cursor
        // moves on every keystroke without needing a new layout build.
        if let Some(focus) = st.active_tab().focused_input {
            if let Some(input) = st.active_tab().page_inputs.get(&focus) {
                if let Some((frame, style)) = find_input_frame(bx, focus) {
                    let font = bui_text::shared_font();
                    let pad = (
                        style.padding.left.resolve(style.font_size, 16.0, frame.width),
                        style.padding.top.resolve(style.font_size, 16.0, frame.height),
                    );
                    let border = (
                        style.border.left.resolve(style.font_size, 16.0, frame.width),
                        style.border.top.resolve(style.font_size, 16.0, frame.height),
                    );
                    let inner_x = frame.x + border.0 + pad.0;
                    let inner_top = frame.y + border.1 + pad.1;
                    // Selection highlight first.
                    if let Some((s, e)) = input.selection_range() {
                        if s < e {
                            let sx = inner_x
                                + prefix_width(font, &input.text, style.font_size, s);
                            let sw = prefix_width(font, &input.text, style.font_size, e)
                                - (sx - inner_x);
                            dl.fill_rect(
                                Rect::new(sx, inner_top + dy, sw, style.font_size * 1.2),
                                Color::rgba(70, 130, 230, 110),
                            );
                        }
                    }
                    // Then the caret. Always draw — animation/blink TBD.
                    let cx = inner_x
                        + prefix_width(font, &input.text, style.font_size, input.cursor);
                    dl.fill_rect(
                        Rect::new(cx, inner_top + dy, 1.5, style.font_size * 1.2),
                        Color::rgb(40, 90, 200),
                    );
                }
            }
        }

        // Page selection highlight. Painted after page text so the
        // 30 %-alpha blue tint sits over the glyphs without erasing
        // them. We re-walk text runs each frame; cheap enough since
        // the typical viewport has hundreds of runs at most, and
        // the work only happens while a selection exists.
        if let Some(sel) = st.active_tab().page_selection {
            paint_page_selection(bx, sel, dy, &mut dl);
        }
    }

    let can_back = !st.active_tab().history.is_empty();
    let can_forward = !st.active_tab().forward.is_empty();

    // Sidebar (tabs) — only when open. Painted before the top bar so
    // the top bar's bottom rule visually crosses the sidebar's right
    // edge cleanly.
    if sidebar_w > 0.0 {
        let sidebar_layout = build_sidebar_layout(h, &tabs_for_sidebar);
        paint_sidebar(h, viewport.cursor, &tabs_for_sidebar, &sidebar_layout, &mut dl);
    }

    // Top bar (nav + URL pill, to the right of the sidebar).
    paint_chrome(
        viewport.width,
        viewport.cursor,
        &active_url,
        &st.address_input,
        &tabs_meta,
        tab_count,
        can_back,
        can_forward,
        sidebar_w,
        &mut dl,
    );

    // Dev-dock (above the status line, when open).
    if dock_h > 0.0 {
        let active_dock_tab = st.active_dock_tab;
        let (net_log, console_log, source_html) = {
            let tab = st.active_tab();
            (tab.net_log.clone(), tab.console_log.clone(), tab.source_html.clone())
        };
        paint_dock(
            w,
            h,
            sidebar_w,
            viewport.cursor,
            active_dock_tab,
            &net_log,
            &console_log,
            &source_html,
            &mut dl,
        );
    }

    // Status line — bottom strip showing mode pill, URL context, scroll.
    let scroll_pct = {
        let tab = st.active_tab();
        if let Some(layout) = &tab.layout {
            let doc_h = layout.frame.height.max(1.0);
            let max_scroll = (doc_h - viewport_h_px).max(1.0);
            ((tab.scroll_y / max_scroll).clamp(0.0, 1.0) * 100.0).round() as u32
        } else {
            0
        }
    };
    paint_status(w, h, &active_url, scroll_pct, &mut dl);
    dl
}

fn paint_nav_buttons(
    cursor: (f32, f32),
    can_back: bool,
    can_forward: bool,
    sidebar_w: f32,
    dl: &mut DisplayList,
) {
    let buttons = [
        (NavButton::Back, can_back),
        (NavButton::Forward, can_forward),
        (NavButton::Reload, true),
    ];
    let hovered = nav_button_at(cursor.0, cursor.1, sidebar_w);
    for (i, (btn, enabled)) in buttons.iter().enumerate() {
        let r = nav_button_rect(i, sidebar_w);
        let is_hovered = hovered == Some(*btn) && *enabled;
        // Background: only on hover, to keep idle chrome visually clean.
        if is_hovered {
            dl.fill_rounded_rect(r, TAB_INACTIVE_HOVER, [r.h * 0.5; 4]);
        }
        let icon_color = if *enabled {
            Color::rgb(60, 60, 70)
        } else {
            Color::rgb(180, 180, 188)
        };
        let cx = r.x + r.w / 2.0;
        let cy = r.y + r.h / 2.0;
        match btn {
            NavButton::Back => {
                // Filled left-pointing triangle, ~10×12 px.
                let half = 5.0;
                let pts = vec![
                    (cx + 2.0, cy - half - 1.0),
                    (cx - 4.0, cy),
                    (cx + 2.0, cy + half + 1.0),
                ];
                dl.fill_path(pts, icon_color);
            }
            NavButton::Forward => {
                let half = 5.0;
                let pts = vec![
                    (cx - 2.0, cy - half - 1.0),
                    (cx + 4.0, cy),
                    (cx - 2.0, cy + half + 1.0),
                ];
                dl.fill_path(pts, icon_color);
            }
            NavButton::Reload => {
                // ~270° arc with arrowhead pointing tangentially. Rendered
                // as a closed filled outline (outer arc + inner arc back).
                let outer = 6.5;
                let inner = 3.5;
                use std::f32::consts::PI;
                let start = -PI * 0.45; // ~just past 12 o'clock
                let sweep = PI * 1.55;  // ~280°
                let steps = 24;
                let mut pts = Vec::with_capacity(steps * 2 + 6);
                for i in 0..=steps {
                    let t = i as f32 / steps as f32;
                    let a = start + t * sweep;
                    pts.push((cx + outer * a.cos(), cy + outer * a.sin()));
                }
                // Arrowhead at the end of the outer arc, pointing
                // tangentially (the way the swept circle is going).
                let end_a = start + sweep;
                let tangent_x = -end_a.sin();
                let tangent_y = end_a.cos();
                let tip = (
                    cx + (outer + 4.0) * end_a.cos() + tangent_x * 1.0,
                    cy + (outer + 4.0) * end_a.sin() + tangent_y * 1.0,
                );
                pts.push(tip);
                pts.push((cx + inner * end_a.cos(), cy + inner * end_a.sin()));
                // Inner arc back to start.
                for i in (0..=steps).rev() {
                    let t = i as f32 / steps as f32;
                    let a = start + t * sweep;
                    pts.push((cx + inner * a.cos(), cy + inner * a.sin()));
                }
                dl.fill_path(pts, icon_color);
            }
        }
    }
}

fn tab_layout(viewport_w: u32, n_tabs: usize) -> (f32, f32) {
    // Returns (tab_width, total_strip_used_width). The leading reserve
    // (traffic lights on macOS) is subtracted from the available band.
    let avail =
        (viewport_w as f32 - TAB_STRIP_LEADING - NEW_TAB_BTN_WIDTH - TAB_GAP).max(0.0);
    let n = n_tabs.max(1) as f32;
    let raw = avail / n - TAB_GAP;
    (raw.clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH), avail)
}

// ---- IDE-Pane sidebar (left strip, vertical tab tree) ----

/// Top inset inside the sidebar. On macOS the window's traffic-light
/// buttons (close / minimize / zoom) live at roughly x ∈ [10, 70],
/// y ∈ [4, 22]. We reserve enough vertical space above the tab list
/// so they don't sit on top of tab rows.
#[cfg(target_os = "macos")]
const SIDEBAR_TOP_INSET: f32 = 36.0;
#[cfg(not(target_os = "macos"))]
const SIDEBAR_TOP_INSET: f32 = 12.0;
const SIDEBAR_PAD_X: f32 = 12.0;
const SIDEBAR_HEADER_FONT: f32 = 11.0;
const SIDEBAR_TAB_FONT: f32 = 13.0;
const SIDEBAR_TAB_HEIGHT: f32 = 28.0;
const SIDEBAR_TAB_GAP: f32 = 2.0;
const SIDEBAR_GROUP_GAP: f32 = 8.0;
const SIDEBAR_CLOSE_W: f32 = 22.0;

/// Hostname extracted from a tab's URL for grouping. Falls back to the
/// raw scheme-string for `copper://` internal pages so the start page
/// gets its own visible group instead of being lumped with everything
/// else.
fn tab_group_key(url: &Url) -> String {
    if url.host.is_empty() {
        url.scheme.clone()
    } else {
        url.host.clone()
    }
}

/// Hit-testable region for the sidebar tab list. Built each frame from
/// the current tab order so paint and hit-test agree on geometry.
#[derive(Debug, Clone)]
struct SidebarLayout {
    /// `(tab_index, row_rect, close_rect)` in window coords. Group
    /// headers are not in this list — they're rendered but not yet
    /// interactive (collapse/expand comes later).
    rows: Vec<(usize, Rect, Rect)>,
}

fn build_sidebar_layout(
    height: f32,
    tabs: &[(String, Url, bool)],
) -> SidebarLayout {
    let mut rows = Vec::with_capacity(tabs.len());
    let mut y = SIDEBAR_TOP_INSET + SIDEBAR_HEADER_FONT + 8.0; // header + gap
    // Group tabs by host, preserving source order inside each group.
    let mut current_group: Option<String> = None;
    for (i, (_title, url, _active)) in tabs.iter().enumerate() {
        let key = tab_group_key(url);
        if current_group.as_deref() != Some(key.as_str()) {
            if current_group.is_some() {
                y += SIDEBAR_GROUP_GAP;
            }
            // group header line
            y += SIDEBAR_HEADER_FONT + 8.0;
            current_group = Some(key);
        }
        if y + SIDEBAR_TAB_HEIGHT > height - STATUS_HEIGHT {
            break;
        }
        let row_rect = Rect::new(
            SIDEBAR_PAD_X,
            y,
            SIDEBAR_WIDTH - SIDEBAR_PAD_X * 2.0,
            SIDEBAR_TAB_HEIGHT,
        );
        let close_rect = Rect::new(
            row_rect.x + row_rect.w - SIDEBAR_CLOSE_W,
            row_rect.y,
            SIDEBAR_CLOSE_W,
            row_rect.h,
        );
        rows.push((i, row_rect, close_rect));
        y += SIDEBAR_TAB_HEIGHT + SIDEBAR_TAB_GAP;
    }
    SidebarLayout { rows }
}

fn paint_sidebar(
    height: f32,
    cursor: (f32, f32),
    tabs: &[(String, Url, bool)],
    layout: &SidebarLayout,
    dl: &mut DisplayList,
) {
    // Sidebar surface.
    dl.fill_rect(Rect::new(0.0, 0.0, SIDEBAR_WIDTH, height), SIDEBAR_BG);
    // Right-edge rule separating sidebar from viewport.
    dl.fill_rect(
        Rect::new(SIDEBAR_WIDTH - 1.0, 0.0, 1.0, height),
        SIDEBAR_RULE,
    );

    let font = bui_text::shared_font();

    // Section heading "TABS · n open" — mono, copper-uppercase per design.
    let header = format!("TABS · {} open", tabs.len());
    let header_advance = font.measure_text(&header, SIDEBAR_HEADER_FONT);
    dl.commands.push(PaintCommand::Text {
        x: SIDEBAR_PAD_X,
        baseline: SIDEBAR_TOP_INSET + SIDEBAR_HEADER_FONT,
        advance: header_advance,
        font_size: SIDEBAR_HEADER_FONT,
        color: INK_3,
        content: header,
    });

    // Walk paint geometry alongside the host groups so each header
    // gets a small chevron + hostname above its tabs.
    let mut current_group: Option<String> = None;
    let mut group_y = SIDEBAR_TOP_INSET + SIDEBAR_HEADER_FONT + 8.0;
    let mut row_iter = layout.rows.iter().peekable();
    while let Some(&(idx, row, close_rect)) = row_iter.next() {
        let (title, url, is_active) = &tabs[idx];
        let key = tab_group_key(url);
        if current_group.as_deref() != Some(key.as_str()) {
            if current_group.is_some() {
                group_y += SIDEBAR_GROUP_GAP;
            }
            let label = format!("▾ {}", key);
            let advance = font.measure_text(&label, SIDEBAR_HEADER_FONT);
            dl.commands.push(PaintCommand::Text {
                x: SIDEBAR_PAD_X,
                baseline: group_y + SIDEBAR_HEADER_FONT,
                advance,
                font_size: SIDEBAR_HEADER_FONT,
                color: INK_3,
                content: label,
            });
            group_y += SIDEBAR_HEADER_FONT + 8.0;
            current_group = Some(key);
        }

        let row_hovered = rect_contains(row, cursor);
        let close_hovered = rect_contains(close_rect, cursor);
        let (bg, text_color) = if *is_active {
            (COPPER_SOFT, INK)
        } else if row_hovered {
            (PAPER, INK)
        } else {
            (Color::rgba(0, 0, 0, 0), INK_2)
        };
        if bg.a > 0 {
            dl.fill_rounded_rect(row, bg, [6.0; 4]);
        }
        // Live dot on the left.
        let dot_r = 3.5;
        let dot_cx = row.x + 10.0;
        let dot_cy = row.y + row.h / 2.0;
        let dot_color = if *is_active { COPPER } else { INK_3 };
        dl.fill_rounded_rect(
            Rect::new(dot_cx - dot_r, dot_cy - dot_r, dot_r * 2.0, dot_r * 2.0),
            dot_color,
            [dot_r; 4],
        );
        // Title.
        let title_x = row.x + 22.0;
        let title_max = row.w - 22.0 - SIDEBAR_CLOSE_W - 4.0;
        let visible = truncate_to_width(title, font, SIDEBAR_TAB_FONT, title_max.max(0.0));
        let advance = font.measure_text(&visible, SIDEBAR_TAB_FONT);
        dl.commands.push(PaintCommand::Text {
            x: title_x,
            baseline: row.y + row.h * 0.5 + SIDEBAR_TAB_FONT * 0.34,
            advance,
            font_size: SIDEBAR_TAB_FONT,
            color: text_color,
            content: visible,
        });
        // Close 'x' on hover.
        if row_hovered || *is_active {
            let close_fg = if close_hovered { Color::WHITE } else { INK_3 };
            if close_hovered {
                dl.fill_rounded_rect(
                    Rect::new(close_rect.x + 2.0, close_rect.y + 4.0, 18.0, row.h - 8.0),
                    COPPER,
                    [4.0; 4],
                );
            }
            let glyph_advance = font.measure_text("×", SIDEBAR_TAB_FONT);
            dl.commands.push(PaintCommand::Text {
                x: close_rect.x + (close_rect.w - glyph_advance) / 2.0,
                baseline: row.y + row.h * 0.5 + SIDEBAR_TAB_FONT * 0.34,
                advance: glyph_advance,
                font_size: SIDEBAR_TAB_FONT,
                color: close_fg,
                content: "×".to_string(),
            });
        }

        // Advance group_y past the row.
        group_y = row.y + row.h + SIDEBAR_TAB_GAP;

        // If next row belongs to a different group we'll re-enter the
        // header branch on the next loop iteration; nothing extra here.
        let _ = row_iter.peek();
    }
}

fn rect_contains(r: Rect, p: (f32, f32)) -> bool {
    p.0 >= r.x && p.0 < r.x + r.w && p.1 >= r.y && p.1 < r.y + r.h
}

/// Paint a single-pixel border around a rounded rect by stacking a
/// slightly larger filled rect underneath. We don't have a stroke
/// primitive so this is the cheapest way to get the look without
/// reaching for a full SVG path. The fill that follows in source order
/// covers all but the 1-px ring.
fn paint_pill_border(r: Rect, radius: f32, color: Color, dl: &mut DisplayList) {
    let outer = Rect::new(r.x - 1.0, r.y - 1.0, r.w + 2.0, r.h + 2.0);
    // Insert behind the most-recently-pushed fill (the pill itself) so
    // the ring shows through 1 px on every side.
    let insert_at = dl.commands.len().saturating_sub(1);
    dl.commands.insert(
        insert_at,
        PaintCommand::FillRoundedRect {
            rect: outer,
            color,
            radii: [radius + 1.0; 4],
        },
    );
}

/// Hit-test rect for a single top-bar action button (right-aligned,
/// laid out from the right edge of the bar inward, in TOP_BAR_ACTIONS
/// source order). Returns the rect for the i-th action, or `None` if
/// the bar isn't wide enough to fit it.
fn top_bar_action_rect(width: u32, idx: usize, sidebar_w: f32) -> Option<Rect> {
    let widths = top_bar_action_widths();
    if idx >= widths.len() {
        return None;
    }
    let w = width as f32;
    let y = (TOP_BAR_HEIGHT - TOP_BAR_ACTION_HEIGHT) / 2.0;
    let mut x = w - ADDR_INSET;
    for (i, btn_w) in widths.iter().enumerate().rev() {
        x -= btn_w;
        // Don't paint into the sidebar's column. With a hidden sidebar
        // (sidebar_w == 0) the actions stretch all the way left if
        // needed; with the sidebar open they stop at its right edge.
        if x < sidebar_w {
            return None;
        }
        if i == idx {
            return Some(Rect::new(x, y, *btn_w, TOP_BAR_ACTION_HEIGHT));
        }
        x -= TOP_BAR_ACTION_GAP;
    }
    None
}

fn top_bar_action_at(width: u32, cursor: (f32, f32), sidebar_w: f32) -> Option<usize> {
    for i in 0..TOP_BAR_ACTIONS.len() {
        if let Some(r) = top_bar_action_rect(width, i, sidebar_w) {
            if rect_contains(r, cursor) {
                return Some(i);
            }
        }
    }
    None
}

fn paint_top_bar_actions(width: u32, cursor: (f32, f32), sidebar_w: f32, dl: &mut DisplayList) {
    let font = bui_text::shared_font();
    let hover = top_bar_action_at(width, cursor, sidebar_w);
    for (i, label) in TOP_BAR_ACTIONS.iter().enumerate() {
        let Some(r) = top_bar_action_rect(width, i, sidebar_w) else { continue };
        let is_hover = hover == Some(i);
        let bg = if is_hover { COPPER_SOFT } else { PAPER };
        let fg = if is_hover { COPPER_DEEP } else { INK_2 };
        dl.fill_rounded_rect(r, bg, [6.0; 4]);
        paint_pill_border(r, 6.0, RULE, dl);
        let advance = font.measure_text(label, TOP_BAR_ACTION_FONT);
        dl.commands.push(PaintCommand::Text {
            x: r.x + (r.w - advance) / 2.0,
            baseline: r.y + r.h * 0.5 + TOP_BAR_ACTION_FONT * 0.34,
            advance,
            font_size: TOP_BAR_ACTION_FONT,
            color: fg,
            content: label.to_string(),
        });
    }
}

// ---- IDE-Pane dev-dock (persistent bottom panel, toggle ⌘J) ----

const DOCK_HEADER_HEIGHT: f32 = 28.0;
const DOCK_TAB_FONT: f32 = 12.0;
const DOCK_BODY_FONT: f32 = 11.5;
const DOCK_TAB_PAD_X: f32 = 14.0;
const DOCK_TAB_GAP: f32 = 6.0;

/// Geometry for one dock tab header — used both by paint_dock to draw
/// the label and by handle_click to resolve which tab a cursor lands
/// on. Returns hit rects in window coords.
fn dock_tab_rects(sidebar_w: f32, dock_y: f32) -> Vec<(DockTab, Rect)> {
    let font = bui_text::shared_font();
    let mut x = sidebar_w + 8.0;
    let y = dock_y;
    DOCK_TAB_ORDER
        .iter()
        .map(|tab| {
            let advance = font.measure_text(tab.label(), DOCK_TAB_FONT);
            let w = advance + DOCK_TAB_PAD_X * 2.0;
            let r = Rect::new(x, y, w, DOCK_HEADER_HEIGHT);
            x += w + DOCK_TAB_GAP;
            (*tab, r)
        })
        .collect()
}

fn paint_dock(
    width: f32,
    height: f32,
    sidebar_w: f32,
    cursor: (f32, f32),
    active_tab: DockTab,
    net_log: &[NetEntry],
    console_log: &[String],
    source: &str,
    dl: &mut DisplayList,
) {
    let viewport_w = (width - sidebar_w).max(0.0);
    let dock_y = height - STATUS_HEIGHT - DOCK_HEIGHT;
    if dock_y <= 0.0 || viewport_w <= 0.0 {
        return;
    }
    // Dock surface.
    dl.fill_rect(
        Rect::new(sidebar_w, dock_y, viewport_w, DOCK_HEIGHT),
        DOCK_BG,
    );
    dl.fill_rect(Rect::new(sidebar_w, dock_y, viewport_w, 1.0), DOCK_RULE);

    let font = bui_text::shared_font();

    // Tab header strip. Active tab gets a copper-soft pill background;
    // hover gets a paper-2 background. Inactive tabs render in ink-3.
    let rects = dock_tab_rects(sidebar_w, dock_y);
    let label_baseline = dock_y + DOCK_HEADER_HEIGHT - 9.0;
    for (tab, r) in &rects {
        let is_active = *tab == active_tab;
        let is_hover = rect_contains(*r, cursor);
        let (bg, fg) = if is_active {
            (COPPER_SOFT, COPPER_DEEP)
        } else if is_hover {
            (PAPER_2, INK_2)
        } else {
            (Color::rgba(0, 0, 0, 0), INK_3)
        };
        if bg.a > 0 {
            dl.fill_rounded_rect(
                Rect::new(r.x + 2.0, r.y + 4.0, r.w - 4.0, r.h - 8.0),
                bg,
                [4.0; 4],
            );
        }
        let advance = font.measure_text(tab.label(), DOCK_TAB_FONT);
        dl.commands.push(PaintCommand::Text {
            x: r.x + (r.w - advance) / 2.0,
            baseline: label_baseline,
            advance,
            font_size: DOCK_TAB_FONT,
            color: fg,
            content: tab.label().to_string(),
        });
    }
    // Right-aligned context: count + ⌘J toggle hint.
    let summary = match active_tab {
        DockTab::Xhr => format!("{} req · ⌘J toggle", net_log.len()),
        DockTab::Console => format!("{} msg · ⌘J toggle", console_log.len()),
        DockTab::Source => format!("{} B · ⌘J toggle", source.len()),
    };
    let summary_adv = font.measure_text(&summary, 10.5);
    dl.commands.push(PaintCommand::Text {
        x: sidebar_w + viewport_w - summary_adv - 14.0,
        baseline: dock_y + DOCK_HEADER_HEIGHT - 10.0,
        advance: summary_adv,
        font_size: 10.5,
        color: INK_3,
        content: summary,
    });
    // Header / body divider.
    dl.fill_rect(
        Rect::new(
            sidebar_w + 8.0,
            dock_y + DOCK_HEADER_HEIGHT,
            viewport_w - 16.0,
            1.0,
        ),
        RULE,
    );

    let body_top = dock_y + DOCK_HEADER_HEIGHT + 8.0;
    let body_bottom = dock_y + DOCK_HEIGHT - 8.0;
    let body_x = sidebar_w + 14.0;
    let body_w = (viewport_w - 28.0).max(0.0);

    match active_tab {
        DockTab::Xhr => paint_dock_xhr(net_log, body_x, body_top, body_w, body_bottom, dl),
        DockTab::Console => {
            paint_dock_console(console_log, body_x, body_top, body_w, body_bottom, dl)
        }
        DockTab::Source => paint_dock_source(source, body_x, body_top, body_w, body_bottom, dl),
    }
}

/// XHR waterfall: each captured fetch is one row.
///
/// Row layout (left → right):
///   [METHOD pill] [URL ............................]
///   [bar track with tinted fill] [status·ms]   [bytes]
///
/// Status and bytes share a right-aligned cluster so they never
/// collide even when a 4-character status text grows to 9 ("503 ·
/// 1240ms"). Bars are normalised against the slowest request in the
/// log so a 5-ms fetch and a 5000-ms fetch share the same visual range.
fn paint_dock_xhr(
    log: &[NetEntry],
    x: f32,
    top: f32,
    width: f32,
    bottom: f32,
    dl: &mut DisplayList,
) {
    let font = bui_text::shared_font();
    if log.is_empty() {
        empty_state(font, "no requests captured", x, top, dl);
        return;
    }
    let max_ms = log.iter().map(|e| e.ms.max(1)).max().unwrap_or(1) as f32;
    // Right-aligned cluster: bytes (62 px), status (88 px). Bar fills
    // half of what remains; the URL/method block takes the rest.
    let bytes_col_w: f32 = 62.0;
    let status_col_w: f32 = 88.0;
    let right_cluster_w = bytes_col_w + status_col_w + 8.0;
    let left_w = (width - right_cluster_w).max(120.0);
    let url_col_w = left_w * 0.55;
    let bar_col_w = left_w - url_col_w;
    let method_pill_w: f32 = 42.0; // enough for GET / POST / DELETE
    let row_h = DOCK_BODY_FONT + 8.0;
    let mut y = top;
    for entry in log.iter().take(((bottom - top) / row_h) as usize) {
        // METHOD pill — coloured background, white glyph, mono-ish.
        let method_color = match entry.method.as_str() {
            "GET" => COPPER_DEEP,
            "POST" | "PUT" | "PATCH" => COPPER,
            "DELETE" => Color::rgb(0xAA, 0x3A, 0x3A),
            _ => INK_3,
        };
        dl.fill_rounded_rect(
            Rect::new(x, y + 2.0, method_pill_w, row_h - 4.0),
            method_color,
            [3.0; 4],
        );
        let method_adv = font.measure_text(&entry.method, DOCK_BODY_FONT);
        dl.commands.push(PaintCommand::Text {
            x: x + (method_pill_w - method_adv) / 2.0,
            baseline: y + DOCK_BODY_FONT + 1.0,
            advance: method_adv,
            font_size: DOCK_BODY_FONT,
            color: Color::WHITE,
            content: entry.method.clone(),
        });
        // URL (truncated to fit the rest of the URL column).
        let url_x = x + method_pill_w + 8.0;
        let url_max_w = (url_col_w - method_pill_w - 16.0).max(40.0);
        let trimmed = truncate_to_width(&entry.url, font, DOCK_BODY_FONT, url_max_w);
        let trimmed_adv = font.measure_text(&trimmed, DOCK_BODY_FONT);
        dl.commands.push(PaintCommand::Text {
            x: url_x,
            baseline: y + DOCK_BODY_FONT + 1.0,
            advance: trimmed_adv,
            font_size: DOCK_BODY_FONT,
            color: INK_2,
            content: trimmed,
        });
        // Bar — width proportional to ms/max_ms.
        let bar_x = x + url_col_w + 4.0;
        let bar_track_w = bar_col_w - 8.0;
        let bar_w = (entry.ms as f32 / max_ms) * bar_track_w;
        dl.fill_rounded_rect(
            Rect::new(bar_x, y + 4.0, bar_track_w, row_h - 8.0),
            PAPER_2,
            [2.0; 4],
        );
        let bar_color = if !(200..300).contains(&entry.status) {
            COPPER
        } else if entry.url.starts_with("https://upload.wikimedia.org/")
            || entry.url.starts_with("https://i.")
        {
            INK_3
        } else {
            COPPER_DEEP
        };
        if bar_w > 0.0 {
            dl.fill_rounded_rect(
                Rect::new(bar_x, y + 4.0, bar_w.max(1.0), row_h - 8.0),
                bar_color,
                [2.0; 4],
            );
        }
        // Right cluster: status (right-aligned inside status_col_w)
        // and bytes (right-aligned inside bytes_col_w).
        let cluster_x = x + width - right_cluster_w;
        let status_text = format!("{} · {}ms", entry.status, entry.ms);
        let status_adv = font.measure_text(&status_text, DOCK_BODY_FONT);
        dl.commands.push(PaintCommand::Text {
            x: cluster_x + (status_col_w - status_adv).max(0.0),
            baseline: y + DOCK_BODY_FONT + 1.0,
            advance: status_adv,
            font_size: DOCK_BODY_FONT,
            color: if (200..300).contains(&entry.status) { INK_2 } else { COPPER_DEEP },
            content: status_text,
        });
        let bytes_text = human_bytes(entry.bytes);
        let bytes_adv = font.measure_text(&bytes_text, DOCK_BODY_FONT);
        dl.commands.push(PaintCommand::Text {
            x: cluster_x + status_col_w + 8.0 + (bytes_col_w - bytes_adv).max(0.0),
            baseline: y + DOCK_BODY_FONT + 1.0,
            advance: bytes_adv,
            font_size: DOCK_BODY_FONT,
            color: INK_3,
            content: bytes_text,
        });
        y += row_h;
    }
}

fn human_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{} B", n)
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f32 / 1024.0)
    } else {
        format!("{:.1} MB", n as f32 / (1024.0 * 1024.0))
    }
}

fn paint_dock_console(
    log: &[String],
    x: f32,
    top: f32,
    width: f32,
    bottom: f32,
    dl: &mut DisplayList,
) {
    let font = bui_text::shared_font();
    if log.is_empty() {
        empty_state(font, "no console messages — try a page with <script>", x, top, dl);
        return;
    }
    let row_h = DOCK_BODY_FONT + 6.0;
    let mut y = top;
    for line in log.iter().take(((bottom - top) / row_h) as usize) {
        let trimmed = truncate_to_width(line, font, DOCK_BODY_FONT, width);
        let advance = font.measure_text(&trimmed, DOCK_BODY_FONT);
        dl.commands.push(PaintCommand::Text {
            x,
            baseline: y + DOCK_BODY_FONT,
            advance,
            font_size: DOCK_BODY_FONT,
            color: INK_2,
            content: trimmed,
        });
        y += row_h;
    }
}

fn paint_dock_source(
    source: &str,
    x: f32,
    top: f32,
    width: f32,
    bottom: f32,
    dl: &mut DisplayList,
) {
    let font = bui_text::shared_font();
    if source.is_empty() {
        empty_state(font, "no source available", x, top, dl);
        return;
    }
    let row_h = DOCK_BODY_FONT + 4.0;
    let max_rows = ((bottom - top) / row_h) as usize;
    let mut y = top;
    for (i, raw) in source.lines().take(max_rows).enumerate() {
        // Render the leading whitespace so structure is visible.
        let trimmed = truncate_to_width(raw, font, DOCK_BODY_FONT, width - 32.0);
        let advance = font.measure_text(&trimmed, DOCK_BODY_FONT);
        // Line number gutter.
        let lineno = format!("{:>4} ", i + 1);
        let lineno_adv = font.measure_text(&lineno, DOCK_BODY_FONT);
        dl.commands.push(PaintCommand::Text {
            x,
            baseline: y + DOCK_BODY_FONT,
            advance: lineno_adv,
            font_size: DOCK_BODY_FONT,
            color: INK_3,
            content: lineno,
        });
        dl.commands.push(PaintCommand::Text {
            x: x + 32.0,
            baseline: y + DOCK_BODY_FONT,
            advance,
            font_size: DOCK_BODY_FONT,
            color: INK_2,
            content: trimmed,
        });
        y += row_h;
    }
}

fn empty_state(font: &bui_text::Font, msg: &str, x: f32, y: f32, dl: &mut DisplayList) {
    let advance = font.measure_text(msg, DOCK_BODY_FONT);
    dl.commands.push(PaintCommand::Text {
        x,
        baseline: y + DOCK_BODY_FONT,
        advance,
        font_size: DOCK_BODY_FONT,
        color: INK_3,
        content: msg.to_string(),
    });
}

// ---- IDE-Pane status line (bottom strip) ----

/// Build a display list that explains why the body is empty.
/// Triggered when the page produced no Text commands at all
/// (Google /search, Discord, Twitter, … any "JS shell with
/// nothing in the static HTML body"). Coordinates are in the
/// page-paint space — the caller wraps this with the standard
/// chrome offset.
///
/// Reads like a paragraph because that's the most accessible
/// way to convey context: title, one-line explanation, the
/// URL the user asked for, a pointer to the roadmap file.
fn build_empty_body_fallback(viewport_w_px: f32, url: &str) -> DisplayList {
    use bui_paint::{DisplayList, PaintCommand};

    const TITLE_FS: f32 = 28.0;
    const BODY_FS: f32 = 16.0;
    const URL_FS: f32 = 14.0;
    const LINE_GAP: f32 = 24.0;
    const PARA_GAP: f32 = 16.0;

    let font = bui_text::shared_font();
    let pad_x = 48.0_f32.min(viewport_w_px * 0.08);
    let inner_w = (viewport_w_px - pad_x * 2.0).max(200.0);
    let mut dl = DisplayList::new();
    let mut y = 80.0_f32;

    // Title.
    let title = "Nothing to render here.";
    let title_w = font.measure_text(title, TITLE_FS);
    dl.commands.push(PaintCommand::Text {
        x: pad_x,
        baseline: y + TITLE_FS,
        advance: title_w,
        font_size: TITLE_FS,
        color: INK_2,
        content: title.to_string(),
    });
    y += TITLE_FS + PARA_GAP;

    // Body lines — flow text within `inner_w` so long
    // explanations wrap. Hand-wrapped by word boundary.
    let body_lines = [
        "This page's content is built entirely by JavaScript Copper doesn't fully run yet.",
        "Real-world examples: Google /search, Discord, Twitter, Maps. They ship a near-empty <body> and let a JS bundle (Closure-library, React, etc.) build the UI from scratch — Copper's JS engine handles inline scripts but doesn't run those bundles.",
        "Pages whose HTML carries the real content (Wikipedia, GitHub, Hacker News, news sites) render fine. See docs/google-render-plan.md for the JS roadmap.",
    ];
    for paragraph in &body_lines {
        for line in wrap_text_by_width(&font, paragraph, BODY_FS, inner_w) {
            let advance = font.measure_text(&line, BODY_FS);
            dl.commands.push(PaintCommand::Text {
                x: pad_x,
                baseline: y + BODY_FS,
                advance,
                font_size: BODY_FS,
                color: INK_2,
                content: line,
            });
            y += LINE_GAP;
        }
        y += PARA_GAP;
    }

    // URL footer — the address the request actually landed
    // on, so the user can confirm the navigation happened and
    // copy the URL elsewhere if they want to retry in a real
    // browser.
    let url_label = "Requested URL:";
    let url_label_w = font.measure_text(url_label, URL_FS);
    dl.commands.push(PaintCommand::Text {
        x: pad_x,
        baseline: y + URL_FS,
        advance: url_label_w,
        font_size: URL_FS,
        color: INK_3,
        content: url_label.to_string(),
    });
    y += LINE_GAP;
    for line in wrap_text_by_width(&font, url, URL_FS, inner_w) {
        let advance = font.measure_text(&line, URL_FS);
        dl.commands.push(PaintCommand::Text {
            x: pad_x,
            baseline: y + URL_FS,
            advance,
            font_size: URL_FS,
            color: INK_3,
            content: line,
        });
        y += LINE_GAP;
    }
    dl
}

/// Simple word-wrap by measured pixel width. Words longer than
/// `max_w` get a line to themselves (they overflow gracefully
/// rather than being chopped mid-character — readability
/// matters more than absolute compliance for an error page).
fn wrap_text_by_width(
    font: &bui_text::Font,
    text: &str,
    font_size: f32,
    max_w: f32,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let candidate = if cur.is_empty() {
            word.to_string()
        } else {
            format!("{cur} {word}")
        };
        if font.measure_text(&candidate, font_size) <= max_w || cur.is_empty() {
            cur = candidate;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_string();
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

fn paint_status(width: f32, height: f32, url: &str, scroll_pct: u32, dl: &mut DisplayList) {
    let y = height - STATUS_HEIGHT;
    if y <= 0.0 {
        return;
    }
    dl.fill_rect(Rect::new(0.0, y, width, STATUS_HEIGHT), STATUS_BG);
    dl.fill_rect(Rect::new(0.0, y, width, 1.0), RULE);

    let font = bui_text::shared_font();
    let baseline = y + STATUS_HEIGHT - 7.0;

    // Left segment: NORMAL pill + URL context.
    let mode = "NORMAL";
    let mode_w = font.measure_text(mode, 10.5) + 14.0;
    dl.fill_rounded_rect(
        Rect::new(8.0, y + 4.0, mode_w, STATUS_HEIGHT - 8.0),
        COPPER,
        [3.0; 4],
    );
    let mode_advance = font.measure_text(mode, 10.5);
    dl.commands.push(PaintCommand::Text {
        x: 8.0 + (mode_w - mode_advance) / 2.0,
        baseline,
        advance: mode_advance,
        font_size: 10.5,
        color: Color::WHITE,
        content: mode.to_string(),
    });

    let ctx_x = 8.0 + mode_w + 10.0;
    let ctx = truncate_to_width(url, font, 10.5, (width * 0.5 - ctx_x).max(0.0));
    let ctx_advance = font.measure_text(&ctx, 10.5);
    dl.commands.push(PaintCommand::Text {
        x: ctx_x,
        baseline,
        advance: ctx_advance,
        font_size: 10.5,
        color: STATUS_INK,
        content: ctx,
    });

    // Right segment: scroll %.
    let right_text = format!("scroll {}%", scroll_pct);
    let right_advance = font.measure_text(&right_text, 10.5);
    dl.commands.push(PaintCommand::Text {
        x: width - right_advance - 12.0,
        baseline,
        advance: right_advance,
        font_size: 10.5,
        color: STATUS_INK,
        content: right_text,
    });
}

fn paint_chrome(
    width: u32,
    cursor: (f32, f32),
    url: &str,
    address: &AddressInput,
    _tabs: &[(String, bool)],
    _tab_count: usize,
    can_back: bool,
    can_forward: bool,
    sidebar_w: f32,
    dl: &mut DisplayList,
) {
    let w = width as f32;
    // Slim top bar: nav buttons + URL pill, sitting to the right of the
    // sidebar. The leftmost `sidebar_w` px belong to the sidebar surface
    // (painted separately by `paint_sidebar`) or vanish entirely when
    // ⌘B hides it.
    let bar_x = sidebar_w;
    let bar_w = (w - sidebar_w).max(0.0);
    dl.fill_rect(Rect::new(bar_x, 0.0, bar_w, TOP_BAR_HEIGHT), TOP_BAR_BG);
    // 1-px rule along the bottom of the top bar so the page edge reads.
    dl.fill_rect(
        Rect::new(bar_x, TOP_BAR_HEIGHT - 1.0, bar_w, 1.0),
        TOP_BAR_RULE,
    );

    // Nav buttons — back / forward / reload.
    paint_nav_buttons(cursor, can_back, can_forward, sidebar_w, dl);

    // Address-bar pill. Paper-white surface with a copper focus halo
    // (replaces the blue halo the legacy chrome used).
    let addr_rect = address_bar_rect(width, sidebar_w);
    if address.focused {
        let halo = Rect::new(
            addr_rect.x - 1.5,
            addr_rect.y - 1.5,
            addr_rect.w + 3.0,
            addr_rect.h + 3.0,
        );
        dl.fill_rounded_rect(halo, COPPER_SOFT, [ADDR_PILL_RADIUS + 1.5; 4]);
    }
    dl.fill_rounded_rect(addr_rect, Color::WHITE, [ADDR_PILL_RADIUS; 4]);
    // 1-px ink rule around the pill so it reads against the paper bg.
    let rule_color = if address.focused { COPPER } else { RULE };
    paint_pill_border(addr_rect, ADDR_PILL_RADIUS, rule_color, dl);

    // Right-side action strip (⌘K, split, settings, theme toggle).
    paint_top_bar_actions(width, cursor, sidebar_w, dl);

    let url_font = bui_text::shared_font();
    let text_origin_x = address_text_origin_x(sidebar_w);
    let baseline = addr_rect.y + addr_rect.h * 0.5 + URL_FONT_SIZE * 0.32;
    let max_w = (addr_rect.w - 32.0).max(0.0);

    if address.focused {
        // Selection highlight — drawn behind the glyphs. We use exact
        // proportional widths so the rect tracks each glyph correctly.
        if let Some((s, e)) = address.selection_range() {
            if s < e {
                let sel_x = text_origin_x
                    + prefix_width(url_font, &address.text, URL_FONT_SIZE, s);
                let sel_w = prefix_width(url_font, &address.text, URL_FONT_SIZE, e)
                    - (sel_x - text_origin_x);
                let sel_y = baseline - URL_FONT_SIZE * 0.9;
                let sel_h = URL_FONT_SIZE * 1.25;
                dl.fill_rect(
                    Rect::new(sel_x, sel_y, sel_w, sel_h),
                    Color::rgba(70, 130, 230, 110),
                );
            }
        }
        // Buffer text. v1: no horizontal scroll — long URLs just truncate
        // at the right with an ellipsis. Cursor at far-right may go off
        // the visible region; fixing that is a `view_offset` field on
        // AddressInput, follow-up.
        let text_str: String = address.text.iter().collect();
        let visible = truncate_to_width(&text_str, url_font, URL_FONT_SIZE, max_w);
        let advance = url_font.measure_text(&visible, URL_FONT_SIZE);
        dl.commands.push(PaintCommand::Text {
            x: text_origin_x,
            baseline,
            advance,
            font_size: URL_FONT_SIZE,
            color: URL_TEXT,
            content: visible,
        });
        // Caret — 2px wide. Position by walking the prefix's actual
        // glyph advances rather than assuming a fixed advance.
        let cursor_x = text_origin_x
            + prefix_width(url_font, &address.text, URL_FONT_SIZE, address.cursor);
        let cursor_y = baseline - URL_FONT_SIZE * 0.9;
        let cursor_h = URL_FONT_SIZE * 1.25;
        if cursor_x >= addr_rect.x && cursor_x < addr_rect.x + addr_rect.w {
            dl.fill_rect(
                Rect::new(cursor_x, cursor_y, 2.0, cursor_h),
                Color::rgb(40, 90, 200),
            );
        }
    } else {
        let visible = truncate_to_width(url, url_font, URL_FONT_SIZE, max_w);
        let advance = url_font.measure_text(&visible, URL_FONT_SIZE);
        dl.commands.push(PaintCommand::Text {
            x: text_origin_x,
            baseline,
            advance,
            font_size: URL_FONT_SIZE,
            color: URL_TEXT,
            content: visible,
        });
    }
}

// ---- address-bar geometry ----

/// Width reserved on the right side of the top bar for the action
/// icons (⌘K, split, settings, theme toggle). Computed from the
/// action-button geometry so the URL pill always knows how much room
/// to leave.
const TOP_BAR_ACTIONS: [&str; 4] = ["⌘K", "split", "⚙", "◐"];
const TOP_BAR_ACTION_FONT: f32 = 11.5;
const TOP_BAR_ACTION_HEIGHT: f32 = 24.0;
const TOP_BAR_ACTION_GAP: f32 = 6.0;
const TOP_BAR_ACTION_PAD_X: f32 = 8.0;

fn top_bar_action_widths() -> Vec<f32> {
    let font = bui_text::shared_font();
    TOP_BAR_ACTIONS
        .iter()
        .map(|label| font.measure_text(label, TOP_BAR_ACTION_FONT) + TOP_BAR_ACTION_PAD_X * 2.0)
        .collect()
}

fn top_bar_actions_total_width() -> f32 {
    let widths = top_bar_action_widths();
    let n = widths.len() as f32;
    widths.iter().sum::<f32>() + (n - 1.0).max(0.0) * TOP_BAR_ACTION_GAP
}

fn address_bar_rect(width: u32, sidebar_w: f32) -> Rect {
    let w = width as f32;
    let addr_y = (ADDR_BAR_HEIGHT - ADDR_BG_HEIGHT) / 2.0;
    let x_start = sidebar_w + ADDR_INSET + NAV_BTN_AREA_WIDTH + NAV_BTN_AFTER_GAP;
    let right_reserve = top_bar_actions_total_width() + ADDR_INSET + 6.0;
    let addr_w = (w - x_start - right_reserve).max(0.0);
    Rect::new(x_start, addr_y, addr_w, ADDR_BG_HEIGHT)
}

fn address_text_origin_x(sidebar_w: f32) -> f32 {
    sidebar_w + ADDR_INSET + NAV_BTN_AREA_WIDTH + NAV_BTN_AFTER_GAP + 16.0
}

fn nav_button_rect(idx: usize, sidebar_w: f32) -> Rect {
    let y = (ADDR_BAR_HEIGHT - NAV_BTN_SIZE) / 2.0;
    let x = sidebar_w + ADDR_INSET + idx as f32 * (NAV_BTN_SIZE + NAV_BTN_GAP);
    Rect::new(x, y, NAV_BTN_SIZE, NAV_BTN_SIZE)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavButton {
    Back,
    Forward,
    Reload,
}

fn nav_button_at(x: f32, y: f32, sidebar_w: f32) -> Option<NavButton> {
    for (i, btn) in [NavButton::Back, NavButton::Forward, NavButton::Reload]
        .iter()
        .enumerate()
    {
        let r = nav_button_rect(i, sidebar_w);
        if x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h {
            return Some(*btn);
        }
    }
    None
}

fn address_char_index_at_x(x: f32, text: &[char], sidebar_w: f32) -> usize {
    let local = (x - address_text_origin_x(sidebar_w)).max(0.0);
    cursor_index_at_x(bui_text::shared_font(), text, URL_FONT_SIZE, local)
}

/// Walk `text` accumulating per-glyph advances and return the index
/// whose half-point is just past `x` (CSS-style cursor positioning).
/// Returns `text.len()` if `x` is past the end of the rendered string.
/// Walk a laid-out box tree looking for the LineItem::Control whose
/// originating DOM node matches `target`. Returns the rect we should
/// position cursor / selection inside, plus a snapshot of the relevant
/// computed style. Used by the post-paint cursor overlay.
fn find_input_frame(
    layout: &bui_layout::LayoutBox,
    target: bui_dom::NodeId,
) -> Option<(bui_layout::Frame, bui_style::ComputedValues)> {
    for line in &layout.lines {
        for item in &line.items {
            if let bui_layout::LineItem::Control {
                frame,
                node: Some(n),
                style,
                ..
            } = item
            {
                if *n == target {
                    return Some((*frame, style.clone()));
                }
            }
        }
    }
    for c in &layout.children {
        if let Some(r) = find_input_frame(c, target) {
            return Some(r);
        }
    }
    None
}

/// What clicking on a hit-tested DOM node should do, as far as form
/// controls are concerned. Anything that's neither an editable input
/// nor a submit-style trigger returns `None`, so the caller falls
/// through to the standard link-navigation flow.
enum PageControlAction {
    FocusInput(bui_dom::NodeId),
    /// Carries the enclosing `<form>` whose action URL should be
    /// followed.
    SubmitForm(bui_dom::NodeId),
    None,
}

fn page_control_action(doc: &bui_dom::Document, node: bui_dom::NodeId) -> PageControlAction {
    let Some(elem) = doc.element(node) else { return PageControlAction::None };
    let name = elem.name.as_str();
    let ty = elem
        .get_attr("type")
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    match name {
        "input" => match ty.as_str() {
            "submit" | "image" => match enclosing_form(doc, node) {
                Some(f) => PageControlAction::SubmitForm(f),
                None => PageControlAction::None,
            },
            "button" | "reset" | "checkbox" | "radio" | "file" | "hidden" => {
                PageControlAction::None
            }
            // Default and every text-flavoured type we actually render.
            _ => PageControlAction::FocusInput(node),
        },
        // <textarea> is editable like a text <input>. Google's
        // search box uses one (so the input expands as the user
        // types) — clicking it should focus the page-level input
        // buffer just like any other text field.
        "textarea" => PageControlAction::FocusInput(node),
        "button" => {
            // <button> defaults to type="submit" per the HTML spec.
            let effective = if ty.is_empty() { "submit" } else { ty.as_str() };
            if effective == "submit" {
                match enclosing_form(doc, node) {
                    Some(f) => PageControlAction::SubmitForm(f),
                    None => PageControlAction::None,
                }
            } else {
                PageControlAction::None
            }
        }
        _ => {
            // The hit might have landed on a wrapper div inside a
            // styled search box (Google's `.RNNXgb` rounded pill,
            // `.SDkEP` flex column, etc.). Walk up to the enclosing
            // <form>; if the form has exactly one focusable text
            // input or textarea, treat the click as focusing that
            // single editable target. Mirrors how real browsers
            // forward clicks on a `<label>` to its associated
            // control, but driven by structural heuristic rather
            // than a `for=` attribute (Google doesn't ship one).
            if let Some(form) = enclosing_form(doc, node) {
                let mut text_inputs: Vec<bui_dom::NodeId> = Vec::new();
                for nid in doc.descendants(form) {
                    let Some(e) = doc.element(nid) else { continue };
                    match e.name.as_str() {
                        "textarea" => text_inputs.push(nid),
                        "input" => {
                            let t = e
                                .get_attr("type")
                                .map(|s| s.to_ascii_lowercase())
                                .unwrap_or_default();
                            if !matches!(
                                t.as_str(),
                                "submit" | "button" | "reset" | "image"
                                    | "file" | "checkbox" | "radio" | "hidden"
                            ) {
                                text_inputs.push(nid);
                            }
                        }
                        _ => {}
                    }
                    if text_inputs.len() > 1 {
                        break;
                    }
                }
                if text_inputs.len() == 1 {
                    return PageControlAction::FocusInput(text_inputs[0]);
                }
            }
            PageControlAction::None
        }
    }
}

fn enclosing_form(doc: &bui_dom::Document, node: bui_dom::NodeId) -> Option<bui_dom::NodeId> {
    let mut cur = Some(node);
    while let Some(id) = cur {
        if let Some(e) = doc.element(id) {
            if e.name == "form" {
                return Some(id);
            }
        }
        cur = doc.node(id).parent;
    }
    None
}

/// Build the URL for a form submission: resolve the form's `action`
/// against the page URL, then append a query string assembled from
/// every named `<input>` descendant. Submit-only inputs (button,
/// submit, reset, etc.) don't contribute. Per-input edited buffers
/// override the static `value` attribute.
fn build_form_url(tab: &TabState, form_node: bui_dom::NodeId) -> Option<Url> {
    let doc = tab.doc.lock().unwrap();
    let action_str = doc
        .element(form_node)
        .and_then(|e| e.get_attr("action").map(|s| s.to_string()))
        .unwrap_or_default();
    let mut target = if action_str.is_empty() {
        tab.url.clone()
    } else {
        tab.url.join(&action_str).ok()?
    };
    let mut pairs: Vec<String> = Vec::new();
    for nid in doc.descendants(form_node) {
        let Some(e) = doc.element(nid) else { continue };
        match e.name.as_str() {
            "input" => {
                let ty = e
                    .get_attr("type")
                    .map(|s| s.to_ascii_lowercase())
                    .unwrap_or_else(|| "text".to_string());
                // Submit / image inputs that are themselves the trigger
                // only contribute their value when they're the activated
                // control — but we don't carry that context here.
                // Conservative: skip.
                if matches!(
                    ty.as_str(),
                    "submit" | "button" | "reset" | "image" | "file"
                        | "checkbox" | "radio"
                ) {
                    continue;
                }
                let Some(name) = e.get_attr("name") else { continue };
                let value = tab
                    .page_inputs
                    .get(&nid)
                    .map(|i| i.text_string())
                    .or_else(|| e.get_attr("value").map(|s| s.to_string()))
                    .unwrap_or_default();
                pairs.push(format!("{}={}", urlencode(name), urlencode(&value)));
            }
            "textarea" => {
                // Google's search box is a <textarea name="q">. Pull
                // its current text from the per-input edit buffer if
                // the user typed into it, otherwise fall back to the
                // textarea's child text content (rarely used here).
                let Some(name) = e.get_attr("name") else { continue };
                let value = if let Some(buf) = tab.page_inputs.get(&nid) {
                    buf.text_string()
                } else {
                    let mut text = String::new();
                    let mut child = doc.node(nid).first_child;
                    while let Some(c) = child {
                        if let bui_dom::NodeKind::Text(t) = &doc.node(c).kind {
                            text.push_str(t);
                        }
                        child = doc.node(c).next_sibling;
                    }
                    text.trim().to_string()
                };
                pairs.push(format!("{}={}", urlencode(name), urlencode(&value)));
            }
            "select" => {
                let Some(name) = e.get_attr("name") else { continue };
                // Pick the option whose `selected` attribute is set,
                // or the first one.
                let mut value = String::new();
                let mut first: Option<String> = None;
                for opt_id in doc.descendants(nid) {
                    let Some(opt) = doc.element(opt_id) else { continue };
                    if opt.name != "option" {
                        continue;
                    }
                    let v = opt
                        .get_attr("value")
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| {
                            let mut text = String::new();
                            let mut c = doc.node(opt_id).first_child;
                            while let Some(cc) = c {
                                if let bui_dom::NodeKind::Text(t) = &doc.node(cc).kind {
                                    text.push_str(t);
                                }
                                c = doc.node(cc).next_sibling;
                            }
                            text
                        });
                    if first.is_none() {
                        first = Some(v.clone());
                    }
                    if opt.get_attr("selected").is_some() {
                        value = v;
                        break;
                    }
                }
                if value.is_empty() {
                    if let Some(f) = first {
                        value = f;
                    }
                }
                pairs.push(format!("{}={}", urlencode(name), urlencode(&value)));
            }
            _ => {}
        }
    }
    let query = pairs.join("&");
    target.query = if query.is_empty() { None } else { Some(query) };
    Some(target)
}

/// `application/x-www-form-urlencoded` percent-encoder. Encodes any
/// byte outside the unreserved set; a literal space becomes `+`.
fn urlencode(s: &str) -> String {
    fn unreserved(b: u8) -> bool {
        b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
    }
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b == b' ' {
            out.push('+');
        } else if unreserved(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn cursor_index_at_x(font: &bui_text::Font, text: &[char], font_size: f32, x: f32) -> usize {
    let mut acc = 0.0;
    for (i, &c) in text.iter().enumerate() {
        let w = font.glyph_advance(c, font_size);
        if acc + w * 0.5 > x {
            return i;
        }
        acc += w;
    }
    text.len()
}

/// Total advance of the first `n` chars of `text`, in CSS pixels.
fn prefix_width(font: &bui_text::Font, text: &[char], font_size: f32, n: usize) -> f32 {
    text.iter()
        .take(n)
        .map(|&c| font.glyph_advance(c, font_size))
        .sum()
}

/// Trim `text` so its rendered width fits in `max_w` at `font_size`.
/// When trimming is required we leave room for an ellipsis. Falls
/// back to the full string if it already fits.
fn truncate_to_width(text: &str, font: &bui_text::Font, font_size: f32, max_w: f32) -> String {
    if max_w <= 0.0 {
        return String::new();
    }
    if font.measure_text(text, font_size) <= max_w {
        return text.to_string();
    }
    let ellipsis_w = font.glyph_advance('…', font_size);
    let budget = (max_w - ellipsis_w).max(0.0);
    let mut acc = 0.0;
    let mut out = String::new();
    for ch in text.chars() {
        let w = font.glyph_advance(ch, font_size);
        if acc + w > budget {
            break;
        }
        acc += w;
        out.push(ch);
    }
    out.push('…');
    out
}

fn address_bar_contains(width: u32, x: f32, y: f32, sidebar_w: f32) -> bool {
    let r = address_bar_rect(width, sidebar_w);
    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
}

// ---- input handling ----

#[derive(Debug, Clone, Copy)]
enum TabHit {
    Tab(usize),
    CloseTab(usize),
    NewTab,
}

fn tab_strip_hit_test(width: u32, n_tabs: usize, x: f32, y: f32) -> Option<TabHit> {
    if y < 0.0 || y > TAB_STRIP_HEIGHT {
        return None;
    }
    // Leading reserve belongs to the traffic-light buttons; let macOS
    // handle clicks there.
    if x < TAB_STRIP_LEADING {
        return None;
    }
    let (tab_w, _) = tab_layout(width, n_tabs);
    let mut tx = TAB_STRIP_LEADING;
    for i in 0..n_tabs {
        if x >= tx && x < tx + tab_w {
            if x >= tx + tab_w - CLOSE_BTN_WIDTH {
                return Some(TabHit::CloseTab(i));
            }
            return Some(TabHit::Tab(i));
        }
        tx += tab_w + TAB_GAP;
    }
    let plus_x = tx + 4.0;
    if x >= plus_x && x < plus_x + NEW_TAB_BTN_WIDTH {
        return Some(TabHit::NewTab);
    }
    None
}

/// Compute the cursor icon for the current pointer position. Inspects
/// the active tab's layout (page region only) and walks up the DOM
/// from the hit node looking for the first ancestor whose computed
/// `cursor` style is non-default. Anchors get a pointer fall-back via
/// `enclosing_anchor` so even author CSS that drops `cursor: pointer`
/// on `<a>` keeps the link affordance.
fn cursor_for(state: &Arc<Mutex<BrowserState>>, viewport: Viewport) -> CursorIcon {
    let st = state.lock().unwrap();
    let (cx, cy) = viewport.cursor;
    let sidebar_w = st.sidebar_w();
    // Sidebar region (left strip): tab rows = pointer.
    if cx < sidebar_w {
        let tabs_for_sidebar: Vec<(String, Url, bool)> = st
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| (t.title.clone(), t.url.clone(), i == st.active))
            .collect();
        let h = viewport.height as f32;
        let sl = build_sidebar_layout(h, &tabs_for_sidebar);
        for (_, row, _) in &sl.rows {
            if rect_contains(*row, (cx, cy)) {
                return CursorIcon::Pointer;
            }
        }
        return CursorIcon::Default;
    }
    if cy < CHROME_HEIGHT {
        // Top bar region: address bar = text caret, nav buttons = pointer.
        if address_bar_contains(viewport.width, cx, cy, sidebar_w) {
            return CursorIcon::Text;
        }
        if nav_button_at(cx, cy, sidebar_w).is_some() {
            return CursorIcon::Pointer;
        }
        if top_bar_action_at(viewport.width, (cx, cy), sidebar_w).is_some() {
            return CursorIcon::Pointer;
        }
        return CursorIcon::Default;
    }
    if cy >= viewport.height as f32 - STATUS_HEIGHT {
        return CursorIcon::Default;
    }
    let scroll_y = st.active_tab().scroll_y;
    let layout = match &st.active_tab().layout {
        Some(l) => l,
        None => return CursorIcon::Default,
    };
    let page_x = cx;
    let page_y = (cy - CHROME_HEIGHT) + scroll_y;
    let Some(node) = bui_layout::hit_test(layout, page_x, page_y) else {
        return CursorIcon::Default;
    };
    let doc = st.active_tab().doc.lock().unwrap();
    let style = &st.active_tab().style;
    // Walk up from the hit node — `cursor` is inherited in CSS, but
    // our cascade resets it per element, so an explicit value on an
    // ancestor (like `a { cursor: pointer }`) wouldn't otherwise reach
    // a child text node.
    let mut cur = Some(node);
    while let Some(id) = cur {
        if let Some(cv) = style.get(id) {
            if cv.cursor != bui_style::Cursor::Default {
                return map_style_cursor(cv.cursor);
            }
        }
        cur = doc.node(id).parent;
    }
    // Fallback: any anchor we land in becomes a pointer, even if its
    // declared cursor was reset by author CSS.
    if bui_layout::enclosing_anchor(&doc, node).is_some() {
        return CursorIcon::Pointer;
    }
    CursorIcon::Default
}

fn map_style_cursor(c: bui_style::Cursor) -> CursorIcon {
    match c {
        bui_style::Cursor::Default => CursorIcon::Default,
        bui_style::Cursor::Pointer => CursorIcon::Pointer,
        bui_style::Cursor::Text => CursorIcon::Text,
        bui_style::Cursor::NotAllowed => CursorIcon::NotAllowed,
        bui_style::Cursor::Wait => CursorIcon::Wait,
        bui_style::Cursor::Crosshair => CursorIcon::Crosshair,
        bui_style::Cursor::Move => CursorIcon::Move,
        bui_style::Cursor::Help => CursorIcon::Help,
        bui_style::Cursor::Progress => CursorIcon::Progress,
    }
}

fn handle_click(
    state: &Arc<Mutex<BrowserState>>,
    viewport: Viewport,
    x: f32,
    y: f32,
    mods: bui_shell::Modifiers,
) -> bool {
    let mut st = state.lock().unwrap();
    if st.quit_requested {
        return false;
    }
    let sidebar_w = st.sidebar_w();
    // Sidebar clicks (left strip). Tabs live here in the IDE-Pane layout;
    // a tab body activates the tab, the trailing close 'x' shuts it.
    if x < sidebar_w {
        if st.address_input.focused {
            st.address_input.blur();
        }
        let tabs_for_sidebar: Vec<(String, Url, bool)> = st
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| (t.title.clone(), t.url.clone(), i == st.active))
            .collect();
        let h = viewport.height as f32;
        let sl = build_sidebar_layout(h, &tabs_for_sidebar);
        for (idx, row, close_rect) in &sl.rows {
            if rect_contains(*close_rect, (x, y)) {
                st.active = *idx;
                st.close_active();
                if st.quit_requested {
                    std::process::exit(0);
                }
                return true;
            }
            if rect_contains(*row, (x, y)) {
                st.switch_to(*idx);
                return true;
            }
        }
        return false;
    }
    // Top bar (nav buttons + URL pill).
    if y < CHROME_HEIGHT {
        if let Some(btn) = nav_button_at(x, y, sidebar_w) {
            if st.address_input.focused {
                st.address_input.blur();
            }
            return match btn {
                NavButton::Back => st.active_tab_mut().go_back(),
                NavButton::Forward => st.active_tab_mut().go_forward(),
                NavButton::Reload => st.active_tab_mut().reload(),
            };
        }
        if address_bar_contains(viewport.width, x, y, sidebar_w) {
            // Compute click count for double/triple click — same time +
            // position window as Chrome (~400ms, ~4px).
            let now = std::time::Instant::now();
            let count = match st.last_click {
                Some((t, lx, ly, c))
                    if now.duration_since(t).as_millis() < 400
                        && (lx - x).abs() < 4.0
                        && (ly - y).abs() < 4.0 =>
                {
                    c + 1
                }
                _ => 1,
            };
            st.last_click = Some((now, x, y, count));

            let was_focused = st.address_input.focused;
            if !was_focused {
                let url_str = st.active_tab().url.to_string();
                st.address_input.focus_with(&url_str);
                return true;
            }
            // Already focused — translate the click to a cursor / selection action.
            let pos = address_char_index_at_x(x, &st.address_input.text, sidebar_w);
            match count {
                1 => st.address_input.place_cursor(pos),
                2 => {
                    st.address_input.select_word_at(pos);
                }
                _ => st.address_input.select_all(),
            }
            return true;
        }
        // Click in chrome but outside the pill — blur if focused.
        if st.address_input.focused {
            st.address_input.blur();
            return true;
        }
        return false;
    }
    // Dev-dock click. The header strip switches viewers; body clicks
    // (selection, link follow inside source/console panels) are not
    // wired yet so a body click just falls through.
    if st.dock_open {
        let h = viewport.height as f32;
        let dock_y = h - STATUS_HEIGHT - DOCK_HEIGHT;
        if y >= dock_y && y < dock_y + DOCK_HEADER_HEIGHT {
            for (tab, r) in dock_tab_rects(sidebar_w, dock_y) {
                if rect_contains(r, (x, y)) {
                    st.active_dock_tab = tab;
                    return true;
                }
            }
            return false;
        }
        if y >= dock_y && y < h - STATUS_HEIGHT {
            // Inside the dock body — swallow the click so it doesn't
            // navigate the page underneath.
            return false;
        }
    }
    // Body click. Blur the address bar if it was focused, but still process
    // the link-navigation logic so the click "goes through".
    if st.address_input.focused {
        st.address_input.blur();
    }
    let scroll_y = st.active_tab().scroll_y;
    let layout = match &st.active_tab().layout {
        Some(l) => l,
        None => return true, // we did blur; that warrants a redraw
    };
    let page_x = x;
    let page_y = (y - CHROME_HEIGHT) + scroll_y;
    let hit = bui_layout::hit_test(layout, page_x, page_y);
    // Always start a fresh drag-selection anchor at the click point.
    // The drag handler will extend `end` as the cursor moves; if the
    // mouse is released at the same point, `mouse_up` discards a
    // zero-length selection so a plain click doesn't leave the
    // previous highlight visible. We do this BEFORE the link/form
    // logic so even click-on-text starts a selection.
    {
        let tab = st.active_tab_mut();
        tab.page_selection = Some(PageSelection {
            start: (page_x, page_y),
            end: (page_x, page_y),
            dragging: true,
        });
    }
    let Some(node) = hit else { return true };

    // Form-control click. We classify the hit element as an editable
    // input, a submit-style trigger, or "neither" — then peel off the
    // matching action before falling through to the link path.
    let action = page_control_action(&st.active_tab().doc.lock().unwrap(), node);
    match action {
        PageControlAction::FocusInput(input_node) => {
            // Blur previous, focus this one, seed buffer from the
            // value attribute so existing content shows up immediately.
            // For <textarea>, the initial text comes from the child
            // text nodes, not a `value` attribute (HTML spec).
            let initial = {
                let doc = st.active_tab().doc.lock().unwrap();
                let elem = doc.element(input_node);
                let is_textarea = elem.map(|e| e.name == "textarea").unwrap_or(false);
                if is_textarea {
                    let mut text = String::new();
                    let mut child = doc.node(input_node).first_child;
                    while let Some(c) = child {
                        if let bui_dom::NodeKind::Text(t) = &doc.node(c).kind {
                            text.push_str(t);
                        }
                        child = doc.node(c).next_sibling;
                    }
                    text.trim().to_string()
                } else {
                    elem.and_then(|e| e.get_attr("value")).unwrap_or("").to_string()
                }
            };
            let tab = st.active_tab_mut();
            tab.focused_input = Some(input_node);
            // First-create vs. re-focus: only seed the buffer
            // from the DOM when the entry didn't exist yet.
            // Re-focusing must NOT wipe typed text — for
            // <textarea>, `initial` is the textarea's child text
            // content (per the HTML spec), which Google's search
            // box leaves empty, so re-running the seed cleared
            // every keystroke the user had typed when they
            // clicked the pill again.
            let already_focused = tab.page_inputs.contains_key(&input_node);
            let entry = tab
                .page_inputs
                .entry(input_node)
                .or_insert_with(AddressInput::default);
            if !already_focused {
                entry.text = initial.chars().collect();
                entry.cursor = entry.text.len();
                entry.selection_anchor = Some(0);
            }
            entry.focused = true;
            // Force a relayout next frame so the cursor renders.
            tab.last_width = 0;
            return true;
        }
        PageControlAction::SubmitForm(form_node) => {
            let outcome = st
                .active_tab_mut()
                .dispatch_input_event("submit", form_node);
            if let Some(target) = outcome.pending_nav {
                if let Ok(next_url) = st.active_tab().url.join(&target) {
                    eprintln!("↳ form submit (JS-redirect) → {next_url}");
                    if let Err(e) = st.active_tab_mut().navigate_to(&next_url) {
                        eprintln!("submit redirect failed: {e}");
                    }
                    return true;
                }
            }
            if outcome.default_prevented {
                eprintln!("↳ form submit suppressed by JS handler");
                return true;
            }
            if let Some(url) = build_form_url(st.active_tab(), form_node) {
                eprintln!("↳ form submit → {url}");
                if let Err(e) = st.active_tab_mut().navigate_to(&url) {
                    eprintln!("submit failed: {e}");
                }
            }
            return true;
        }
        PageControlAction::None => {}
    }

    // Click outside any input drops focus on whatever was focused.
    {
        let tab = st.active_tab_mut();
        if tab.focused_input.is_some() {
            tab.focused_input = None;
            tab.last_width = 0;
        }
    }

    let doc = st.active_tab().doc.lock().unwrap();
    let anchor = bui_layout::enclosing_anchor(&doc, node);
    let Some((anchor_node, href)) = anchor else { drop(doc); return true };
    let href = href.to_string();
    let target = doc
        .element(anchor_node)
        .and_then(|e| e.get_attr("target"))
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    drop(doc);
    let new_url = match st.active_tab().url.join(&href) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("bad href {href:?}: {e}");
            return true;
        }
    };
    // Open in a new tab when the user explicitly asks (Cmd / Ctrl
    // click) or the link declares `target="_blank"`. The current
    // tab stays put; we just append a fetched tab and switch to it.
    let new_tab = mods.cmd || mods.ctrl || target == "_blank";
    if new_tab {
        eprintln!("→ opening {new_url} in new tab");
        let s = new_url.to_string();
        st.open_tab(&s);
    } else {
        eprintln!("→ navigating to {new_url}");
        if let Err(e) = st.active_tab_mut().navigate_to(&new_url) {
            eprintln!("navigation failed: {e}");
        }
    }
    true
}

/// Extend an in-progress page selection to the cursor's current
/// position. Fires on every `CursorMoved` while the left button is
/// held — cheap because we just update one struct, no layout work.
fn handle_drag(
    state: &Arc<Mutex<BrowserState>>,
    _viewport: Viewport,
    x: f32,
    y: f32,
    _mods: bui_shell::Modifiers,
) -> bool {
    if y < CHROME_HEIGHT {
        return false;
    }
    let mut st = state.lock().unwrap();
    let scroll_y = st.active_tab().scroll_y;
    let tab = st.active_tab_mut();
    let Some(sel) = tab.page_selection.as_mut() else { return false };
    if !sel.dragging {
        return false;
    }
    sel.end = (x, (y - CHROME_HEIGHT) + scroll_y);
    true
}

/// Mouse-up on the body finishes a drag selection. A "click without
/// drag" produces a zero-length selection — we discard those so a
/// plain click doesn't leave a stale highlight from a previous
/// gesture (and so right-click on empty body doesn't try to copy
/// nothing).
fn handle_mouse_up(
    state: &Arc<Mutex<BrowserState>>,
    _viewport: Viewport,
    _x: f32,
    _y: f32,
    _mods: bui_shell::Modifiers,
) -> bool {
    let mut st = state.lock().unwrap();
    let tab = st.active_tab_mut();
    let Some(sel) = tab.page_selection.as_mut() else { return false };
    sel.dragging = false;
    let dx = sel.end.0 - sel.start.0;
    let dy = sel.end.1 - sel.start.1;
    // A nonzero drag distance is the "real selection" threshold —
    // 1 px keeps the bar low so even a slow draw still selects.
    if dx.abs() < 1.0 && dy.abs() < 1.0 {
        tab.page_selection = None;
    }
    true
}

/// Right-click acts as "copy current selection to clipboard". No
/// context menu UI; if the user has highlighted page text, this
/// is the fastest way to grab it.
fn handle_right_click(
    state: &Arc<Mutex<BrowserState>>,
    _viewport: Viewport,
    _x: f32,
    _y: f32,
    _mods: bui_shell::Modifiers,
) -> bool {
    let mut st = state.lock().unwrap();
    let tab = st.active_tab_mut();
    let Some(layout) = tab.layout.as_ref() else { return false };
    let Some(sel) = tab.page_selection else { return false };
    let text = collect_selected_text(layout, sel);
    if text.is_empty() {
        return false;
    }
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text);
    }
    false
}

/// Iterate every painted text run in document order and return the
/// concatenated content of any run (or run prefix/suffix) inside the
/// selection rectangle. Each line break in the visual flow becomes
/// a `\n` in the output so a multi-line selection round-trips back
/// to readable text on paste.
fn collect_selected_text(root: &LayoutBox, sel: PageSelection) -> String {
    let runs = visit_text_runs(root);
    let (top, bot) = order_selection(sel);
    let font = bui_text::shared_font();
    let mut out = String::new();
    let mut last_y: Option<f32> = None;
    for (frame, text, font_size, _node) in runs {
        let run_top = frame.y;
        let run_bot = frame.y + frame.height;
        // Skip runs entirely above or below the selection.
        if run_bot <= top.1 || run_top >= bot.1 {
            continue;
        }
        let chars: Vec<char> = text.chars().collect();
        // Resolve x bounds within this run. A run is on the "first"
        // line if the selection's top y intersects it; on the "last"
        // line if the bottom y does. Same-line selections (top y and
        // bottom y both in this run) clip to the x range; otherwise
        // pick the appropriate side.
        let same_line = run_top <= top.1 + 0.5 && run_bot >= bot.1 - 0.5;
        let on_first_line = run_top <= top.1 + 0.5 && run_bot >= top.1 - 0.5;
        let on_last_line = run_top <= bot.1 + 0.5 && run_bot >= bot.1 - 0.5;
        let (x_start, x_end) = if same_line {
            (top.0.min(bot.0), top.0.max(bot.0))
        } else if on_first_line {
            (top.0, frame.x + frame.width)
        } else if on_last_line {
            (frame.x, bot.0)
        } else {
            (frame.x, frame.x + frame.width)
        };
        let lo = char_index_at_x(font, &chars, font_size, (x_start - frame.x).max(0.0));
        let hi = char_index_at_x(font, &chars, font_size, (x_end - frame.x).max(0.0));
        let (lo, hi) = (lo.min(hi), lo.max(hi));
        if lo >= hi || lo >= chars.len() {
            continue;
        }
        let slice: String = chars[lo..hi.min(chars.len())].iter().collect();
        if slice.is_empty() {
            continue;
        }
        // Insert a newline when the run is on a different line than
        // the previous emitted run (y center jumps by more than half
        // the run's height). A space joins runs on the same line so
        // adjacent inline elements don't lose word boundaries.
        if let Some(prev_y) = last_y {
            if (frame.y - prev_y).abs() > frame.height * 0.5 {
                out.push('\n');
            } else if !out.ends_with(' ') && !slice.starts_with(' ') {
                out.push(' ');
            }
        }
        out.push_str(&slice);
        last_y = Some(frame.y);
    }
    out
}

/// Order a selection's two endpoints into `(top, bottom)` so callers
/// can reason about "first line" vs "last line" without an
/// orientation check at every call.
fn order_selection(sel: PageSelection) -> ((f32, f32), (f32, f32)) {
    if sel.start.1 < sel.end.1 || (sel.start.1 == sel.end.1 && sel.start.0 <= sel.end.0) {
        (sel.start, sel.end)
    } else {
        (sel.end, sel.start)
    }
}

/// Walk the layout tree and collect every `LineItem::Text` run as
/// `(frame, text, font_size, node)`. Frames are already in absolute
/// page coordinates from the layout pass.
fn visit_text_runs(
    bx: &LayoutBox,
) -> Vec<(bui_layout::Frame, String, f32, Option<bui_dom::NodeId>)> {
    let mut out = Vec::new();
    walk_text_runs(bx, &mut out);
    out
}

fn walk_text_runs(
    bx: &LayoutBox,
    out: &mut Vec<(bui_layout::Frame, String, f32, Option<bui_dom::NodeId>)>,
) {
    // visibility: hidden subtrees still occupy layout but are not
    // selectable text — skip them so a copy doesn't pull invisible
    // ARIA labels or off-screen helpers into the user's clipboard.
    if matches!(bx.style.visibility, bui_style::Visibility::Hidden) {
        return;
    }
    for line in &bx.lines {
        for item in &line.items {
            if let bui_layout::LineItem::Text(run) = item {
                out.push((run.frame, run.text.clone(), run.style.font_size, run.node));
            } else if let bui_layout::LineItem::InlineBlock { host, .. } = item {
                walk_text_runs(host, out);
            }
        }
    }
    for child in &bx.children {
        walk_text_runs(child, out);
    }
}

/// Paint a translucent blue rectangle over each text run inside the
/// selection. `dy` is the chrome offset minus the active scroll —
/// same correction the page-paint loop above uses, so the highlight
/// aligns with the rendered glyphs whatever the scroll position.
fn paint_page_selection(root: &LayoutBox, sel: PageSelection, dy: f32, dl: &mut DisplayList) {
    let runs = visit_text_runs(root);
    let (top, bot) = order_selection(sel);
    let font = bui_text::shared_font();
    for (frame, text, font_size, _node) in runs {
        let run_top = frame.y;
        let run_bot = frame.y + frame.height;
        if run_bot <= top.1 || run_top >= bot.1 {
            continue;
        }
        let chars: Vec<char> = text.chars().collect();
        let same_line = run_top <= top.1 + 0.5 && run_bot >= bot.1 - 0.5;
        let on_first_line = run_top <= top.1 + 0.5 && run_bot >= top.1 - 0.5;
        let on_last_line = run_top <= bot.1 + 0.5 && run_bot >= bot.1 - 0.5;
        let (x_start_abs, x_end_abs) = if same_line {
            (top.0.min(bot.0), top.0.max(bot.0))
        } else if on_first_line {
            (top.0, frame.x + frame.width)
        } else if on_last_line {
            (frame.x, bot.0)
        } else {
            (frame.x, frame.x + frame.width)
        };
        let lo = char_index_at_x(font, &chars, font_size, (x_start_abs - frame.x).max(0.0));
        let hi = char_index_at_x(font, &chars, font_size, (x_end_abs - frame.x).max(0.0));
        let (lo, hi) = (lo.min(hi), lo.max(hi));
        if lo >= hi {
            continue;
        }
        let prefix_w: f32 = chars[..lo].iter().map(|&c| font.glyph_advance(c, font_size)).sum();
        let span_w: f32 = chars[lo..hi.min(chars.len())]
            .iter()
            .map(|&c| font.glyph_advance(c, font_size))
            .sum();
        let hx = frame.x + prefix_w;
        let hy = frame.y + dy;
        dl.fill_rect(
            Rect::new(hx, hy, span_w, frame.height),
            Color::rgba(70, 130, 230, 90),
        );
    }
}

/// Convert a local x offset (relative to the run's left edge) to a
/// character index in `chars`. Used both for selection clipping and
/// for the highlight-rect painter — same algorithm so the visual
/// highlight matches the copied text exactly.
fn char_index_at_x(font: &bui_text::Font, chars: &[char], font_size: f32, x: f32) -> usize {
    if x <= 0.0 {
        return 0;
    }
    let mut acc = 0.0;
    for (i, &c) in chars.iter().enumerate() {
        let w = font.glyph_advance(c, font_size);
        if acc + w * 0.5 > x {
            return i;
        }
        acc += w;
    }
    chars.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a TabState from just title + URL — no fetch — for index-logic
    /// tests that don't care about real DOM contents.
    fn stub_tab(title: &str, url: &str) -> TabState {
        let doc = bui_dom::Document::new();
        let style = bui_style::style_document(&doc, &[]);
        TabState {
            title: title.to_string(),
            url: Url::parse(url).unwrap(),
            net_log: Vec::new(),
            console_log: Vec::new(),
            source_html: String::new(),
            doc: Arc::new(Mutex::new(doc)),
            style,
            body_node: bui_dom::NodeId(0),
            images: bui_layout::ImageRegistry::new(),
            svgs: bui_layout::SvgRegistry::new(),
            layout: None,
            last_width: 0,
            history: Vec::new(),
            forward: Vec::new(),
            scroll_y: 0.0,
            page_inputs: std::collections::HashMap::new(),
            focused_input: None,
            page_selection: None,
            js_ctx: None,
        }
    }

    #[test]
    #[test]
    fn urlencode_encodes_form_chars() {
        assert_eq!(urlencode("hello world"), "hello+world");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencode("Bjørn"), "Bj%C3%B8rn");
        assert_eq!(urlencode("dot.dash-und_score~tilde"), "dot.dash-und_score~tilde");
    }

    #[test]
    fn build_form_url_appends_inputs_as_query() {
        let mut doc = bui_dom::Document::new();
        let body = doc.create_element("body");
        let form = doc.create_element("form");
        doc.element_mut(form).unwrap().set_attr("action", "/search");
        let q = doc.create_element("input");
        doc.element_mut(q).unwrap().set_attr("name", "q");
        doc.element_mut(q).unwrap().set_attr("value", "rust browser");
        let submit = doc.create_element("input");
        doc.element_mut(submit).unwrap().set_attr("type", "submit");
        doc.element_mut(submit).unwrap().set_attr("value", "Go");
        doc.append_child(doc.root, body);
        doc.append_child(body, form);
        doc.append_child(form, q);
        doc.append_child(form, submit);
        let style = bui_style::style_document(&doc, &[]);
        let tab = TabState {
            title: String::new(),
            url: Url::parse("https://example.com/").unwrap(),
            net_log: Vec::new(),
            console_log: Vec::new(),
            source_html: String::new(),
            doc: Arc::new(Mutex::new(doc)),
            style,
            body_node: body,
            images: bui_layout::ImageRegistry::new(),
            svgs: bui_layout::SvgRegistry::new(),
            layout: None,
            last_width: 0,
            history: Vec::new(),
            forward: Vec::new(),
            scroll_y: 0.0,
            page_inputs: std::collections::HashMap::new(),
            focused_input: None,
            page_selection: None,
            js_ctx: None,
        };
        let url = build_form_url(&tab, form).expect("form url");
        assert_eq!(url.path, "/search");
        // The submit input doesn't contribute its value; only `q` does.
        assert_eq!(url.query.as_deref(), Some("q=rust+browser"));
    }

    #[test]
    fn cycle_wraps_both_directions() {
        let mut s = BrowserState::new(stub_tab("a", "https://a.test/"));
        s.tabs.push(stub_tab("b", "https://b.test/"));
        s.tabs.push(stub_tab("c", "https://c.test/"));
        assert_eq!(s.active, 0);
        s.cycle(true);
        assert_eq!(s.active, 1);
        s.cycle(true);
        assert_eq!(s.active, 2);
        s.cycle(true);
        assert_eq!(s.active, 0); // wrapped
        s.cycle(false);
        assert_eq!(s.active, 2); // wrapped backwards
    }

    #[test]
    fn close_active_keeps_index_in_range() {
        let mut s = BrowserState::new(stub_tab("a", "https://a.test/"));
        s.tabs.push(stub_tab("b", "https://b.test/"));
        s.tabs.push(stub_tab("c", "https://c.test/"));
        s.active = 2;
        s.close_active();
        assert_eq!(s.tabs.len(), 2);
        assert_eq!(s.active, 1);
        assert_eq!(s.closed.len(), 1);
        s.active = 0;
        s.close_active();
        assert_eq!(s.tabs.len(), 1);
        assert_eq!(s.active, 0);
    }

    #[test]
    fn close_last_tab_requests_quit() {
        let mut s = BrowserState::new(stub_tab("a", "https://a.test/"));
        s.close_active();
        assert!(s.quit_requested);
    }

    #[test]
    fn reopen_pulls_from_closed_stack() {
        let mut s = BrowserState::new(stub_tab("a", "https://a.test/"));
        s.tabs.push(stub_tab("b", "https://b.test/"));
        s.active = 1;
        s.close_active();
        assert_eq!(s.tabs.len(), 1);
        s.reopen_closed();
        assert_eq!(s.tabs.len(), 2);
        assert_eq!(s.tabs[1].title, "b");
        assert_eq!(s.active, 1);
    }

    #[test]
    fn switch_to_clamps_to_existing_tabs() {
        let mut s = BrowserState::new(stub_tab("a", "https://a.test/"));
        s.tabs.push(stub_tab("b", "https://b.test/"));
        s.switch_to(99); // out of range, ignored
        assert_eq!(s.active, 0);
        s.switch_to(1);
        assert_eq!(s.active, 1);
    }

    fn make_state() -> Arc<Mutex<BrowserState>> {
        Arc::new(Mutex::new(BrowserState::new(stub_tab(
            "Example",
            "https://example.com/",
        ))))
    }

    fn key(c: char, mods: bui_shell::Modifiers) -> KeyPress {
        KeyPress {
            key: Key::Char(c),
            modifiers: mods,
            repeat: false,
        }
    }

    fn key_named(k: Key, mods: bui_shell::Modifiers) -> KeyPress {
        KeyPress { key: k, modifiers: mods, repeat: false }
    }

    fn cmd_only() -> bui_shell::Modifiers {
        bui_shell::Modifiers {
            cmd: true,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    fn no_mods() -> bui_shell::Modifiers {
        bui_shell::Modifiers::default()
    }

    fn mid_address_bar(width: u32) -> (f32, f32) {
        // Tests run with sidebar_open=true by default, so use the
        // full SIDEBAR_WIDTH offset to match the rendered geometry.
        let r = address_bar_rect(width, SIDEBAR_WIDTH);
        (r.x + r.w / 2.0, r.y + r.h / 2.0)
    }

    #[test]
    fn click_address_bar_focuses_and_selects_all() {
        let state = make_state();
        let viewport = Viewport { width: 1280, height: 800, cursor: (0.0, 0.0) };
        let (x, y) = mid_address_bar(viewport.width);
        let redraw = handle_click(&state, viewport, x, y, bui_shell::Modifiers::default());
        assert!(redraw);
        let st = state.lock().unwrap();
        assert!(st.address_input.focused);
        assert_eq!(
            st.address_input.text.iter().collect::<String>(),
            "https://example.com/"
        );
        assert_eq!(
            st.address_input.selection_range(),
            Some((0, "https://example.com/".len()))
        );
    }

    #[test]
    fn cmd_l_focuses_address_bar() {
        let state = make_state();
        let redraw = handle_key(&state, key('l', cmd_only()));
        assert!(redraw);
        let st = state.lock().unwrap();
        assert!(st.address_input.focused);
    }

    #[test]
    fn typing_replaces_selection_via_key_path() {
        let state = make_state();
        // Focus → select all.
        handle_key(&state, key('l', cmd_only()));
        // Type 'A' (no modifiers) — replaces selection.
        handle_key(&state, key('A', no_mods()));
        let st = state.lock().unwrap();
        assert_eq!(st.address_input.text_string(), "A");
        assert_eq!(st.address_input.cursor, 1);
        assert!(st.address_input.selection_anchor.is_none());
    }

    #[test]
    fn escape_blurs_without_changing_url() {
        let state = make_state();
        handle_key(&state, key('l', cmd_only()));
        // Type something so the buffer differs from the URL.
        handle_key(&state, key('x', no_mods()));
        // Escape blurs without navigating.
        let redraw = handle_key(&state, key_named(Key::Escape, no_mods()));
        assert!(redraw);
        let st = state.lock().unwrap();
        assert!(!st.address_input.focused);
        // Tab URL is unchanged.
        assert_eq!(st.active_tab().url.to_string(), "https://example.com/");
    }

    #[test]
    fn click_outside_pill_blurs() {
        let state = make_state();
        // Focus first.
        handle_key(&state, key('l', cmd_only()));
        // Click below the chrome (in the body region) — would normally
        // navigate via link, but the stub layout has no clickable anchors.
        let viewport = Viewport { width: 1280, height: 800, cursor: (0.0, 0.0) };
        handle_click(&state, viewport, 300.0, 400.0, bui_shell::Modifiers::default());
        let st = state.lock().unwrap();
        assert!(!st.address_input.focused);
    }

    #[test]
    fn cmd_t_passes_through_when_focused() {
        let state = make_state();
        // Focus address bar first.
        handle_key(&state, key('l', cmd_only()));
        // ⌘T should still open a new tab — but since the open-tab path
        // does network I/O, we can't run it here. We just verify that
        // the edit handler returns `None` (delegate) for ⌘T.
        let mut st = state.lock().unwrap();
        let press = key('t', cmd_only());
        let edit_result = handle_edit_key(&mut st, press);
        assert!(edit_result.is_none(), "⌘T should not be consumed by edit handler");
    }

    #[test]
    fn arrow_clears_selection_then_moves() {
        let state = make_state();
        handle_key(&state, key('l', cmd_only()));
        // Right arrow collapses selection to the end.
        handle_key(&state, key_named(Key::ArrowRight, no_mods()));
        let st = state.lock().unwrap();
        assert_eq!(st.address_input.cursor, "https://example.com/".len());
        assert!(st.address_input.selection_anchor.is_none());
    }

    #[test]
    fn shift_arrow_extends_selection() {
        let state = make_state();
        handle_key(&state, key('l', cmd_only()));
        // Right collapses to end, then Shift+Left extends a one-char selection.
        handle_key(&state, key_named(Key::ArrowRight, no_mods()));
        let shift = bui_shell::Modifiers {
            shift: true,
            ..no_mods()
        };
        handle_key(&state, key_named(Key::ArrowLeft, shift));
        let st = state.lock().unwrap();
        let len = "https://example.com/".len();
        assert_eq!(st.address_input.selection_range(), Some((len - 1, len)));
    }

    #[test]
    fn sidebar_layout_locates_tab_rows_and_close_buttons() {
        // IDE-Pane shell: tabs live in the sidebar, not the top strip.
        // Each row is hit-testable for activation (full width) and close
        // (right-edge sub-rect).
        let tabs = vec![
            (
                "GitHub".to_string(),
                Url::parse("https://github.com/").unwrap(),
                false,
            ),
            (
                "Issues".to_string(),
                Url::parse("https://github.com/copper").unwrap(),
                true,
            ),
            (
                "HN".to_string(),
                Url::parse("https://news.ycombinator.com/").unwrap(),
                false,
            ),
        ];
        let sl = build_sidebar_layout(800.0, &tabs);
        assert_eq!(sl.rows.len(), 3);
        // Active tab (index 1) is on the same host as tab 0 so it
        // sits in the same group; tab 2 is on a different host so a
        // header pushes it down: tab[2].row.y > tab[1].row.y + row_h.
        let (_, row0, close0) = sl.rows[0];
        let (_, row1, _) = sl.rows[1];
        let (_, row2, _) = sl.rows[2];
        assert!(row1.y > row0.y);
        assert!(row2.y > row1.y + row1.h);
        // Close button is fully inside the row, right-aligned.
        assert!(close0.x + close0.w <= row0.x + row0.w + 0.5);
        assert!(close0.x >= row0.x + row0.w - SIDEBAR_CLOSE_W - 0.5);
        // Rows are sized to fit inside the sidebar.
        assert!(row0.x + row0.w <= SIDEBAR_WIDTH);
    }

    #[test]
    fn google_search_is_pass_through() {
        // The function is currently a no-op (see its docstring):
        // we route real Google traffic to Google. The user sees
        // the "please enable JS" notice until the JS-engine
        // milestone (docs/js-engine-plan.md) lands. Once it does
        // we'll either delete this test or repurpose it to assert
        // that the modern Google homepage renders + submits.
        fn rw(s: &str) -> String {
            let u = Url::parse(s).unwrap();
            maybe_rewrite_google_search(&u).to_string()
        }
        for raw in [
            "https://google.com/search?q=x",
            "https://www.google.de/search?q=x",
            "https://scholar.google.com/search?q=x",
            "https://google.evilattacker.com/search?q=x",
            "https://google.de/maps?q=x",
        ] {
            assert_eq!(rw(raw), raw);
        }
    }

    #[test]
    fn selection_collects_text_across_paragraphs() {
        // Two paragraphs stacked vertically. A selection that spans
        // both should pick up text from both, with a newline between
        // them (because their y-centers are far apart).
        let html = "<body><p>Hello world</p><p>Second line</p></body>";
        let doc = bui_html::parse(html);
        let style = bui_style::style_document(&doc, &[]);
        let body = doc
            .descendants(doc.root)
            .find(|n| doc.element(*n).map(|e| e.name == "body").unwrap_or(false))
            .unwrap();
        bui_style::set_viewport(800.0, 600.0);
        let mut bx = bui_layout::build(&doc, &style, body);
        bui_layout::layout(&mut bx, 0.0, 0.0, 800.0);
        // Sanity: at least two text runs were emitted.
        let runs = visit_text_runs(&bx);
        assert!(runs.len() >= 2, "expected ≥2 text runs, got {}", runs.len());
        // Span from the top-left of the first run to past the end of
        // the last run.
        let first = &runs[0];
        let last = runs.last().unwrap();
        let sel = PageSelection {
            start: (first.0.x, first.0.y + 1.0),
            end: (
                last.0.x + last.0.width + 100.0,
                last.0.y + last.0.height - 1.0,
            ),
            dragging: false,
        };
        let text = collect_selected_text(&bx, sel);
        assert!(text.contains("Hello"), "selection missing 'Hello': {text:?}");
        assert!(text.contains("Second"), "selection missing 'Second': {text:?}");
        assert!(text.contains('\n'), "expected newline between paragraphs: {text:?}");
    }
}

fn handle_key(state: &Arc<Mutex<BrowserState>>, press: KeyPress) -> bool {
    let mods = press.modifiers;
    let mut st = state.lock().unwrap();
    if st.quit_requested {
        return false;
    }

    // ⌘L always focuses the address bar — works whether already focused or
    // not, just like Chrome.
    if mods.cmd && key_lower(press.key) == Some('l') {
        let url_str = st.active_tab().url.to_string();
        st.address_input.focus_with(&url_str);
        // Steal page focus too — only one editable thing at a time.
        st.active_tab_mut().focused_input = None;
        return true;
    }

    // Page input has priority over the address bar. We claim every
    // typing-style key here so users can't accidentally trigger a
    // browser hotkey while typing a search query.
    if st.active_tab().focused_input.is_some() {
        if let Some(redraw) = handle_page_input_key(&mut st, press) {
            return redraw;
        }
    }

    // While the address bar is editing, route most keys into the input.
    if st.address_input.focused {
        if let Some(redraw) = handle_edit_key(&mut st, press) {
            return redraw;
        }
        // Edit handler said "not mine" (e.g. ⌘T while focused) — fall
        // through to browser-level hotkeys below.
    }

    // ⌃Tab cycle.
    if mods.ctrl && press.key == Key::Tab {
        st.cycle(!mods.shift);
        return true;
    }
    // Keyboard scroll. Only when the address bar isn't focused
    // and there's no in-page focused input — otherwise these keys
    // should edit text. We use the page render area for the
    // page-up/down step so each press moves about a screenful.
    let page_step = 600.0_f32;
    let line_step = 60.0_f32;
    let scroll_delta: f32 = match press.key {
        Key::ArrowDown => line_step,
        Key::ArrowUp => -line_step,
        Key::PageDown => page_step,
        Key::PageUp => -page_step,
        Key::Home => f32::NEG_INFINITY,
        Key::End => f32::INFINITY,
        Key::Char(' ') if !mods.shift => page_step,
        Key::Char(' ') if mods.shift => -page_step,
        _ => 0.0,
    };
    if scroll_delta != 0.0 {
        let tab = st.active_tab_mut();
        tab.scroll_y = if scroll_delta == f32::NEG_INFINITY {
            0.0
        } else if scroll_delta == f32::INFINITY {
            // Approximate end-of-page; clamp to the layout's content
            // height when available, else just advance a lot.
            tab.layout
                .as_ref()
                .map(|bx| bx.frame.height)
                .unwrap_or(99_999.0)
        } else {
            (tab.scroll_y + scroll_delta).max(0.0)
        };
        return true;
    }
    if !mods.cmd {
        return false;
    }
    // ⌘C / ⌃C copies the current page selection. Only fires when no
    // editable input owns the keyboard — those handlers above
    // already short-circuit and run their own copy paths.
    if key_lower(press.key) == Some('c') {
        let tab = st.active_tab();
        if let (Some(layout), Some(sel)) = (tab.layout.as_ref(), tab.page_selection) {
            let text = collect_selected_text(layout, sel);
            if !text.is_empty() {
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    let _ = cb.set_text(text);
                }
                return false;
            }
        }
    }
    // ⌘J — toggle the dev-dock. The page reflows to fit the
    // shrunken/expanded viewport on next paint (last_width=0 force).
    if key_lower(press.key) == Some('j') {
        st.dock_open = !st.dock_open;
        st.active_tab_mut().last_width = 0;
        return true;
    }
    // ⌘B — toggle the sidebar. Same reflow trigger.
    if key_lower(press.key) == Some('b') {
        st.sidebar_open = !st.sidebar_open;
        st.active_tab_mut().last_width = 0;
        return true;
    }
    match key_lower(press.key) {
        Some('t') if mods.shift => {
            st.reopen_closed();
            true
        }
        Some('t') => {
            st.open_tab(HOME_URL);
            // Focus the URL bar so the user can immediately type a new URL,
            // matching Chrome's "new-tab → ready to type" expectation.
            // The buffer starts empty (drop the placeholder URL) so the
            // first keystroke begins a fresh address.
            st.address_input.focus_with("");
            // focus_with selects all by default; nothing to select since
            // it's empty, so just leave anchor cleared and cursor at 0.
            st.address_input.selection_anchor = None;
            true
        }
        Some('w') => {
            st.close_active();
            if st.quit_requested {
                std::process::exit(0);
            }
            true
        }
        Some('r') => st.active_tab_mut().reload(),
        Some('[') => st.active_tab_mut().go_back(),
        Some(']') => st.active_tab_mut().go_forward(),
        Some(c @ '1'..='8') => {
            let idx = (c as u32 - '1' as u32) as usize;
            st.switch_to(idx);
            true
        }
        Some('9') => {
            let last = st.tabs.len().saturating_sub(1);
            st.switch_to(last);
            true
        }
        _ => false,
    }
}

fn key_lower(k: Key) -> Option<char> {
    if let Key::Char(c) = k {
        Some(c.to_ascii_lowercase())
    } else {
        None
    }
}

/// Editing keys for the focused page `<input>`. Mirrors
/// `handle_edit_key`'s contract: `Some(redraw)` when consumed,
/// `None` to fall through. Enter submits the enclosing form.
fn handle_page_input_key(st: &mut BrowserState, press: KeyPress) -> Option<bool> {
    let mods = press.modifiers;
    let focus = st.active_tab().focused_input?;
    match press.key {
        Key::Escape => {
            st.active_tab_mut().focused_input = None;
            if let Some(input) = st.active_tab_mut().page_inputs.get_mut(&focus) {
                input.focused = false;
                input.selection_anchor = None;
            }
            st.active_tab_mut().last_width = 0;
            Some(true)
        }
        Key::Enter => {
            // Submit the enclosing form, if any. We re-resolve via
            // enclosing_form so paste-in HTML without a wrapping <form>
            // (e.g. a hand-built test page) just clears focus instead
            // of crashing.
            let form = enclosing_form(&st.active_tab().doc.lock().unwrap(), focus);
            st.active_tab_mut().focused_input = None;
            if let Some(input) = st.active_tab_mut().page_inputs.get_mut(&focus) {
                input.focused = false;
            }
            st.active_tab_mut().last_width = 0;
            if let Some(form_node) = form {
                let outcome = st
                    .active_tab_mut()
                    .dispatch_input_event("submit", form_node);
                if let Some(target) = outcome.pending_nav {
                    if let Ok(next_url) = st.active_tab().url.join(&target) {
                        eprintln!("↳ form submit (Enter, JS-redirect) → {next_url}");
                        if let Err(e) = st.active_tab_mut().navigate_to(&next_url) {
                            eprintln!("submit redirect failed: {e}");
                        }
                        return Some(true);
                    }
                }
                if outcome.default_prevented {
                    eprintln!("↳ form submit (Enter) suppressed by JS handler");
                    return Some(true);
                }
                if let Some(url) = build_form_url(st.active_tab(), form_node) {
                    eprintln!("↳ form submit (Enter) → {url}");
                    if let Err(e) = st.active_tab_mut().navigate_to(&url) {
                        eprintln!("submit failed: {e}");
                    }
                }
            }
            Some(true)
        }
        Key::Backspace => {
            if let Some(input) = st.active_tab_mut().page_inputs.get_mut(&focus) {
                input.backspace();
            }
            st.active_tab_mut().last_width = 0;
            Some(true)
        }
        Key::Delete => {
            if let Some(input) = st.active_tab_mut().page_inputs.get_mut(&focus) {
                input.delete_forward();
            }
            st.active_tab_mut().last_width = 0;
            Some(true)
        }
        Key::ArrowLeft => {
            if let Some(input) = st.active_tab_mut().page_inputs.get_mut(&focus) {
                let step = if mods.alt { Step::Word(-1) } else { Step::Char(-1) };
                input.move_by(step, mods.shift);
            }
            Some(true)
        }
        Key::ArrowRight => {
            if let Some(input) = st.active_tab_mut().page_inputs.get_mut(&focus) {
                let step = if mods.alt { Step::Word(1) } else { Step::Char(1) };
                input.move_by(step, mods.shift);
            }
            Some(true)
        }
        Key::Home => {
            if let Some(input) = st.active_tab_mut().page_inputs.get_mut(&focus) {
                input.move_by(Step::LineStart, mods.shift);
            }
            Some(true)
        }
        Key::End => {
            if let Some(input) = st.active_tab_mut().page_inputs.get_mut(&focus) {
                input.move_by(Step::LineEnd, mods.shift);
            }
            Some(true)
        }
        Key::Char(c) => {
            // Skip control-modifier combos so ⌘A / ⌘C still work.
            if mods.cmd || mods.ctrl {
                return None;
            }
            if let Some(input) = st.active_tab_mut().page_inputs.get_mut(&focus) {
                input.insert_char(c);
            }
            st.active_tab_mut().last_width = 0;
            Some(true)
        }
        _ => None,
    }
}

/// Returns `Some(redraw)` when the press was consumed by the edit input,
/// `None` to let the caller fall through to browser-level hotkeys.
fn handle_edit_key(st: &mut BrowserState, press: KeyPress) -> Option<bool> {
    let mods = press.modifiers;
    match press.key {
        Key::Escape => {
            st.address_input.blur();
            Some(true)
        }
        Key::Enter => {
            let typed: String = st.address_input.text.iter().collect();
            st.address_input.blur();
            let trimmed = typed.trim();
            if !trimmed.is_empty() {
                let candidate = if trimmed.starts_with("http://") || trimmed.starts_with("https://")
                {
                    trimmed.to_string()
                } else {
                    format!("https://{trimmed}")
                };
                match Url::parse(&candidate) {
                    Ok(url) => {
                        if let Err(e) = st.active_tab_mut().navigate_to(&url) {
                            eprintln!("navigation failed: {e}");
                        }
                    }
                    Err(e) => eprintln!("invalid URL {trimmed:?}: {e}"),
                }
            }
            Some(true)
        }
        Key::Backspace => {
            st.address_input.backspace();
            Some(true)
        }
        Key::Delete => {
            st.address_input.delete_forward();
            Some(true)
        }
        Key::ArrowLeft => {
            let step = if mods.cmd {
                Step::LineStart
            } else if mods.alt {
                Step::Word(-1)
            } else {
                Step::Char(-1)
            };
            st.address_input.move_by(step, mods.shift);
            Some(true)
        }
        Key::ArrowRight => {
            let step = if mods.cmd {
                Step::LineEnd
            } else if mods.alt {
                Step::Word(1)
            } else {
                Step::Char(1)
            };
            st.address_input.move_by(step, mods.shift);
            Some(true)
        }
        Key::Home => {
            st.address_input.move_by(Step::LineStart, mods.shift);
            Some(true)
        }
        Key::End => {
            st.address_input.move_by(Step::LineEnd, mods.shift);
            Some(true)
        }
        Key::Char(c) => {
            if mods.cmd {
                match c.to_ascii_lowercase() {
                    'a' => {
                        st.address_input.select_all();
                        Some(true)
                    }
                    'c' => {
                        clipboard_copy(&st.address_input);
                        Some(true)
                    }
                    'x' => {
                        clipboard_copy(&st.address_input);
                        st.address_input.delete_selection();
                        Some(true)
                    }
                    'v' => {
                        if let Some(text) = clipboard_paste() {
                            // URL bar is single-line; collapse newlines.
                            let cleaned: String = text
                                .chars()
                                .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                                .collect();
                            st.address_input.insert_str(&cleaned);
                        }
                        Some(true)
                    }
                    // Other ⌘-modified keys (⌘T, ⌘W, ⌘R, ...) propagate
                    // up to the browser-level handler.
                    _ => None,
                }
            } else {
                // Plain typing (or Shift+char already case-folded by winit).
                // Skip control codes like Tab that might surface here.
                if !c.is_control() {
                    st.address_input.insert_char(c);
                    Some(true)
                } else {
                    None
                }
            }
        }
        _ => None,
    }
}

fn clipboard_copy(input: &AddressInput) {
    let Some(text) = input.selected_text() else { return };
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text);
    }
}

fn clipboard_paste() -> Option<String> {
    let mut cb = arboard::Clipboard::new().ok()?;
    cb.get_text().ok()
}
