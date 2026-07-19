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

The ribbon and hint bar fade between `ramp-start` and `ramp-end`: each element
of a run takes one interpolated stop along the gradient.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `ramp-start` | `#RRGGBB` — first end of the chrome gradient | `#581c87` | ≥ 0.1.0 |
| `ramp-end` | `#RRGGBB` — second end of the gradient | `#3b82f6` | ≥ 0.1.0 |
| `on-ramp` | `#RRGGBB` — text over a ramp-colored block | `#f4f1fa` | ≥ 0.1.0 |
| `on-ramp-dim` | `#RRGGBB` — text over a dimmed ramp block | `#c9c4d4` | ≥ 0.1.0 |
| `accent` | `#RRGGBB` — marks the chords already pressed in a pending shortcut | `#a78bfa` | ≥ 0.1.0 |
| `on-accent` | `#RRGGBB` — text over an accent block | `#1e1033` | ≥ 0.1.0 |
| `border-focused` | `#RRGGBB` — border of the focused pane | `#00afd7` | ≥ 0.1.0 |
| `border-unfocused` | `#RRGGBB` — border of the other panes | `#585858` | ≥ 0.1.0 |
| `border-hover` | `#RRGGBB` — border of the pane the pointer is over (the wheel target) | `#af5fff` | ≥ 0.1.0 |
| `stack-header-fg` | `#RRGGBB` — text of a collapsed stack member's header | `#f4f1fa` | ≥ 0.1.0 |
| `stack-header-bg` | `#RRGGBB` — background of that header | `#300f4a` | ≥ 0.1.0 |
| `letterbox` | `#RRGGBB` — the margin around a centered layout | `#585858` | ≥ 0.1.0 |

## Full example

This is **every** `theme.kdl` field, set to its **default** value — copy it as a
complete baseline and change the colors you like. Any color you delete just
restores its default.

```kdl
// theme.kdl — the complete default theme.
version 1
name "default"

colors {
    ramp-start "#581c87"
    ramp-end "#3b82f6"
    on-ramp "#f4f1fa"
    on-ramp-dim "#c9c4d4"
    accent "#a78bfa"
    on-accent "#1e1033"
    border-focused "#00afd7"
    border-unfocused "#585858"
    border-hover "#af5fff"
    stack-header-fg "#f4f1fa"
    stack-header-bg "#300f4a"
    letterbox "#585858"
}
```
