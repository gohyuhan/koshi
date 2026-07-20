# `theme.kdl` — colors

The colors koshi draws its own chrome with: pane borders, the tab ribbon, the
key-hint bar, stack headers. It does **not** recolor what runs inside a pane —
that is the program's own output, drawn with the colors it asks for.

**Where it goes:** directly in the config directory — `~/.config/koshi/theme.kdl`
on Linux, `~/Library/Application Support/koshi/theme.kdl` on macOS,
`%APPDATA%\koshi\config\theme.kdl` on Windows. See [README](README.md#where-the-files-go).

**If a color is wrong:** it is skipped (keeps its default) and koshi logs it;
every other color still applies.

The file is the theme itself — no wrapping block. It has an optional `name` and
a `colors` block. Every color is a `#RRGGBB` hex string (the leading `#` is
optional).

| Key | Value / type | Default | Since |
|---|---|---|---|
| `name` | string — a label for the theme | `"default"` | ≥ 0.1.0 |

## `colors`

The tab ribbon and the hint bar fade between `ramp-start` and `ramp-end`: each
element of a run takes one interpolated stop along the gradient. Both of
koshi's own rows — the tab bar on top and the key-hint bar on the bottom — are
filled with `bar-bg` before anything is drawn on them, so their text sits on a
color the theme picks rather than on whatever your terminal's background is.

The stock colors are built for that black bar: the ramp is light, so it reads
as **text** (the session name, the active tab, a `Ctrl +` header), and
`on-ramp` is near-black, because the same ramp color is the **background** of
every key block.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `ramp-start` | `#RRGGBB` — first end of the chrome gradient | `#d0a5ff` | ≥ 0.1.0 |
| `ramp-end` | `#RRGGBB` — second end of the gradient | `#7dbcff` | ≥ 0.1.0 |
| `on-ramp` | `#RRGGBB` — text over a ramp-colored block | `#12091f` | ≥ 0.1.0 |
| `on-ramp-dim` | `#RRGGBB` — text over a dimmed ramp block | `#f0ecfa` | ≥ 0.1.0 |
| `accent` | `#RRGGBB` — marks the chords already pressed in a pending shortcut | `#f5c2ff` | ≥ 0.1.0 |
| `on-accent` | `#RRGGBB` — text over an accent block | `#1e1033` | ≥ 0.1.0 |
| `bar-bg` | `#RRGGBB` — background of the tab bar and the key-hint bar | `#000000` | ≥ 0.1.0 |
| `border-focused` | `#RRGGBB` — border of the focused pane | `#00afd7` | ≥ 0.1.0 |
| `border-unfocused` | `#RRGGBB` — border of the other panes | `#585858` | ≥ 0.1.0 |
| `border-hover` | `#RRGGBB` — border of the pane the pointer is over (the wheel target) | `#af5fff` | ≥ 0.1.0 |
| `stack-header-fg` | `#RRGGBB` — text of a collapsed stack member's header | `#f4f1fa` | ≥ 0.1.0 |
| `stack-header-bg` | `#RRGGBB` — background of that header | `#300f4a` | ≥ 0.1.0 |
| `letterbox` | `#RRGGBB` — the margin around a centered layout | `#585858` | ≥ 0.1.0 |

**Picking your own ramp:** a ramp color is used both as text on `bar-bg` and as
the background of a key block, so it has to contrast with `bar-bg` *and* with
`on-ramp`. Going dark on both ends (say `ramp-start "#581c87"`) makes the
session name and the `Ctrl +` headers nearly unreadable on a black bar; if you
want a dark ramp, lighten `bar-bg` to match, and flip `on-ramp` back to a light
color.

## Full example

This is **every** `theme.kdl` field, set to its **default** value — copy it as a
complete baseline and change the colors you like. Any color you delete just
restores its default.

```kdl
// theme.kdl — the complete default theme.
version 1
name "default"

colors {
    ramp-start "#d0a5ff"
    ramp-end "#7dbcff"
    on-ramp "#12091f"
    on-ramp-dim "#f0ecfa"
    accent "#f5c2ff"
    on-accent "#1e1033"
    bar-bg "#000000"
    border-focused "#00afd7"
    border-unfocused "#585858"
    border-hover "#af5fff"
    stack-header-fg "#f4f1fa"
    stack-header-bg "#300f4a"
    letterbox "#585858"
}
```
