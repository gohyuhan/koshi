# `profile/<name>.kdl` — saved layouts

A profile defines tabs, pane layouts, and pane commands. Load one by name:

```
koshi --profile dev
```

This reads `profile/dev.kdl` instead of opening one shell pane.

**Where it goes:** in a `profile/` subdirectory of the config directory —
`~/.config/koshi/profile/dev.kdl` on Linux,
`~/Library/Application Support/koshi/profile/dev.kdl` on macOS,
`%APPDATA%\koshi\config\profile\dev.kdl` on Windows. See
[README](README.md#where-the-files-go).

**If anything is wrong, the whole profile is dropped.** Koshi starts one shell
instead. A missing named profile uses the same fallback. `koshi config check`
validates every saved profile and names each problem with its line and column.

## Structure

A profile is one or more `tab` blocks. A `version` line is required.

| Node | Meaning | Since |
|---|---|---|
| `version <n>` | Required schema version | ≥ 0.1.0 |
| `tab { … }` | One tab. Its children are the pane arrangement. | ≥ 0.1.0 |
| `pane { … }` | A terminal pane. | ≥ 0.1.0 |
| `horizontal { … }` | Split its children left to right. | ≥ 0.1.0 |
| `vertical { … }` | Split its children top to bottom. | ≥ 0.1.0 |
| `stack { … }` | Its children share one rectangle; one is expanded. | ≥ 0.1.0 |
| `lock` | Bare marker. Start the session's first terminal with input locked. | ≥ 0.4.0 |

## Inside a `pane`

| Key | Value / type | Since |
|---|---|---|
| `command "prog" "arg"…` | The program to run and its arguments. Omit for the default shell. | ≥ 0.1.0 |
| `cwd "/path"` | Working directory, used as written. Use an absolute path; `~` is not expanded. | ≥ 0.1.0 |
| `env "NAME" "VALUE"` | An environment variable for this pane (repeatable). | ≥ 0.1.0 |
| `focus` | The session's first terminal starts with this pane focused. One per tab. | ≥ 0.1.0 |

Omitting `cwd` inherits the directory where koshi was launched. Example:
launch from `/home/me/proj` + `pane` results in that pane starting in
`/home/me/proj`. An explicit `cwd "/srv/app"` still wins.

## Sizing (only inside `horizontal` / `vertical`)

Each child of a split may carry sizing. Without any, children share the space
equally. `size` and `weight` are the same slot: give one of the two, once.
Every sizing node anywhere else, such as on a stack member or on the one node
of a `tab`, is a problem of its own.

| Key | Value / type | Since |
|---|---|---|
| `size 40` / `size "60%"` | A fixed size: 1–65535 cells, or 1–100 percent of the split. | ≥ 0.1.0 |
| `weight 2` | A relative share of the leftover space, 1–4294967295. | ≥ 0.1.0 |
| `min 10` | Never shrink below this many cells, 1–65535. | ≥ 0.1.0 |
| `preferred 30` | The size to aim for when there is room, 1–65535 cells. A resize moves the pane off it. | ≥ 0.1.0 |

## Stacks

Inside a `stack`, `expanded` marks the one member shown open; the rest collapse
to a one-row header. A client that then focuses another member of the stack
shows that member open on its own screen.

## Starting locked

A bare top-level `lock` node starts the session's first terminal in locked
input mode: keys go straight to the program in the focused pane, and Koshi
shortcuts do nothing until the lock is released. `koshi unlock` and
`koshi toggle-lock` release it, and so does the `<C-l>` binding.

`lock` takes no values and no children, and may appear once. A second `lock`,
or a `lock` carrying a value, drops the whole profile.

Terminals that attach later start unlocked: the node names the state the first
terminal opens in, not a setting the session keeps.

```kdl
version 2
lock

tab {
    pane {
        command "htop"
    }
}
```

## Focus

`focus` inside a `pane` marks the pane that the session's first terminal
focuses in that tab (one per tab). `focus` as a direct child of a `tab` marks
the tab that the session's first terminal opens on (one per profile). Without
either, the first terminal opens on the first tab, and focuses the first pane
that shows in each tab. In a `stack`, that pane is the `expanded` member.

Every terminal that attaches after the first one opens on the first tab,
focused on its first pane that shows. The markers name where the first
terminal opens, not a setting the session keeps. Example: with `tab { pane }`
and then `tab { focus; pane }`, the terminal that runs `koshi --profile` opens
on the second tab, and a `koshi attach` from another terminal opens on the
first tab.

## Full example

This uses every layout form, every pane setting, and every sizing key.

```kdl
// profile/dev.kdl — every layout form, pane setting, and sizing key.
version 2

tab {
    // a horizontal split: editor on the left (60%), a tools column (40%) right
    horizontal {
        pane {
            command "nvim" "src/main.rs"    // program + its arguments
            cwd "/home/me/proj"             // absolute path (~ is not expanded)
            env "RUST_LOG" "debug"          // one env var...
            env "NO_COLOR" "1"              // ...repeat for more
            size "60%"                      // fixed share of the split
            focus                           // the first terminal focuses this pane
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
    focus                                    // the first terminal opens on this tab

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
```
