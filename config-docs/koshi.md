# `koshi.kdl` — app settings

Main settings for theme, panes, scrollback, layout, mouse, terminal values,
logging, and updates. `version` is required. Other settings are optional.

**Where it goes:** directly in the config directory — `~/.config/koshi/koshi.kdl`
on Linux, `~/Library/Application Support/koshi/koshi.kdl` on macOS,
`%APPDATA%\koshi\config\koshi.kdl` on Windows. See [README](README.md#where-the-files-go).

**Bad fields:** startup skips them, keeps their defaults, and logs each one.
`koshi config check` and `migrate` reject them. A bad value in `update` rejects
the whole app file for that launch.

Settings use blocks. `theme` is top-level.

## `theme`

`theme "midnight"` loads `themes/midnight.kdl`. Missing, invalid, omitted, or
`"default"` themes use built-in colors. See [theme.md](theme.md).

| Key | Value / type | Default | Since |
|---|---|---|---|
| `theme` | string — the `themes/<name>.kdl` to use, without the `.kdl` | `"default"` | ≥ 0.1.0 |

## `pane`

| Key | Value / type | Default | Since |
|---|---|---|---|
| `min-cols` | integer — smallest width a pane may shrink to | `2` | ≥ 0.1.0 |
| `min-rows` | integer — smallest height a pane may shrink to | `1` | ≥ 0.1.0 |

## `scrollback`

| Key | Value / type | Default | Since |
|---|---|---|---|
| `max-lines` | integer — lines of history kept per pane (a negative value means `0`: no scrollback) | `10000` | ≥ 0.1.0 |
| `max-bytes` | integer — byte ceiling on that history (negative means `0`) | `33554432` (32 MiB) | ≥ 0.1.0 |
| `scroll-on-input` | boolean — when you have scrolled up into history, typing or pasting into the pane snaps the view back to the newest line (`#false` keeps it parked while the input still goes through). Only the primary screen follows; the alternate screen is left to the full-screen program on it | `#true` | ≥ 0.1.0 |

## `layout`

| Key | Value / type | Default | Since |
|---|---|---|---|
| `new-pane-direction` | `"left"` \| `"right"` \| `"up"` \| `"down"` — where a new pane opens when the command does not say | `"right"` | ≥ 0.1.0 |

## `mouse`

| Key | Value / type | Default | Since |
|---|---|---|---|
| `border-resize` | boolean — drag a pane border to resize it | `#true` | ≥ 0.1.0 |
| `scroll-lines` | integer — lines per wheel notch | `3` | ≥ 0.1.0 |
| `wheel` | `"scroll-scrollback"` (scroll koshi's history) \| `"ignore"` | `"scroll-scrollback"` | ≥ 0.1.0 |

## `copy`

| Key | Value / type | Default | Since |
|---|---|---|---|
| `trim-trailing-whitespace` | boolean — drop trailing blanks from copied lines | `#true` | ≥ 0.1.0 |

## `terminal`

| Key | Value / type | Default | Since |
|---|---|---|---|
| `term` | string — the `TERM` value child programs see | `"xterm-256color"` | ≥ 0.1.0 |
| `colorterm` | string — the `COLORTERM` value child programs see | `"truecolor"` | ≥ 0.1.0 |
| `default-shell` | string — the shell to launch | your `$SHELL` (`%COMSPEC%` on Windows) | ≥ 0.1.0 |

## `logging`

Koshi writes `logs/koshi-log-<session-id>.log` below the state directory.
Disabled logging creates no log file.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `enabled` | boolean — write a log file at all | `#false` | ≥ 0.1.0 |
| `level` | `"info"` \| `"warning"` \| `"error"` — lowest severity written: `info` writes everything, `warning` writes warnings and errors, `error` writes only errors | `"warning"` | ≥ 0.1.0 |
| `format` | `"pretty"` \| `"json"` — `pretty` is human-readable, `json` is one JSON object per line for a machine to parse | `"pretty"` | ≥ 0.1.0 |

`info` includes normal lifecycle events. `warning` includes recoverable
problems. `error` includes failures that stop Koshi. Each level includes higher
severity. Logs store ids and byte counts, not typed or copied text.

## `update`

Self-update settings. A bad value here drops the whole `koshi.kdl` for that
launch.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `auto-check` | boolean — check GitHub for a newer koshi at startup | `#true` | ≥ 0.1.0 |
| `check-interval-days` | integer — days between checks | `14` | ≥ 0.1.0 |
| `allow-prerelease` | boolean — offer pre-release builds too | `#false` | ≥ 0.1.0 |

## Full example

This shows every app setting. Fixed values match defaults. `default-shell` is
commented because its default comes from `$SHELL` or `%COMSPEC%`.

```kdl
// koshi.kdl — the complete default configuration.
version 1

theme "default"

pane {
    min-cols 2
    min-rows 1
}

scrollback {
    max-lines 10000
    max-bytes 33554432       // 32 MiB
    scroll-on-input #true
}

layout {
    new-pane-direction "right"
}

mouse {
    border-resize #true
    scroll-lines 3
    wheel "scroll-scrollback"
}

copy {
    trim-trailing-whitespace #true
}

terminal {
    term "xterm-256color"
    colorterm "truecolor"
    // default-shell "/bin/zsh"  // optional override
}

logging {
    enabled #false
    level "warning"
    format "pretty"
}

update {
    auto-check #true
    check-interval-days 14
    allow-prerelease #false
}
```
