# docs/

Long-form documentation for Copper. The repo's top-level
[`README.md`](../README.md) covers what the project is and how to
build it; everything below digs into one area in depth.

## Index

| Doc | What it covers |
|---|---|
| [architecture.md](architecture.md) | Crate graph, end-to-end pipeline (HTTP bytes → pixels), what each crate owns, deliberate deferrals. |
| [keybindings.md](keybindings.md) | Every shortcut the chrome wires today, with macOS / Linux equivalents. |
| [dev-dock.md](dev-dock.md)         | XHR / Console / Source viewers — what they show, where capture happens. |
| [contributing.md](contributing.md) | Setup, code style, conventions, how to add a CSS property or a chrome surface. |
| [js-engine-plan.md](js-engine-plan.md) | Six-phase roadmap from the current "JS stubs only" state to real Google search end-to-end (Zinc upvalue fix → host-object alloc → addEventListener firing → fetch/XHR → setTimeout → drop fallback). |
| [screenshot.png](screenshot.png)   | Reference render of Copper showing the IDE-Pane chrome on Wikipedia. |
