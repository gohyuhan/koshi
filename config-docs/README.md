# koshi configuration

koshi reads four kinds of KDL file. Each is optional — with none of them, koshi
runs on its built-in defaults.

| File | What it sets | Reference |
|---|---|---|
| `koshi.kdl` | App settings: scrollback, mouse, split direction, terminal, updates | [koshi.md](koshi.md) |
| `theme.kdl` | Chrome colors (borders, tab ribbon, accents) | [theme.md](theme.md) |
| `keybinding.kdl` | Key bindings and the modes they live in | [keybinding.md](keybinding.md) |
| `profile/<name>.kdl` | A saved session layout: tabs, panes, commands, opened with `koshi --profile <name>` | [profile.md](profile.md) |

## Where the files go

koshi looks in one config directory. The three top-level files sit directly in
it; profiles sit in a `profile/` subdirectory.

| Platform | Config directory |
|---|---|
| Linux | `~/.config/koshi` |
| macOS | `~/Library/Application Support/koshi` |
| Windows | `%APPDATA%\koshi\config` |

Set `KOSHI_CONFIG_DIR` to an absolute path to override the location on any
platform. On Linux the usual `XDG_CONFIG_HOME` also moves the base.

So a full config directory looks like:

```
<config dir>/
    koshi.kdl
    theme.kdl
    keybinding.kdl
    profile/
        dev.kdl
        writing.kdl
```

## Versions

Every field table has a **Since** column: the lowest koshi version that
understands that field. A field newer than your koshi is simply ignored, so a
config written for a newer koshi still works on an older one (minus the newer
fields). Every field so far has been here since **0.1.0**, the first release.

A file may declare its own `version` (an integer). koshi rejects a file whose
version is newer than the running build understands, and otherwise ignores it.

## When a file has a mistake

- **`koshi.kdl` and `theme.kdl` are field-partial.** One bad field (a typo, the
  wrong kind of value) is skipped — that setting keeps its default — and every
  other field in the file still applies. koshi logs which field it skipped.
- **`keybinding.kdl` and `profile/<name>.kdl` are all-or-nothing.** Any error
  in the file drops the *whole* file back to defaults, because a half-applied
  keymap or a half-built layout (some panes spawned, some silently missing) is
  worse than a clean fallback.
