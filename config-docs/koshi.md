# `koshi.kdl` — app settings

The main settings file: which color theme to use, scrollback size, mouse
behavior, the default split direction, what koshi advertises to child programs,
and the self-update check. Every setting is optional and falls back to the
default shown below.

**Where it goes:** directly in the config directory — `~/.config/koshi/koshi.kdl`
on Linux, `~/Library/Application Support/koshi/koshi.kdl` on macOS,
`%APPDATA%\koshi\config\koshi.kdl` on Windows. See [README](README.md#where-the-files-go).

**If a field is wrong:** it is skipped (keeps its default) and koshi logs it;
every other field still applies. The one exception is the `update` section,
which is stricter — see its note below.

Settings are grouped into sections. Each section is a block; each field inside
it is one node with one value. `theme` is the one setting that stands on its
own, outside any block.

## `theme`

Which color theme koshi draws its chrome with. The value is the name of a file
in the `themes/` subdirectory: `theme "midnight"` reads
`themes/midnight.kdl`. The colors live in that file, never here.

`"default"` is koshi's built-in theme. Naming it — or leaving the line out, or
naming a theme koshi cannot load — draws the built-in colors. See
[theme.md](theme.md).

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

koshi writes a per-session log file, `logs/koshi-log-<session-id>.log`, under
the state directory. Disabled, no file and no `logs/` directory are ever
created; enabled, the file is created on the first log line at or above `level`
and appended thereafter.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `enabled` | boolean — write a log file at all | `#false` | ≥ 0.1.0 |
| `level` | `"info"` \| `"warning"` \| `"error"` — lowest severity written: `info` writes everything, `warning` writes warnings and errors, `error` writes only errors | `"warning"` | ≥ 0.1.0 |
| `format` | `"pretty"` \| `"json"` — `pretty` is human-readable, `json` is one JSON object per line for a machine to parse | `"pretty"` | ≥ 0.1.0 |

What you get at each level:

- `info` — everything that worked, as it happens: config files read, config
  applied, terminal ready, session started, panes and tabs opening and closing,
  focus moving, a pane's process exiting. Set this when you want to see what
  koshi did.
- `warning` — only the things that went wrong but that koshi had an answer for,
  so it kept running: a profile that would not parse (one plain shell starts
  instead), a `keybinding.kdl` with a conflict (the built-in keys stay), a
  command that was rejected.
- `error` — only the things koshi could not work around at all, after which it
  exits: it could not enter raw mode, could not build its output terminal, could
  not start the shell, or panicked.

Each level includes the ones below it, so `info` writes all three. The default
`warning` records failures only; if you turn logging on to follow what koshi is
doing, set `level "info"`.

Log lines carry ids, never content. A copied selection is recorded as its byte
count, not its text; what you type into a pane is never written at any level.

## `update`

The self-update check. **This section is strict:** a bad value here drops the
*whole* `koshi.kdl` to defaults for the run, because `auto-check` gates a
network call and a typo must never silently turn it back on.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `auto-check` | boolean — check GitHub for a newer koshi at startup | `#true` | ≥ 0.1.0 |
| `check-interval-days` | integer — days between checks | `14` | ≥ 0.1.0 |
| `allow-prerelease` | boolean — offer pre-release builds too | `#false` | ≥ 0.1.0 |

## Full example

This is **every** `koshi.kdl` field, set to its **default** value — copy it as a
complete baseline and change what you like. Every section and every field is
optional; deleting any line just restores that default.

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
    default-shell "/bin/zsh"     // default: omit this line to use your $SHELL
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
