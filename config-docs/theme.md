# `themes/<name>.kdl` — colors

The colors koshi draws its own chrome with: pane borders, the tab ribbon, the
key-hint bar, stack headers. It does **not** recolor what runs inside a pane —
that is the program's own output, drawn with the colors it asks for.

**Where it goes:** in a `themes/` subdirectory of the config directory —
`~/.config/koshi/themes/midnight.kdl` on Linux,
`~/Library/Application Support/koshi/themes/midnight.kdl` on macOS,
`%APPDATA%\koshi\config\themes\midnight.kdl` on Windows. See
[README](README.md#where-the-files-go).

**How a theme is picked:** keep as many themes side by side as you like, one
file each, and name the one you want in [`koshi.kdl`](koshi.md):

```kdl
// koshi.kdl
theme "midnight"        // reads themes/midnight.kdl
```

**Don't want to build one?** [`themes-example/`](../themes-example/) ships 25
ready-made themes — Dracula, Gruvbox, Nord, Catppuccin, Tokyo Night, Rosé Pine,
Solarized, Everforest, Kanagawa and more, in both dark and light. Copy one into
your `themes/` folder and name it. Its [README](../themes-example/README.md)
lists them all.

The theme's name **is** its file name — `themes/midnight.kdl` is the theme
`midnight`. The file itself carries no name.

**Falling back to the built-in theme.** koshi has one theme compiled in, called
`default`. You get it when:

- `koshi.kdl` has no `theme` line, or
- it says `theme "default"` — the name is reserved for the built-in colors, so
  a `themes/default.kdl` of your own is never read, or
- it names a theme whose file is missing or cannot be parsed. koshi logs which
  theme it could not load and carries on with the built-in colors, so a typo in
  the name never stops koshi from starting.

**If a color is wrong:** it is skipped (keeps its default) and koshi logs it;
every other color still applies.

The file is the theme itself — no wrapping block, just a required `version`
and an optional `colors` block. Every color is a `#RRGGBB` hex string (the
leading `#` is optional).

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

This is **every** theme field, set to its **default** value — save it as
`themes/<your name>.kdl`, point `koshi.kdl` at that name, and change the colors
you like. Any color you delete just restores its default.

For themes that are already built, see
[`themes-example/`](../themes-example/).

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
