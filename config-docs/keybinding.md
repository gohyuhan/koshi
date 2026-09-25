# `keybinding.kdl` — key bindings

User bindings are applied over built-in bindings. A user binding wins for the
same key. Unchanged keys keep built-in actions.

**Where it goes:** directly in the config directory —
`~/.config/koshi/keybinding.kdl` on Linux,
`~/Library/Application Support/koshi/keybinding.kdl` on macOS,
`%APPDATA%\koshi\config\keybinding.kdl` on Windows. See
[README](README.md#where-the-files-go).

**Any error drops the whole file.** `koshi keys conflicts` reports problems.
`koshi keys list` prints the active keymap.

## Top-level settings

`version` is required. Every other setting is optional and may appear at most
once beside the `mode` blocks.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `chord-timeout-ms` | integer — ms to wait for the next key in a multi-key shortcut before giving up | `500` | ≥ 0.1.0 |
| `which-key-delay-ms` | integer — ms before the hint bar shows the keys that continue a shortcut | `300` | ≥ 0.1.0 |
| `max-chord-depth` | integer 0–255 — most keys a single shortcut may chain. A shortcut longer than this can never fire, and `koshi keys conflicts` reports it | `4` | ≥ 0.1.0 |
| `leader` | string — the key `<leader>` stands for in bindings; a modifier run like `"C-"` or a single chord like `"<Space>"` | `"C-"` (the Ctrl modifier run) | ≥ 0.1.0 |
| `unlock-alternative` | string — the chord that leaves locked mode, in place of `<C-l>`. Setting it frees `<C-l>` to rebind, and the whole file is rejected unless the `locked` block also binds this chord to `core:unlock`. A chord plain typing produces is rejected | (none) | ≥ 0.1.0 |

## Key grammar

Keys use angle brackets. Spaces join keys into a sequence.

- `<C-t>` — Ctrl+t. Modifiers: `C` Ctrl, `A` Alt, `S` Shift.
- `<Tab>`, `<CR>`, `<Esc>`, `<BS>` — named keys. A bare word like `Tab` is read
  as one chord *per character* (`T`, `a`, `b`), so always bracket named keys.
- `<leader>p` — leader plus `p`. Leader `"C-"` makes `<C-p>`. Leader
  `"<Space>"` makes `<Space>` then `p`. Explicit keys such as `<A-f>` do not
  move when the leader changes.

## `mode` blocks

A `mode` block holds the bindings for one built-in keymap — `"normal"`,
`"locked"`, or `"pane-placement"`. Normal and locked are base modes.
Pane placement is a local mode that takes priority while a pane placement is
open.

- `bind "<key>" "core:action"` — exactly two strings: the key sequence and the
  **full** action reference (namespaced, e.g. `core:new-tab`; a bare `new-tab`
  is rejected with a hint). A `bind` takes no arguments — a fixed choice lives
  in the action name (`core:new-pane-left`), an open value lives in the CLI.
- `remove "<key>"` — void a key in this mode, so lower layers no longer bind it.

Run `koshi actions list` to see every action name you can bind to.

## Full example

Complete built-in keymap. `<leader>` bindings follow the configured leader.
`<C-l>`, `<A-f>`, Tab, and Shift+Tab stay fixed.

```kdl
// keybinding.kdl — the complete default keymap.
version 2

chord-timeout-ms 500
which-key-delay-ms 300
max-chord-depth 4
leader "C-"                      // the leader = the Ctrl modifier run (default)
// unlock-alternative "<A-u>"    // replaces <C-l>; also bind it to core:unlock in "locked"

mode "normal" {
    // reserved lock/unlock chord — explicit, never moves with the leader
    bind "<C-l>" "core:lock"

    // leader-relative — rebind `leader` and all of these move together
    bind "<leader>q" "core:quit"
    bind "<leader>g" "core:mouse-select"
    bind "<leader>p n" "core:new-pane"          // <C-p> n with the default leader
    bind "<leader>p h" "core:new-pane-left"
    bind "<leader>p j" "core:new-pane-down"
    bind "<leader>p k" "core:new-pane-up"
    bind "<leader>p l" "core:new-pane-right"
    bind "<leader>p s" "core:new-pane-stacked"  // <C-p> s: a new pane in the focused pane's stack
    bind "<leader>p m" "core:begin-pane-placement"
    bind "<leader>p x" "core:close-pane-tree"
    bind "<leader>p <Left>" "core:focus-pane-left"
    bind "<leader>p <Down>" "core:focus-pane-down"
    bind "<leader>p <Up>" "core:focus-pane-up"
    bind "<leader>p <Right>" "core:focus-pane-right"
    bind "<leader>s <Left>" "core:resize-pane-left"
    bind "<leader>s <Down>" "core:resize-pane-down"
    bind "<leader>s <Up>" "core:resize-pane-up"
    bind "<leader>s <Right>" "core:resize-pane-right"
    bind "<leader>t n" "core:new-tab"           // <C-t> n with the default leader
    bind "<leader>t x" "core:close-tab"

    // explicit chords — they never move with the leader
    bind "<A-f>" "core:toggle-pane-fullscreen"
    bind "<Tab>" "core:next-tab"
    bind "<S-Tab>" "core:previous-tab"
}

mode "locked" {
    // locked mode passes every other key straight to the program.
    bind "<C-l>" "core:unlock"
    bind "<leader>q" "core:quit"
    bind "<leader>p m" "core:begin-pane-placement"
    bind "<leader>g" "core:mouse-select"
}

mode "pane-placement" {
    // Placement owns these keys while a pane placement is open.
    bind "<Left>" "core:select-pane-target-left"
    bind "<Down>" "core:select-pane-target-down"
    bind "<Up>" "core:select-pane-target-up"
    bind "<Right>" "core:select-pane-target-right"
    bind "<S-Left>" "core:select-pane-insertion-left"
    bind "<S-Down>" "core:select-pane-insertion-down"
    bind "<S-Up>" "core:select-pane-insertion-up"
    bind "<S-Right>" "core:select-pane-insertion-right"
    bind "<Space>" "core:cycle-pane-placement-span"
    bind "<Tab>" "core:select-next-placement-tab"
    bind "<S-Tab>" "core:select-previous-placement-tab"
    bind "<CR>" "core:confirm-pane-placement"
    bind "<Esc>" "core:cancel-pane-placement"
}
```

## Pane placement mode

`core:begin-pane-placement` opens the local `pane-placement` mode. It keeps the
current base mode: entering from locked mode keeps the client locked after
cancellation. While it is active, the `pane-placement` keymap has priority over
the normal or locked keymap. An unbound key and every key release are consumed
by placement.

The default `pane-placement` bindings are:

- Arrow keys choose a destination pane. A collapsed stack member is a
  destination at its header strip: in a stack `[A, B, C open]`, Up from `C`
  picks `B`, and Enter gives `[A, C open, B]`. A swap trades places, so a pane
  that lands in a stack slot joins that stack and a stack member that lands in
  a plain slot leaves it.
- Shift+Arrow keys choose a split edge. Toward a stacked pane, the edge belongs
  to the whole stack, and the placed pane lands beside the stack as a plain pane.
- Space cycles the available split spans.
- Tab and Shift+Tab preview the next or previous visible tab with the pane
  placed at the right edge of that whole tab. Enter places it there: from tab
  `#1` `[A | B]`, Tab and Enter on `A` give `#1` `[B]` and `#2` `[C | A]`. In
  that preview, Shift+Arrow picks another edge of the tab and an Arrow picks a
  pane in that tab to swap with. Returning to the pane's own tab selects no
  destination.
- A left-button drag previews the pane under the pointer and a valid release
  submits the placement. Pressing a pane's content, its grab handle, or a
  collapsed stack header picks that pane up. On the tab you are on, the pane
  also takes focus, so a collapsed member opens in its stack as the drag
  starts. On a tab you only preview, focus stays where it is and a collapsed
  member stays collapsed.
- Enter submits the selected placement. Pane placement mode stays active after
  the session accepts it, unless `koshi.kdl` sets
  `stay-in-pane-placement-mode-after-placement #false`.
- Escape runs `core:cancel-pane-placement`, discards the unconfirmed placement,
  and returns to the base normal or locked mode.

Every placement action is a normal bindable action in the `pane-placement`
block. For example, this changes the cancel key while keeping Escape available
for a different placement action:

```kdl
mode "pane-placement" {
    bind "<Esc>" "core:select-pane-target-right"
    bind "<C-c>" "core:cancel-pane-placement"
}
```

The effective `pane-placement` map must keep one live
`core:cancel-pane-placement` binding. Removing Escape without binding that
action to another key rejects the keymap.

`<leader>p m` opens the mode, then `<Right>` selects a destination instead of
running `core:focus-pane-right`. A normal-mode binding for `<leader>p
<Right>` still applies before placement opens.

### Pane placement mode and `koshi move-pane`

- Pane placement mode shows a preview only in the client that opened it.
  Other clients see nothing until Enter or a mouse drop confirms the
  placement. Then every client sees the new layout, and the other clients
  slide to it.
- `koshi move-pane --direction <direction> [--pane <pane-id>]` swaps a pane
  with its visible neighbor at once. It has no preview and no confirm step,
  and every client showing that tab slides to the new layout. Its action,
  `core:move-pane`, runs only from the command line: a key bound to it loads
  with an unresolvable-arguments warning and sends nothing when pressed.
