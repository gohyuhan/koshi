<p align="center">
  <img src="assets/koshi.png" alt="koshi logo" width="150">
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Rust-000000?style=for-the-badge&logo=rust&logoColor=white" alt="Rust">
  <img src="https://img.shields.io/github/v/release/gohyuhan/koshi?style=for-the-badge" alt="Release">
  <img src="https://img.shields.io/github/license/gohyuhan/koshi?style=for-the-badge" alt="License">
  <img src="https://img.shields.io/github/actions/workflow/status/gohyuhan/koshi/ci.yaml?style=for-the-badge&label=CI" alt="CI">
</p>

<p align="center">
  <strong>A terminal multiplexer in Rust — split panes and tabs in one terminal window.</strong><br>
</p>

# koshi

> ⚠️ **koshi is in active development.** The author uses it as a daily driver,
> so it changes often — commands, config fields, keybindings, and defaults can
> all shift between commits. Expect that to keep being true.

## Requirements

- A terminal emulator that supports **true color** and **256 colors** (any modern one does)
- **Linux** or **macOS**; Windows is targeted but far less exercised
- **Rust 1.96+** — you build from source, there are no binaries yet

## Description

koshi is a terminal multiplexer: it runs inside one real terminal window and
gives you many independent terminals inside it. Each pane owns its own shell,
its own screen, and its own scrollback. Panes tile inside a tab, and tabs sit
inside one session.

To the terminal you launched it from, koshi is one program. To every shell it
starts, koshi *is* the terminal — it handles colors, cursor movement, and mouse
input itself, which is why programs like `vim`, `htop`, and `less` work normally
inside a pane.

## Why koshi?

koshi tries to keep its core small — tabs, panes, layout, and lock mode — and
leave the rest out of the binary. No Git panels, dashboards, or launchers
bundled in.

## Features

- 🪟 **Split Panes** — split any pane left, right, up, or down; the panes fill the tab, with no gaps left behind
- 🗂️ **Stacked Panes** — put several panes in one slot and flip between them, instead of shrinking everything
- 🔍 **Fullscreen Zoom** — blow one pane up to the whole tab and back, without losing the layout
- 📐 **Resize** — nudge a pane border one character at a time by key, or drag it with the mouse
- 📑 **Tabs** — create, close, rename, and cycle tabs; the tab bar scrolls when it runs out of room
- ⌨️ **Multi-key Shortcuts** — chained keys (`<C-p> n`), a leader key you pick, a hint bar showing what comes next, and clash detection
- 🔒 **Lock Mode** — send every key straight to the program in the pane, so koshi's own shortcuts stop stealing them
- 🖱️ **Mouse Support** — click to focus, drag borders to resize, scroll each pane, double-click and drag to select
- 📋 **Copy to Clipboard** — select with the mouse and copy to your real clipboard, working over SSH too
- 🎯 **Mouse Select Mode** — take the mouse back from a full-screen program that is using it, so you can select text
- 📜 **Per-pane History** — each pane keeps its own scrollback and its own place in it
- 🧾 **Terminal Behavior** — true color, bold and italics, full-screen programs, wide characters like CJK and emoji, box drawing
- 🎨 **Themes** — 25 ready-made ones included (Dracula, Gruvbox, Nord, Catppuccin, Tokyo Night, Rosé Pine, Solarized, and more)
- ⚙️ **Simple Config Files** — app settings, themes, shortcuts, and saved layouts, each in its own readable file
- 💾 **Saved Layouts** — save a set of tabs, panes, and the commands they run, then open the lot with one flag
- 🪵 **Logging** — optional log file per session, plain text or JSON, recording ids and never your content
- 🌍 **Cross-platform** — koshi targets Linux, macOS, and Windows; it is developed on macOS and CI currently builds and tests on Linux only

## Installation

> 🚧 **Coming soon.**

### Build from source

```bash
git clone https://github.com/gohyuhan/koshi.git
cd koshi
cargo build --release

# the binary lands here
./target/release/koshi
```

## Quick Start

Launch koshi:

