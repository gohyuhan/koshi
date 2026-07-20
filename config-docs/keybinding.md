# `keybinding.kdl` — key bindings

Your key bindings, folded on top of the built-in ones. A binding you set wins
over the default for the same key; keys you do not mention keep their defaults.

**Where it goes:** directly in the config directory —
`~/.config/koshi/keybinding.kdl` on Linux,
`~/Library/Application Support/koshi/keybinding.kdl` on macOS,
`%APPDATA%\koshi\config\keybinding.kdl` on Windows. See
[README](README.md#where-the-files-go).

**If anything is wrong: the whole file is dropped** back to the built-in keymap
(all-or-nothing), so a mistake never leaves you with a half-working keymap. Run
`koshi keys conflicts` to see what a rejected file tripped on, and
`koshi keys list` to see the keymap actually in effect.

## Top-level settings

All optional, each at most once, sitting beside the `mode` blocks.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `chord-timeout-ms` | integer — ms to wait for the next key in a multi-key shortcut before giving up | `500` | ≥ 0.1.0 |
| `which-key-delay-ms` | integer — ms before the hint bar shows the keys that continue a shortcut | `300` | ≥ 0.1.0 |
| `max-chord-depth` | integer — most keys a single shortcut may chain (must be ≥ 1) | `4` | ≥ 0.1.0 |
| `leader` | string — the key `<leader>` stands for in bindings; a modifier run like `"C-"` or a single chord like `"<Space>"` | `"C-"` (the Ctrl modifier run) | ≥ 0.1.0 |
| `unlock-alternative` | string — an extra chord that leaves locked mode | (none) | ≥ 0.1.0 |

## Key grammar

Keys use an angle grammar: a modifier + key in angle brackets, chained with
spaces for a multi-key shortcut.

- `<C-t>` — Ctrl+t. Modifiers: `C` Ctrl, `A` Alt, `S` Shift.
- `<Tab>`, `<CR>`, `<Esc>`, `<BS>` — named keys. A bare word like `Tab` is read
  as one chord *per character* (`T`, `a`, `b`), so always bracket named keys.
- `<leader>p` — the leader, then `p`. `<leader>` resolves against the `leader`
  setting: a **modifier run** like `"C-"` merges into the next key, so
  `<leader>p` is `<C-p>` — one press; a **chord** like `"<Space>"` becomes a
  prefix, so `<leader>p` is `<Space>` then `p` — two presses. Rebind `leader`
  and every `<leader>` binding moves with it — **including koshi's own
  defaults**, which are written with `<leader>` (see the example). Explicit
  chords like `<A-f>` are *not* the leader and never move.

## `mode` blocks

A `mode` block holds the bindings for one mode — `"normal"` (the usual mode) or
`"locked"` (keys pass straight to the program, except the unlock chord).

- `bind "<key>" "core:action"` — exactly two strings: the key sequence and the
  **full** action reference (namespaced, e.g. `core:new-tab`; a bare `new-tab`
  is rejected with a hint). A `bind` takes no arguments — a fixed choice lives
  in the action name (`core:new-pane-left`), an open value lives in the CLI.
- `remove "<key>"` — void a key in this mode, so lower layers no longer bind it.

Run `koshi actions list` to see every action name you can bind to.

## Full example

This is koshi's **complete default keymap**, written exactly as koshi ships it —
the leader-relative bindings use `<leader>`, so changing `leader` moves them all
at once. With the default `leader "C-"`, `<leader>p n` **is** `<C-p> n`; set
`leader "A-"` and it becomes `<A-p> n`. `bind` and `remove` both accept
`<leader>`. `<C-l>` (the reserved lock/unlock) and `<A-f>` are explicit — they
never move. Run `koshi keys list` to see the resolved keymap.

```kdl
// keybinding.kdl — the complete default keymap.
version 1

chord-timeout-ms 500
which-key-delay-ms 300
max-chord-depth 4
leader "C-"                      // the leader = the Ctrl modifier run (default)
// unlock-alternative "<A-u>"    // optional extra unlock chord; off by default

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
    bind "<leader>g" "core:mouse-select"
}
```
