# `themes/<name>.kdl` — colors

Colors for pane borders, tab bar, key hints, and stack headers. Pane programs
keep their own colors.

**Where it goes:** in a `themes/` subdirectory of the config directory —
`~/.config/koshi/themes/midnight.kdl` on Linux,
`~/Library/Application Support/koshi/themes/midnight.kdl` on macOS,
`%APPDATA%\koshi\config\themes\midnight.kdl` on Windows. See
[README](README.md#where-the-files-go).

Select a theme in [`koshi.kdl`](koshi.md):

```kdl
// koshi.kdl
version 1
theme "midnight"        // reads themes/midnight.kdl
```

[`themes-example/`](../themes-example/) contains 25 themes ready to copy. Its
[README](../themes-example/README.md) lists them.

The theme's name **is** its file name — `themes/midnight.kdl` is the theme
`midnight`. The file itself carries no name.

Built-in `default` colors apply when:

- `theme` is omitted;
- `theme "default"` is used; or
- the selected file is missing or invalid.

Koshi logs theme load errors. A bad color keeps its default; other valid colors
still apply.

The file is the theme itself — no wrapping block, just a required `version`
and an optional `colors` block. Every color is a `#RRGGBB` hex string (the
leading `#` is optional).

## `colors`

The tab and key-hint bars fade from `ramp-start` to `ramp-end`. `bar-bg` fills
both rows. `on-ramp` and `on-accent` color text drawn over filled blocks.

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

Keep the ramp readable against `bar-bg`, and keep `on-ramp` readable against
the ramp.

## Full example

Every theme field at its default value:

```kdl
// themes/midnight.kdl — every color, at its default value.
version 1

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
