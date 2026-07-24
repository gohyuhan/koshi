# koshi configuration

Koshi reads four optional KDL file types. With none present, built-in defaults
apply. Every present file must declare a supported `version`.

| File | What it sets | Reference |
|---|---|---|
| `koshi.kdl` | Theme, pane, scrollback, layout, mouse, copy, terminal, logging, and update settings | [koshi.md](koshi.md) |
| `themes/<name>.kdl` | Interface colors selected by `theme "<name>"` | [theme.md](theme.md) |
| `keybinding.kdl` | Key bindings and the modes they live in | [keybinding.md](keybinding.md) |
| `profile/<name>.kdl` | A saved session layout: tabs, panes, commands, opened with `koshi --profile <name>` | [profile.md](profile.md) |

Command-line reference: [cli.md](cli.md).

## Where the files go

The two top-level files sit in the config directory. Themes and profiles use
their own subdirectories.

| Platform | Config directory |
|---|---|
| Linux | `~/.config/koshi` |
| macOS | `~/Library/Application Support/koshi` |
| Windows | `%APPDATA%\koshi\config` |

Koshi has no config-path override. Linux still follows `XDG_CONFIG_HOME`.

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

`koshi.kdl` selects one theme with `theme "<name>"`.

[`themes-example/`](../themes-example/) contains 25 themes ready to copy.

## Versions and migration

Every file must declare one top-level `version` with one integer argument,
starting at 1, and no child block:

```kdl
version 1
```

`koshi config check` validates every present known file without changing it.
Missing versions, bad KDL, bad values, unknown keys, and unsupported versions
fail the check. Errors from all files are reported together.

`koshi config migrate` validates every file before writing. It applies each
registered version step in order and validates after each step. Invalid input
or a missing step stops migration before any file is written.

Current schema version is `1`, so valid version `1` files are reported as
current and stay unchanged. Migration does not repair invalid config and never
runs during startup.

Changed files use atomic replacement, one file at a time. Config symlinks stay;
their regular-file targets change. A write error lists earlier completed files
and marks the failed file as possibly changed.

## When a file has a mistake

- **`koshi.kdl` and themes are field-partial.** A bad field keeps its default;
  other valid fields still apply. Koshi logs each skipped field.
- **Keybindings and profiles are all-or-nothing.** Any error drops the whole
  file.
- **Unknown keys name the nearest valid key.** `min-col 2` results in
  ``did you mean `min-cols`?``. `koshi config check` treats that typo as an
  error even though normal startup can keep the other field-partial settings.
- **A missing or invalid theme** uses built-in `default` colors and logs why.
