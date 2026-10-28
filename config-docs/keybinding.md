# `keybinding.kdl` — key bindings

User bindings are applied over built-in bindings. A user binding wins for the
same key. Unchanged keys keep built-in actions.

**Where it goes:** directly in the config directory —
`~/.config/koshi/keybinding.kdl` on Linux,
`~/Library/Application Support/koshi/keybinding.kdl` on macOS,
`%APPDATA%\koshi\config\keybinding.kdl` on Windows. See
[README](README.md#where-the-files-go).

**Any error drops the whole file.** `koshi keys conflicts` reports problems,
and exits 2 when koshi drops the file.
`koshi keys list` prints the active keymap.

## Top-level settings

`version` is required. Every other setting is optional and may appear at most
once beside the `mode` blocks.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `chord-timeout-ms` | integer — ms a shortcut waits for its next key when it is a complete binding and also the start of a longer one. When the time passes, the complete binding fires. A shortcut that only starts longer ones has no time limit (see [Key sequences](#key-sequences)) | `500` | ≥ 0.1.0 |
| `which-key-delay-ms` | integer — ms from a shortcut's first key until the hint bar shows the keys that continue it. A shortcut that ends sooner leaves the hint bar as it was. `0` shows them with the first key | `300` | ≥ 0.1.0 |
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

### Key sequences

A key that starts a longer shortcut opens a sequence. While the sequence is
open, no key reaches the program. A sequence that is not also a complete
binding waits for its next key with no time limit. A key that continues
nothing is discarded, and the sequence stays open. The sequence ends when a key
completes a binding whose action does not repeat (see
[Repeating keys](#repeating-keys)), when you press `<Esc>`, when the input
mode changes, or, in locked mode, when you press the unlock chord. For example,
in normal mode `<C-p>` then `z` discards `z` and leaves `<C-p>` open, and a
following `n` opens a new pane.

### Repeating keys

Some actions repeat from their prefix. If a shortcut of two or more keys fires
one of them, the shortcut without its last key opens again. The last key alone
then fires the action again. For example, `<C-p> <Left> <Left> <Down>` focuses
the pane on the left, then the next pane on the left, then the pane below.
`<C-p>` stays open after each key.

These actions repeat:

- `core:focus-pane-left`, `core:focus-pane-down`, `core:focus-pane-up`,
  `core:focus-pane-right`
- `core:resize-pane-left`, `core:resize-pane-down`, `core:resize-pane-up`,
  `core:resize-pane-right`
- `core:focus-next-floating-pane`, `core:focus-previous-floating-pane`
- `core:scroll-pane-up`, `core:scroll-pane-down`

`koshi keys describe` prints `continuous: true` for a shortcut that fires one
of them. Every other action, a plugin action included, does not repeat.

The prefix that opens again is an open sequence like any other:

- A key bound under it fires its action. If that action repeats, the prefix
  stays open. If it does not, the prefix ends. For example, `<C-p> <Left> n`
  focuses the pane on the left, opens a new pane, and ends `<C-p>`.
- A key bound to nothing under it is discarded, and the prefix stays open. For
  example, `<C-p> <Left> a` focuses the pane on the left and discards `a`.
- `<Esc>` ends it, and the program receives no key.
- `chord-timeout-ms` does not end it. It waits for its next key with no time
  limit.
- The hint bar keeps the timing of the first key. With the default
  `which-key-delay-ms 300`, the hint bar shows the keys that continue `<C-p>`
  300 ms after `<C-p>`. A repeated `<Left>` does not restart that wait.

A one-key shortcut opens no prefix. With `bind "<A-h>" "core:focus-pane-left"`,
each `<A-h>` focuses the pane on the left once, and the next key goes where it
went before.

## `mode` blocks

A `mode` block holds the bindings for one built-in keymap — `"normal"`,
`"locked"`, `"pane-placement"`, `"pane-move-choice"`, `"floating-pane-move"`,
`"dock-undock"`, `"floating-pane-tray"`, or `"notification-center"`. Normal
and locked are base modes. The other six are local modes. Each one takes
priority over the base mode while it is open.

A block for any other mode name, such as `mode "resize"`, binds nothing. The
rest of the file still applies, and `koshi keys conflicts` warns that the mode
is unregistered.

- `bind "<key>" "core:action"` — exactly two strings: the key sequence and the
  **full** action reference (namespaced, e.g. `core:new-tab`; a bare `new-tab`
  is rejected with a hint). A `bind` takes no arguments — a fixed choice lives
  in the action name (`core:new-pane-left`), an open value lives in the CLI.
- `remove "<key>"` — void a key in this mode, so lower layers no longer bind it.

Run `koshi actions list` to see every action name you can bind to.

## Full example

Complete built-in keymap. `<leader>` bindings follow the configured leader.
`<C-l>`, `<A-f>`, `<A-n>`, Tab, and Shift+Tab stay fixed.

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
    bind "<leader>p f" "core:new-pane-floating"
    bind "<leader>p m" "core:begin-pane-placement"
    bind "<leader>p u" "core:begin-dock-undock"
    bind "<leader>p <Tab>" "core:focus-next-floating-pane"       // repeats: the prefix stays open
    bind "<leader>p <S-Tab>" "core:focus-previous-floating-pane"
    bind "<leader>p w" "core:toggle-floating-pane-tray"
    bind "<leader>p i" "core:toggle-pane-pinned"
    bind "<leader>p x" "core:close-pane-tree"
    bind "<leader>p <Left>" "core:focus-pane-left"              // repeats: the prefix stays open
    bind "<leader>p <Down>" "core:focus-pane-down"
    bind "<leader>p <Up>" "core:focus-pane-up"
    bind "<leader>p <Right>" "core:focus-pane-right"
    bind "<leader>s <Left>" "core:resize-pane-left"             // repeats: the prefix stays open
    bind "<leader>s <Down>" "core:resize-pane-down"
    bind "<leader>s <Up>" "core:resize-pane-up"
    bind "<leader>s <Right>" "core:resize-pane-right"
    bind "<leader>t n" "core:new-tab"           // <C-t> n with the default leader
    bind "<leader>t x" "core:close-tab"

    // explicit chords — they never move with the leader
    bind "<A-f>" "core:toggle-pane-fullscreen"
    bind "<A-n>" "core:toggle-notification-center"
    bind "<Tab>" "core:next-tab"
    bind "<S-Tab>" "core:previous-tab"
}

mode "locked" {
    // locked mode passes every other key straight to the program.
    bind "<C-l>" "core:unlock"
    bind "<leader>q" "core:quit"
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
    bind "s" "core:toggle-pane-placement-stack"
    bind "<Tab>" "core:select-next-placement-tab"
    bind "<S-Tab>" "core:select-previous-placement-tab"
    bind "<CR>" "core:confirm-pane-placement"
    bind "<Esc>" "core:cancel-pane-placement"
}

mode "pane-move-choice" {
    // The chooser owns these keys while it is open.
    bind "t" "core:begin-tiled-pane-placement"
    bind "f" "core:begin-floating-pane-move"
    bind "<CR>" "core:confirm-pane-move-choice"
    bind "<Esc>" "core:cancel-pane-move-choice"
}

mode "floating-pane-move" {
    // Floating pane move mode owns these keys while it is open.
    bind "<Left>" "core:move-floating-pane-left"
    bind "<Down>" "core:move-floating-pane-down"
    bind "<Up>" "core:move-floating-pane-up"
    bind "<Right>" "core:move-floating-pane-right"
    bind "<Tab>" "core:select-next-floating-pane"
    bind "<S-Tab>" "core:select-previous-floating-pane"
    bind "<CR>" "core:confirm-floating-pane-move"
    bind "<Esc>" "core:cancel-floating-pane-move"
}

mode "dock-undock" {
    // Dock/undock mode owns these keys while it is open.
    bind "<Left>" "core:select-pane-target-left"
    bind "<Down>" "core:select-pane-target-down"
    bind "<Up>" "core:select-pane-target-up"
    bind "<Right>" "core:select-pane-target-right"
    bind "<S-Left>" "core:select-pane-insertion-left"
    bind "<S-Down>" "core:select-pane-insertion-down"
    bind "<S-Up>" "core:select-pane-insertion-up"
    bind "<S-Right>" "core:select-pane-insertion-right"
    bind "<Space>" "core:cycle-pane-placement-span"
    bind "s" "core:toggle-pane-placement-stack"
    bind "<Tab>" "core:select-next-placement-tab"
    bind "<S-Tab>" "core:select-previous-placement-tab"
    bind "<CR>" "core:confirm-dock-undock"
    bind "<Esc>" "core:cancel-dock-undock"
}

mode "floating-pane-tray" {
    // The floating pane tray owns these keys while it is open.
    bind "<Left>" "core:select-tray-tile-left"
    bind "<Down>" "core:select-tray-tile-down"
    bind "<Up>" "core:select-tray-tile-up"
    bind "<Right>" "core:select-tray-tile-right"
    bind "<CR>" "core:toggle-selected-tray-tile"
    bind "a" "core:toggle-minimize-all-floating-panes"
    bind "<Esc>" "core:close-floating-pane-tray"
    bind "<leader>p w" "core:toggle-floating-pane-tray"
}

mode "notification-center" {
    // The notification center owns these keys while it is open.
    bind "<Up>" "core:select-newer-notification"
    bind "<Down>" "core:select-older-notification"
    bind "x" "core:dismiss-selected-notification"
    bind "c" "core:clear-notifications"
    bind "<Esc>" "core:close-notification-center"
    bind "<A-n>" "core:toggle-notification-center"
}
```

## Pane placement mode

`core:begin-pane-placement` opens the local `pane-placement` mode when this
client draws no floating pane that it did not pin. Otherwise it opens the pane
move chooser (see [below](#the-pane-move-chooser)). Pane placement mode moves
tiled and stacked panes only: its pane is the focused pane of the active tab,
also while a floating pane is focused. While it is open, this client draws no
floating pane, and a click never reaches one. The mode keeps the
current base mode: a client that enters it from locked mode is still locked
when the mode ends. The default `locked` keymap binds no key that opens it. A
locked client opens it by dragging a pane's grab handle, or with a key bound in
the `locked` block, such as `bind "<leader>p m" "core:begin-pane-placement"`.
While it is active, the `pane-placement` keymap has priority over
the normal or locked keymap. An unbound key and every key release are consumed
by placement.

The default `pane-placement` bindings are:

- Arrow keys choose a destination pane. A collapsed stack member is a
  destination at its header strip: in a stack `[A, B, C open]`, Up from `C`
  picks `B`, and Enter gives `[A, C open, B]` on this client's screen. A swap
  trades places, so a pane that lands in a stack slot joins that stack and a
  stack member that lands in a plain slot leaves it.
- Shift+Arrow keys choose a split edge. Toward a stacked pane, the edge belongs
  to the whole stack, and the placed pane lands beside the stack as a plain pane.
- Space cycles the available split spans.
- `s` (`core:toggle-pane-placement-stack`) switches the target between a
  swap with a pane, or a split edge of a pane, and that pane's stack. In
  `[A | B]` with `A` picked, Right then `s` targets the stack of `B`, and
  Enter gives the stack `[B, A]` with `A` open on this client's screen.
  Pressed on the stack target, `s` goes back to the swap. On the split edge
  of a whole stack, `s` targets that stack, and pressed again it picks the
  swap with the member that stack shows open. On the edge of a whole tab,
  `s` targets the pane or the stack that fills the tab: in a tab that holds
  only the stack `[S1, S2]` with `S2` open, `s` targets that stack, and
  Enter gives the stack `[S1, S2, A]`. A tab that a split fills has no stack
  target, and `s` changes nothing there.
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
  member stays collapsed. A release over a target with no room sends nothing,
  and the target stays selected. A drag that began on a grab handle outside
  pane placement mode ends there, and nothing moves. A valid release of that
  drag ends pane placement mode: the tabline shows the mode the client was in
  before the drag, such as `BASE` or `LOCK`, whether the session accepts or
  rejects the placement, and whatever
  `stay-in-pane-placement-mode-after-placement` says.
- Enter submits the selected placement. Pane placement mode stays active after
  the session accepts it, unless `koshi.kdl` sets
  `stay-in-pane-placement-mode-after-placement #false`.
- If the selected target has no room, the preview draws the tab as it is.
  The moving pane reads `Moving pane cannot go there`, and the target's panes
  read `No room for this pane in <name>`, or `No room for the panes left in
  <name>` when the pane's own tab cannot hold the panes it leaves behind. A
  tab that no client would show with a pane area reads `No room to show
  <name>`, and any other refusal reads `This place is not available`. Enter
  then changes nothing, and the target stays selected until you pick another
  target or press Escape.
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
action to another key rejects the keymap. The same rule holds for
`core:cancel-pane-move-choice` in `pane-move-choice`,
`core:cancel-floating-pane-move` in `floating-pane-move`,
`core:cancel-dock-undock` in `dock-undock`,
`core:close-floating-pane-tray` in `floating-pane-tray`, and
`core:close-notification-center` in `notification-center`.

`<leader>p m` opens the mode, then `<Right>` selects a destination instead of
running `core:focus-pane-right`. A normal-mode binding for `<leader>p
<Right>` still applies before placement opens.

## Dock/undock mode

`<leader>p u` (`core:begin-dock-undock`) opens the local `dock-undock` mode.
The tabline shows the tag `DOCK`, and the hint bar lists the mode's keys. The
mode selects the pane that holds this client's input. While the mode is open,
the `dock-undock` keymap has priority over the normal or locked keymap, and
the mode consumes every key it does not bind.

Bound in the `dock-undock` keymap, `core:begin-pane-placement` and
`core:begin-tiled-pane-placement` do nothing. `core:begin-floating-pane-move`
ends the mode and opens floating pane move mode. While the mode waits for the
session to answer its undock or dock, it does nothing.

With a tiled or stacked pane selected, Enter (`core:confirm-dock-undock`)
floats the pane up where this client draws it:

- The pane keeps its id, its program and its scrollback. It keeps its top-left
  cell and its size. Each axis is cut to 90% of the smallest pane area of the
  attached clients, rounded down. A client with a pane area smaller than `5x6`
  does not count. A collapsed stack member keeps the top-left cell and the
  size it opens into in its stack: in an `80x22` tab that holds only the
  stack `[vim, htop]` with `vim` open, `htop` floats at `(0, 1)` with the size
  `72x19`.
- The other panes of its tab fill the space it leaves. If the pane was the
  only pane of its tab, the tab closes, and every client on that tab goes to
  the nearest tab.
- The pane takes this client's input. Every other client whose input is on
  the pane also shows it, at the top-left cell where that client draws the
  pane, and keeps typing in it. Every other client holds it minimized. A
  client cannot draw floating panes when its pane area is smaller than `5x6`,
  or when its tab shows `Terminal too small — enlarge window` in place of
  every pane. That client also holds the pane minimized, and its input moves
  to the pane that inherits its focus in the tab. With motion on, the pane
  slides to its floating place in 160 ms.
- The floating pane stays selected, with no dock target, and the mode stays
  open.

Example: tab `#1` holds `A | B` in an `80x22` pane area, with `B` focused.
`<C-p> u` then Enter leaves `A` alone in tab `#1`, and `B` floats at `(40, 0)`
with the size `40x19`.

With a floating pane selected, the mode docks it into a tab:

- `<C-p> u` on a focused floating pane selects the right edge of the whole
  active tab as the dock target. Enter at once docks the pane there: tab `[A]`
  and floating `F` give `[A | F]`.
- While a dock target is selected, the frame shows the pane docked there, and
  this client draws no other floating pane.
- Arrow and Shift+Arrow keys pick the pane to dock beside, and the edge, as
  Shift+Arrow does in pane placement mode. Toward a stacked pane, the edge
  belongs to the whole stack. On the whole-tab target, an Arrow picks a pane
  and Shift+Arrow picks another edge of the tab: in `[A | B]`, Right picks
  the right edge of `B`. Space cycles the spans. `s` switches between the
  edge of a pane and that pane's stack: in `[A | B]`, the target beside `B`
  becomes the stack of `B`, and Enter gives `A` beside the stack `[B, F]`
  with `F` open on this client's screen. On the edge of a whole stack, `s`
  targets that stack: in `[stack(S1, S2) | P]`, Left from `P` picks the edge
  of the stack, `s` targets the stack, and Enter gives the stack
  `[S1, S2, F]`. On the whole-tab target, `s` targets the pane or the stack
  that fills the tab: tab `[A]` gives the stack `[A, F]`, and a tab that a
  split fills has no stack target. Tab and Shift+Tab preview
  the next or previous tab with the pane at the right edge of that whole tab.
- Enter docks the pane at the target, and the docked pane takes this
  client's input: every other floating pane this client shows is minimized.
  A dock into another tab moves this client to that tab. Every other client
  whose input is on the pane moves with it in the same way. Every other
  client keeps the stack member it shows open: a client that shows `A` open
  in the stack `[A, F]` still shows `A` open. After the dock, nothing is
  selected and the mode stays open.
- If the destination tab has no room for the pane, the frame draws the tab as
  it is. The pane reads `Moving pane cannot go there`, and the target's panes
  read `No room for this pane in <name>`. Enter then changes nothing, and the
  target stays selected until you pick another target or press Escape. If
  the tab changes after the frame and before the session answers, the session
  refuses the dock with `tab <name> has no room for another pane`, and
  nothing changes.

With the mouse:

- A press on a tiled pane, then a drag, floats the pane up where it is drawn
  and selects it. The same drag goes on with the floating pane. A press on a
  collapsed stack member's header, then a drag, floats that member up at the
  rect it opens into in its stack.
- A drag of the selected floating pane moves it with the pointer. The pane
  under the pointer shows its edge nearest the pointer as the dock target,
  and this client draws no other floating pane during the drag. A release
  over a target docks the pane there. A release over no target, or over a
  target with no room, leaves the pane floating where it was dropped. A
  pinned floating pane stays at its
  pinned cell during the drag: a release over a target docks it, and a
  release over no target, or over a target with no room, leaves it where it
  was.
- A press on another floating pane selects it, and a drag moves it as above.

During a drag, Escape ends only the drag, and the pane goes back to where it
was. Otherwise, Escape (`core:cancel-dock-undock`) leaves the mode and changes
nothing. While an undock or a dock waits for the session's answer, Enter,
Escape and the mouse change nothing. A lock mode change, a mouse-select
change, and the loss of the terminal's focus also end the mode, unless an
undock or a dock waits.

The session refuses an undock, and changes nothing, when the pane is the only
pane of the only tab, or when the session already holds 12 floating panes.

## Floating pane tray

The tabline shows the tray button `⧉` left of the mode tag while the session
holds at least one floating pane. `<leader>p w`
(`core:toggle-floating-pane-tray`) or a click on the button opens the tray under
the button. The tray lists every floating pane of the session as a tile, in the
order the panes became floating:

- A tile shows its number, `1` to `12`, on its top border, and the pane's
  title under it. A tile is 10 cells wide. A title of 8 cells or more shows
  the characters that fit in its first 6 cells, then `…`: `logserver` reads
  `logser…`. A pane with no title shows its number alone.
- A lit tile is a pane that this client shows. A gray tile is a pane that
  this client minimized.
- The tray holds 12 slots in 2 rows of 6. A narrow terminal puts fewer slots
  in each row.
- The tray's titlebar, the row under its top border, shows the full title of
  the tile under the pointer, else of the selected tile. A title too long for
  its cells ends in `…`. Right of the title, the titlebar reads `minimize all`
  while this client shows a floating pane, and `restore all` while it shows
  none, then the close button `✕`. A tray of one slot per row has no room for
  this label and leaves it out.

While the tray is open, the tabline shows the tag `TRAY`. The
`floating-pane-tray` keymap has priority over the normal or locked keymap, and
the tray consumes every key it does not bind. The tray changes this client's
screen only. Another client sees no change.

- The arrow keys (`core:select-tray-tile-left`, `-down`, `-up`, `-right`)
  select the next tile in that direction.
- Enter (`core:toggle-selected-tray-tile`) minimizes the pane of a lit tile.
  On a gray tile, it shows the pane on top and focuses it.
- `a` (`core:toggle-minimize-all-floating-panes`) minimizes every floating
  pane this client shows. When this client shows none, it shows every one, in
  the order the panes became floating, and focuses the last one. A click on
  `minimize all` or `restore all` does the same.
- A click on a tile does what Enter does on that tile.
- Escape (`core:close-floating-pane-tray`), `<leader>p w`, a click on the
  button and a click on the close button `✕` close the tray.
- A click outside the tray closes the tray. The same click then acts as it
  does with no tray open.

The tray stays open after a tile changes, and it shows each change at once.
It closes when the session holds no floating pane or the terminal has no room
for it. A lock mode change, a mouse-select change, the loss of the terminal's
focus, a lost link to the session and a session switch also close it. The tray
and the notification center are never open together: while the center is
open, the tray's actions do nothing.

A minimize shrinks the pane into its tile in 160 ms. With the tray closed, it
shrinks into the cell under the glyph `⧉`, on the top row of the pane area. A
restore grows the pane out of the same tile or cell. With
`reduced-motion #true`, the pane changes in one frame.

Example: the session holds `htop`, `logs` and `notes`, and this client shows
`htop` and `logs`. Tiles 1 and 2 are lit, and tile 3 is gray. Enter on tile 1
minimizes `htop`, and tile 1 turns gray. `a` then minimizes `logs` too, and
the label changes to `restore all`.

## Notification center

Koshi states each message of its own as a notification: a pane that a signal
killed, a config warning, or a key whose command the session refused. The
notification center lists the notifications of this client, and a toast shows
each new one for a short time. If this client reads its session slower than
the session writes, the session keeps each refusal of this client's commands
until the client catches up. Then equal refusals in a row show as one
notification with their count: `[Koshi] Warning ×3`.

The tabline shows the notification button `●` while this client holds at
least one notification. The button sits left of the tray button, or left of
the mode tag when the tabline shows no tray button. With unread notifications,
the button shows their count: `●3`, and `●99+` above 99. A notification is
read once the center opens after it arrives.

`<A-n>` (`core:toggle-notification-center`) or a click on the button opens the
center. The center covers the right side of the pane area and changes no pane.
It is as tall as the pane area and 1/10 of the pane area's width, from 24 to
48 columns wide. A pane area narrower than 24 columns gives it the full width.
The center slides in from the right edge in 160 ms, and slides out the same
way when it closes. With `reduced-motion #true`, it opens and closes in one
frame.

The center's top row holds the title `Notifications` and the close button
`✕`, and its last row holds `Clear all`. Between them, the center lists one
card per notification, newest first. A card shows a header above the text:
`[Koshi] Error` above `pane …a1b2 killed by signal 9`. A notification that
repeats shows the count in its header: `[Koshi] Warning ×3`. Each card holds
a dismiss button `✕` on its top border. When this client held more
notifications than it keeps, the text under the last card reads
`3 older notifications were dropped`.

The center opens with the newest card selected, and the selected card draws
its border in the hover color. While the center is open, the tabline shows
the tag `NOTIFICATIONS`, and the button shows in reverse video. The
`notification-center` keymap has priority over the normal or locked keymap,
and the center consumes every key it does not bind. No key reaches a pane.

- Up (`core:select-newer-notification`) and Down
  (`core:select-older-notification`) select the next card in that direction.
  The center then scrolls the fewest rows that show the whole card.
- `x` (`core:dismiss-selected-notification`) and a click on a card's `✕`
  remove that notification. If the removed card was selected, the next older
  card is selected, or the next newer card when no older card is left.
- A click on a card selects it. The mouse wheel over the center scrolls it by
  `scroll-lines` rows.
- Escape (`core:close-notification-center`), `<A-n>`, a click on the button,
  and a click on the close button `✕` close the center. Every notification
  stays.
- `c` (`core:clear-notifications`) and a click on `Clear all` remove every
  notification and close the center. A notification that arrives after the
  clear stays in the center.
- A click outside the center closes it. The same click then acts as it does
  with no center open.

Any other action, a lock mode change and a lost link to the session also
close the center. While the center slides out, it takes no key, and a click
on it does nothing. The center
and the floating pane tray are never open together. While the tray is open,
the center's actions do nothing, and a click on the notification button
closes the tray and opens the center. While another local mode is on, such as
dock/undock mode, pane placement, the move chooser, or floating pane move
mode, the center does not open. It also does not open while the link to the
session is lost. A pane area narrower than 10 columns or
shorter than 6 rows has no room for the center. Then the center does not
open, and a resize to such a size closes it at once. It stays closed when the
pane area grows again. The center and its notifications belong to this client
only. Another client of the same session keeps its own list, and a clear on
one client removes nothing from another.

A toast slides in from the side of its corner, stays, and slides out again:
1 s in, 3 s in place, and 1 s out. When more notifications wait, each part
takes 0.5 s. At most 3 notifications wait for a toast. Each further one joins
one last toast that counts them: `36 more notifications in the center`. Once
that toast shows, it counts no new notification: a notification that arrives
then waits for a toast of its own after it. A click on the toast's ` ✕ `
slides it out at once, and its notification stays in the center. Opening the
center removes every toast. A notification that arrives while the center is
open shows no toast.

The toast is as wide as the center, and shows at most 2 rows of text under
its header. A pane area too short for every row keeps the rows that fit, and
the last row then ends in `…`. The toast's time stops while the pane area has
no room for a box 10 columns wide that holds the toast's border, its header
and the first row of its text: 4 rows tall. It also stops after a terminal
resize, until a frame of the new size shows. The `notifications`
block of `koshi.kdl` sets the corner and the time in place, and turns toasts
off.

Example: a build in a command pane fails with exit code 2 while the center is
closed. The toast `[Koshi] Error` `pane …c3d4 exited 2` slides in at the top
right, and the button changes from `●` to `●1`. `<A-n>` opens the center with
the notification on top, and the button changes back to `●`.

## Floating pane actions

`<leader>p f` (`core:new-pane-floating`) opens a shell in a new floating pane.
This client shows the pane above its other floating panes and gives it its
input. Every other client holds the pane minimized, and so does a client that
cannot draw floating panes (`cli.md`, *Floating panes*). The pane takes the
size and the place that `koshi new-pane --floating` gives without `--cols`,
`--rows`, `--x` and `--y`. The shell starts in the working directory of the
pane that has this client's input. With 12 floating panes in the session, the
key is refused with `a session holds at most 12 floating panes`.

`<leader>p i` (`core:toggle-pane-pinned`) pins the focused floating pane for
this client at the cell where it is drawn, as the titlebar's `[ ]` does. When
this client pinned the pane, the key unpins it, as `[⚪]` does. With no
floating pane focused, the key is refused with `no floating pane is focused`.

No default key runs these actions. Bind them in `normal` or `locked`:

- `core:undock-pane` floats the pane that holds this client's input up where
  this client draws it, as Enter does in dock/undock mode. A floating pane is
  refused with `<pane-id> is already floating`.
- `core:pin-pane` and `core:unpin-pane` pin and unpin the focused floating
  pane for this client.
- `core:minimize-pane` minimizes the focused floating pane for this client.
- `core:minimize-all-floating-panes` minimizes every floating pane this client
  shows. `core:restore-all-floating-panes` shows each floating pane this
  client minimized, in the order the panes became floating, and focuses the
  last one it shows.

`core:toggle-minimize-all-floating-panes` has a default key only in the tray,
`a`. Bound in `normal` or `locked`, it runs with the tray closed: it minimizes
every floating pane this client shows, or shows every one when this client
shows none. With no floating pane in the session, it does nothing.

With no floating pane focused, `core:pin-pane`, `core:unpin-pane` and
`core:minimize-pane` are refused with `no floating pane is focused`.

`core:dock-pane`, `core:move-floating-pane` and `core:restore-pane` need a
pane, a tab or a cell. They run only from the command line: `koshi
dock-pane`, `koshi move-pane --x --y`, and `koshi restore-pane --pane`. A key
bound to one of them loads with an unresolvable-arguments warning and sends
nothing when pressed.

`<leader>p <Tab>` (`core:focus-next-floating-pane`) and `<leader>p <S-Tab>`
(`core:focus-previous-floating-pane`) step through every floating pane of the
session, in the order the panes became floating, and wrap at the ends:

- Each step shows the pane it lands on above this client's other floating
  panes and gives it this client's input. A minimized pane is shown again.
- The panes a step passes stay shown.
- From a tiled or stacked pane, `<Tab>` lands on the first floating pane and
  `<S-Tab>` on the last.
- Both actions repeat (see [Repeating keys](#repeating-keys)). The prefix
  stays open, so each further `<Tab>` or `<S-Tab>` takes one more step.

Example: `htop` and then `logs` became floating, this client minimized both,
and its input is on the tiled pane `A`. `<C-p> <Tab>` shows and focuses
`htop`. `<Tab>` again shows `logs` on top, and `htop` stays shown.

With no floating pane in the session, both are refused with `the session holds
no floating pane`. A client that cannot draw floating panes refuses both with
`no floating pane can be drawn on this client`: its pane area is smaller than
`5x6`, or its tab shows `Terminal too small — enlarge window` in place of every
pane.

## The pane move chooser

`<leader>p m` opens the chooser when this client draws at least one floating
pane that it did not pin. The tabline shows the tag `MOVE`, and the hint bar
lists the chooser keys. The chooser itself sends no command. The mode that a
key opens sends what that mode sends: floating pane move mode focuses the
pane it selects.

- `t` (`core:begin-tiled-pane-placement`) opens pane placement mode.
- `f` (`core:begin-floating-pane-move`) opens floating pane move mode.
- Enter (`core:confirm-pane-move-choice`) opens floating pane move mode when a
  floating pane is focused, and pane placement mode otherwise. The hint bar
  names the mode Enter opens: `Move Floating Pane` or `Move Tiled Pane`.
- Escape (`core:cancel-pane-move-choice`) returns to the base mode.

The chooser discards every other key. A mouse click acts as it does in the
base mode. `core:begin-tiled-pane-placement` and
`core:begin-floating-pane-move` also work bound in `normal` or `locked`, with
no chooser.

## Floating pane move mode

Floating pane move mode moves one floating pane inside this client's view.
The tabline shows the tag `MOVE FLOAT`. The mode selects the focused floating
pane, or the front-most floating pane when a tiled pane is focused, and
focuses the selected pane. It never selects a pinned or minimized floating
pane.

- Arrow keys move the selected pane one cell. The pane stops at the edge of
  the pane area. Only this client draws the moved pane until you confirm.
- Tab and Shift+Tab select the next or previous floating pane in the order
  the panes became floating, and focus it. With floating panes `a`, `b` and
  `c` that became floating in that order and `c` selected, Shift+Tab selects
  `b`, then `a`, then `c`. A move you did not confirm is discarded.
- A left-button press on a floating pane's content or grab handle selects
  that pane and focuses it. A drag moves it, and the release sends the move.
  A press on any other place does nothing.
- Enter sends the move. Pane placement mode's setting
  `stay-in-pane-placement-mode-after-placement` also covers this mode: `#true`
  (the default) keeps the mode open for the next move, and `#false` ends it
  once the session runs the move. Until the session answers, the keys and the
  mouse of the mode do nothing, and Escape does not close it. A move the
  session rejects keeps the mode open, and the pane goes back to where the
  session places it. A mouse release that sends a move does the same.
- Escape discards the move and returns to the base mode. During a drag, the
  first Escape ends only the drag.

Each confirmed move sends one `MoveFloatingPane` command. It changes only
this client's view: other clients keep the pane where they put it. If the
selected pane is pinned, minimized or closed while the mode is open, the
unconfirmed move is discarded and nothing is sent.

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
