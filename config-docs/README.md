# koshi configuration

koshi reads four kinds of KDL file. Each is optional — with none of them, koshi
runs on its built-in defaults.

| File | What it sets | Reference |
|---|---|---|
| `koshi.kdl` | App settings: which theme, scrollback, mouse, split direction, terminal, updates | [koshi.md](koshi.md) |
| `themes/<name>.kdl` | Chrome colors (borders, tab ribbon, accents), selected by `koshi.kdl`'s `theme "<name>"` | [theme.md](theme.md) |
| `keybinding.kdl` | Key bindings and the modes they live in | [keybinding.md](keybinding.md) |
| `profile/<name>.kdl` | A saved session layout: tabs, panes, commands, opened with `koshi --profile <name>` | [profile.md](profile.md) |

## Where the files go

koshi looks in one config directory. The two top-level files sit directly in
it; themes and profiles sit in their own subdirectories, one file per theme and
one per profile.

| Platform | Config directory |
|---|---|
| Linux | `~/.config/koshi` |
| macOS | `~/Library/Application Support/koshi` |
| Windows | `%APPDATA%\koshi\config` |

The location is fixed per platform — koshi reads no environment variable to
relocate its config. On Linux the usual `XDG_CONFIG_HOME` moves the base, since
that is the OS's own base-directory rule.

So a full config directory looks like:

```
<config dir>/
    koshi.kdl
    keybinding.kdl
    themes/
        midnight.kdl
        solarized.kdl
    profile/
        dev.kdl
        writing.kdl
```

Only one theme is in effect at a time — the one `koshi.kdl` names with
`theme "<name>"`. The others just sit there until you switch.

## Versions

Every field table has a **Since** column: the lowest koshi version that
understands that field. A field newer than your koshi is simply ignored, so a
config written for a newer koshi still works on an older one (minus the newer
fields). Every field so far has been here since **0.1.0**, the first release.

A file may declare its own `version` (an integer). koshi rejects a file whose
version is newer than the running build understands, and otherwise ignores it.

## When a file has a mistake

- **`koshi.kdl` and `themes/<name>.kdl` are field-partial.** One bad field (a
  typo, the wrong kind of value) is skipped — that setting keeps its default —
  and every other field in the file still applies. koshi logs which field it
  skipped.
- **`keybinding.kdl` and `profile/<name>.kdl` are all-or-nothing.** Any error
  in the file drops the *whole* file back to defaults, because a half-applied
  keymap or a half-built layout (some panes spawned, some silently missing) is
  worse than a clean fallback.
- **A theme koshi cannot find or parse** falls back to the built-in `default`
  theme, with the reason logged. The rest of `koshi.kdl` still applies.
