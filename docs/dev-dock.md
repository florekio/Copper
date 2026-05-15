# Dev-dock

A persistent panel at the bottom of the window with three tabs:
**XHR**, **Console**, **Source**. Toggle visibility with `⌘J`.
Click a tab pill in the dock header to switch viewers.

Every navigation (typing a URL, clicking a link, back/forward,
reload) refreshes the three captures. Closing and reopening a tab
preserves its snapshot.

## XHR

The XHR tab is a waterfall of every HTTP fetch made during the
page load: the main HTML document, every `<link rel=stylesheet>`,
every `<img>` and `<image>` resource. Each row shows:

```
[METHOD pill] [URL...........]  [████░░░░  bar]  [status·ms]  [bytes]
```

- **Method pill** — coloured by verb. Copper-deep for `GET`,
  copper for `POST` / `PUT` / `PATCH`, red for `DELETE`,
  ink-3 for anything else.
- **URL** — truncated with ellipsis to fit its column.
- **Bar** — width is `entry.ms / slowest_ms` of the slowest fetch
  in the log, so a 5-ms fetch and a 5000-ms fetch share the same
  visual scale.
- **status·ms** — the HTTP status code and elapsed milliseconds.
  Non-2xx tints copper.
- **bytes** — response body length, human-formatted (`B` / `KB` / `MB`).

The header strip shows a live summary: `47 req · ⌘J toggle`.

### Where capture happens

A static `Mutex<Vec<NetEntry>>` (see `NET_CAPTURE` in
`crates/bui/src/main.rs`) is cleared at the start of
`TabState::fetch` and drained into the tab's `net_log` after the
page is built. The four fetch sites — page, stylesheet, image,
background-image — call `net_record(method, url, status, ms, bytes)`
as they complete. The dock just reads `tab.net_log` per frame.

## Console

The Console tab shows JS console output captured during inline
`<script>` execution. The browser runs scripts through Zinc
(`bui_js::execute_inline_scripts_with_dom`) at fetch time;
anything passed to `console.log` lands in `tab.console_log` and
the dock surfaces it.

If the page has no `<script>` tags or no logging happens, the tab
shows an empty-state hint: *"no console messages — try a page with
<script>"*.

Future work: capture CSS parse warnings, image decode failures,
and HTTP errors here too.

## Source

The Source tab shows the raw HTML body of the active page with a
4-digit line gutter. The buffer is capped at 64 KB so a 5 MB
response doesn't blow up the dock's text-run count. If the page
is bigger than that, the tail is replaced with `…(truncated)`.

This is the response body as it came off the wire (lossy-decoded
UTF-8), not a pretty-printed DOM dump. For the DOM tree use
`copper --parse <url>` on the command line.

## Toggling and resizing

- `⌘J` toggles the dock open / closed. The viewport reflows on
  the next paint so the page fills the freed space.
- The dock is fixed at 180 px tall. Resizing is on the roadmap.

## When the dock looks empty

If you open the dock and see no XHR rows on a real site, the most
likely cause is that the navigation happened *before* the
dev-dock was wired across `navigate_to` / `go_back` / `go_forward`
/ `reload`. As of commit `c01a5f1` the capture is carried over —
reload the page and rows should appear.