```bash
koshi
```

That opens one tab with one pane running your shell.

Open a saved layout instead:

```bash
koshi --profile dev
```

### Default keybindings

The leader key is **Ctrl** by default, so the shortcuts below start with `Ctrl`
held down. Change the leader once and they all move together.

| Keys | Does |
|---|---|
| `<C-p> n` | New pane (default direction) |
| `<C-p> h` / `j` / `k` / `l` | New pane left / down / up / right |
| `<C-p> x` | Close the pane and everything it started |
| `<C-p> ←` `↓` `↑` `→` | Move focus to the neighbouring pane |
| `<C-s> ←` `↓` `↑` `→` | Move this pane's border one cell |
| `<C-t> n` | New tab |
| `<C-t> x` | Close tab |
| `Tab` / `Shift+Tab` | Next / previous tab |
| `Alt+f` | Toggle fullscreen on the focused pane |
| `<C-g>` | Toggle mouse select mode |
| `<C-l>` | Lock / unlock (keys pass straight to the program) |
| `<C-q>` | Quit |

Run `koshi keys list` to see the shortcuts actually in effect, and
`koshi actions list` for everything you can bind.

Some actions ship without a default key — stacked panes, renaming a pane or a
tab, plain close-pane. Bind them yourself in `keybinding.kdl`.

## Configuration

koshi reads four kinds of [KDL](https://kdl.dev) file, all optional. With none
of them, koshi runs on its built-in defaults.

| File | Sets |
|---|---|
| `koshi.kdl` | App settings: theme, scrollback, mouse, split direction, logging, updates |
| `themes/<name>.kdl` | Colors for borders, tab bar, and accents |
| `keybinding.kdl` | Shortcuts and the modes they live in |
| `profile/<name>.kdl` | A saved layout: tabs, panes, and the commands they run |

They live in one directory per platform:

| Platform | Config directory |
|---|---|
| Linux | `~/.config/koshi` |
| macOS | `~/Library/Application Support/koshi` |
| Windows | `%APPDATA%\koshi\config` |

Full reference: [config-docs/](config-docs/README.md). Ready-made themes to copy
into `themes/`: [themes-example/](themes-example/).

## CLI Reference

### Launching

| Command | Does |
|---|---|
| `koshi` | Start koshi with one tab and one shell pane |
| `koshi --profile <NAME>` | Start with the saved layout in `profile/<NAME>.kdl` |

### Shortcuts

Every one of these takes `--format table` (default) or `--format json`. They all
read the built-in shortcuts with your `keybinding.kdl` folded on top; nothing
here changes a shortcut, since the file is the only place they are set.

| Command | Does |
|---|---|
| `koshi keys list [--mode <MODE>] [--scope default\|user]` | The shortcuts actually in effect |
| `koshi keys describe "<KEY_SEQUENCE>"` | What a key sequence does and which file set it |
| `koshi keys conflicts` | Clashing shortcuts, ones that can never fire, and warnings |
| `koshi keys validate <PATH>` | Check a shortcut file without applying it |

### Actions

| Command | Does |
|---|---|
| `koshi actions list [--format table\|json]` | Everything you can bind, and where it applies |
| `koshi actions explain <ACTION> [--format table\|json]` | One action: where it applies, what it can aim at, examples |

### Updating

| Command | Does |
|---|---|
| `koshi update` | Check for a newer koshi and install it |

## Changelog

### [v0.1.0] — coming soon

First release. What it covers:

- Split panes, stacked panes, fullscreen zoom, and border resize
- Tabs: create, close, rename, cycle
- Terminal behavior: true color, text styles, full-screen programs, wide characters, per-pane scrollback
- Multi-key shortcuts with a leader key, a hint bar, and clash detection
- Lock mode to pass every key through to the program in the pane
- Mouse: click, drag to resize, scroll, select, and copy to the clipboard
- Config files for settings, themes, shortcuts, and saved layouts, with 25 themes included
- Optional logging and self-update

## License

MIT License - see [LICENSE](LICENSE) file for details
