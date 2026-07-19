# `profile/<name>.kdl` — saved session layouts

A profile is a saved session: which tabs to open, how each tab's panes are
arranged, and what command each pane runs. Unlike the other files, a profile is
**never loaded automatically** — you ask for one by name:

```
koshi --profile dev
```

which reads `profile/dev.kdl` and opens its tabs and panes instead of a single
shell.

**Where it goes:** in a `profile/` subdirectory of the config directory —
`~/.config/koshi/profile/dev.kdl` on Linux,
`~/Library/Application Support/koshi/profile/dev.kdl` on macOS,
`%APPDATA%\koshi\config\profile\dev.kdl` on Windows. See
[README](README.md#where-the-files-go).

**If anything is wrong: the whole profile is dropped** (all-or-nothing) and
koshi starts a single shell instead, because a half-built layout — some panes
spawned, some silently missing — is worse than a clean fallback. A profile that
names a profile which does not exist also falls back to a single shell.

> **Plugin panes are not supported yet.** A profile that uses a `plugin` pane
> cannot launch (there is no plugin host to fill it) and falls back to a single
> shell. Everything else below works today.

## Structure

A profile is one or more `tab` blocks. A `version` line is required.

| Node | Meaning | Since |
|---|---|---|
| `version <n>` | Required schema version | ≥ 0.1.0 |
| `tab { … }` | One tab. Its children are the pane arrangement. | ≥ 0.1.0 |
| `pane { … }` | A terminal pane. | ≥ 0.1.0 |
| `plugin "name"` | A plugin pane *(not supported yet — see note above)*. | ≥ 0.1.0 |
| `horizontal { … }` | Split its children left to right. | ≥ 0.1.0 |
| `vertical { … }` | Split its children top to bottom. | ≥ 0.1.0 |
| `stack { … }` | Its children share one rectangle; one is expanded. | ≥ 0.1.0 |

## Inside a `pane`

| Key | Value / type | Since |
|---|---|---|
| `command "prog" "arg"…` | The program to run and its arguments. Omit for the default shell. | ≥ 0.1.0 |
| `cwd "/path"` | Working directory, used as written. Use an absolute path — `~` is **not** expanded yet. | ≥ 0.1.0 |
| `env "NAME" "VALUE"` | An environment variable for this pane (repeatable). | ≥ 0.1.0 |
| `focus` | Start with this pane focused. One per tab. | ≥ 0.1.0 |

## Sizing (only inside `horizontal` / `vertical`)

Each child of a split may carry sizing. Without any, children share the space
equally.

| Key | Value / type | Since |
|---|---|---|
| `size 40` / `size "60%"` | A fixed size, in cells or a percent of the split. | ≥ 0.1.0 |
| `weight 2` | A relative share of the leftover space. | ≥ 0.1.0 |
| `min 10` | Never shrink below this many cells. | ≥ 0.1.0 |
| `preferred 30` | The size to aim for when there is room. | ≥ 0.1.0 |

## Stacks

Inside a `stack`, `expanded` marks the one member shown open; the rest collapse
to a one-row header.

## Focus

`focus` inside a `pane` marks the pane that starts focused (one per tab).
`focus` as a direct child of a `tab` marks the tab that starts active (one per
profile). Without either, the first pane and the first tab start focused.

## Full example

This exercises **every** profile feature: all three split kinds, every pane
setting, every sizing key, and a stack.

```kdl
// profile/dev.kdl — a complete profile using every feature.
version 1

tab {
    // a horizontal split: editor on the left (60%), a tools column (40%) right
    horizontal {
        pane {
            command "nvim" "src/main.rs"    // program + its arguments
            cwd "/home/me/proj"             // absolute path (~ is not expanded)
            env "RUST_LOG" "debug"          // one env var...
            env "NO_COLOR" "1"              // ...repeat for more
            size "60%"                      // fixed share of the split
            focus                           // this pane starts focused
        }
        vertical {
            size "40%"
            pane {
                command "cargo" "watch" "-x" "test"
                cwd "/home/me/proj"
                weight 2                     // twice the leftover share of...
                min 5                        // ...but never below 5 rows,
                preferred 20                 // ...aiming for 20 when there's room
            }
            pane {
                cwd "/home/me/proj"          // no command → the default shell
                weight 1
            }
        }
    }
}

tab {
    focus                                    // this tab starts active

    // a stack: members share one rectangle; `expanded` is the open one
    stack {
        pane {
            command "journalctl" "-f"
        }
        pane {
            command "htop"
            expanded                         // this member starts open
        }
    }
}

// A plugin pane is written `plugin "name"`, but plugin panes are not supported
// yet, so a profile using one falls back to a single shell:
//   tab { plugin "file-tree" }
```
