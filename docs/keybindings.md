# Keybindings

Everything that's keyboard-reachable today. Modifier keys are
shown with `⌘` (Command on macOS, Ctrl on Linux). Where the macOS
key differs from Linux, both are listed.

## Navigation

| Key | Action |
|---|---|
| `⌘L`            | Focus the URL bar (selects existing URL) |
| `⌘T`            | Open new tab, focus URL bar empty |
| `⌘W`            | Close active tab (quits if last tab) |
| `⌘⇧T`           | Reopen the last closed tab |
| `⌘R`            | Reload the active page |
| `⌘[`            | Back |
| `⌘]`            | Forward |
| `⌃Tab`          | Cycle to next tab |
| `⌃⇧Tab`         | Cycle to previous tab |
| `⌘1`–`⌘8`       | Jump to tab N |
| `⌘9`            | Jump to last tab |

## Chrome toggles

| Key | Action |
|---|---|
| `⌘B`            | Toggle sidebar (tab tree) |
| `⌘J`            | Toggle dev-dock (XHR / Console / Source) |

## Page scrolling

Active only when no input is focused.

| Key | Action |
|---|---|
| `↓`             | Line down |
| `↑`             | Line up |
| `Page Down`     | Page down |
| `Page Up`       | Page up |
| `Space`         | Page down |
| `⇧Space`        | Page up |
| `Home`          | Top of document |
| `End`           | Bottom of document |

## Selection / clipboard

| Key | Action |
|---|---|
| `⌘C`            | Copy current page selection |
| Drag            | Select page text |
| Right-click     | Copy selection (context-free for now) |

## Inside an `<input>` / URL bar

Standard edit motions: arrow keys, `Home`, `End`, `Backspace`,
`Delete`, `⌥←` / `⌥→` for word jumps, `⌘A` to select all,
`⌘C` / `⌘V` / `⌘X` for clipboard.

## Coming soon

These belong to the design plan but aren't wired yet:

- `⌘K`           — Command palette overlay
- `⌘\`           — Split pane right (Phase 3, tiling)
- `f`, `gt`, `gT`, `d`, `u`, `j/k/gg/G`, `m{a-z}`, `'{a-z}` —
  Normal-mode keys (vim-style, Phase 1.5)
