# koshi configuration

koshi reads four kinds of KDL file. Each file is optional — with none of them,
koshi runs on its built-in defaults. Every file that exists must start with a
supported `version`.

| File | What it sets | Reference |
|---|---|---|
| `koshi.kdl` | App settings: which theme, scrollback, mouse, split direction, terminal, updates | [koshi.md](koshi.md) |
| `themes/<name>.kdl` | Chrome colors (borders, tab ribbon, accents), selected by `koshi.kdl`'s `theme "<name>"` | [theme.md](theme.md) |
| `keybinding.kdl` | Key bindings and the modes they live in | [keybinding.md](keybinding.md) |
| `profile/<name>.kdl` | A saved session layout: tabs, panes, commands, opened with `koshi --profile <name>` | [profile.md](profile.md) |

Command-line reference: [cli.md](cli.md).

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

You do not have to write a theme yourself: [`themes-example/`](../themes-example/)
ships 25 ready-made ones (Dracula, Gruvbox, Nord, Catppuccin, Tokyo Night, Rosé
Pine, Solarized and more) to copy into `themes/`.

## Versions and migration

Every file must declare one top-level `version` with one integer argument,
starting at 1, and no child block:

```kdl
version 1
```

`koshi config check` validates every present `koshi.kdl`, `keybinding.kdl`,
`themes/*.kdl`, and `profile/*.kdl` without changing them. A missing version,
bad KDL, bad value, unknown key, or unsupported version makes the check fail.
Each matching path must be a regular file. The check reports all file-read and
schema errors it can find in one run. A symbolic link to a regular file is
allowed; migration updates its target and keeps the link.

`koshi config migrate` first validates every present file. It then moves each
old file through every version in order: version 1 to 2, then 2 to 3, until it
reaches the newest schema this Koshi supports. Every step is checked before the
next step starts. A bad old file or missing step stops migration before any
config file is written.

Migration helps a valid old file adopt a valid new shape. It does not repair a
file that was already wrong. Each changed file is replaced atomically, so a
reader sees the whole old file or the whole new file, never half a write. Koshi
never runs migration during startup; only this command changes config files.

## When a file has a mistake

- **`koshi.kdl` and `themes/<name>.kdl` are field-partial.** One bad field (a
  typo, the wrong kind of value) is skipped — that setting keeps its default —
  and every other field in the file still applies. koshi logs which field it
  skipped.
- **`keybinding.kdl` and `profile/<name>.kdl` are all-or-nothing.** Any error
  in the file drops the *whole* file back to defaults, because a half-applied
  keymap or a half-built layout (some panes spawned, some silently missing) is
  worse than a clean fallback.
- **Unknown keys name the nearest valid key.** `min-col 2` results in
  ``did you mean `min-cols`?``. `koshi config check` treats that typo as an
  error even though normal startup can keep the other field-partial settings.
- **A theme koshi cannot find or parse** falls back to the built-in `default`
  theme, with the reason logged. The rest of `koshi.kdl` still applies.
