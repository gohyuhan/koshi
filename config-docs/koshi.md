# `koshi.kdl` — app settings

Main settings for theme, panes, scrollback, layout, mouse, terminal values,
logging, updates, beta features, session closing, and who else on this machine
may reach your sessions. `version` is required. Other settings are optional.

**Where it goes:** directly in the config directory — `~/.config/koshi/koshi.kdl`
on Linux, `~/Library/Application Support/koshi/koshi.kdl` on macOS,
`%APPDATA%\koshi\config\koshi.kdl` on Windows. See [README](README.md#where-the-files-go).

**Bad fields:** startup skips them, keeps their defaults, and logs each one.
`koshi config check` and `migrate` reject them. A bad value in `update` rejects
the whole app file for that launch.

Settings use blocks. `theme`, `allow-beta-features`, `allow-other-users`,
`remote-listen`, `shared-sessions-dir` and `auto-close-session` are top-level.

**Whose settings they are:** some belong to the session and are shared by every
terminal looking at it; the rest belong to the terminal you are sitting at,
which reads its own `koshi.kdl`, `themes/<name>.kdl`, and `keybinding.kdl`. Two
terminals showing one session can differ on those. Each section below says
which.

## `theme`

`theme "midnight"` loads `themes/midnight.kdl`. Missing, invalid, omitted, or
`"default"` themes use built-in colors. Each terminal reads this for itself, so
two terminals showing one session can wear different colors. See
[theme.md](theme.md).

| Key | Value / type | Default | Since |
|---|---|---|---|
| `theme` | string — the `themes/<name>.kdl` to use, without the `.kdl` | `"default"` | ≥ 0.1.0 |

## `pane`

The session reads these: panes are the session's, so every terminal looking at
it sees the same sizes.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `min-cols` | integer — smallest width a pane may shrink to | `2` | ≥ 0.1.0 |
| `min-rows` | integer — smallest height a pane may shrink to | `1` | ≥ 0.1.0 |

## `scrollback`

`max-lines` and `max-bytes` size the history the session keeps, so every
terminal looking at the session gets the same amount. `scroll-on-input` is about
what one terminal's view does, but the session applies it from its own
`koshi.kdl` — so it too is the same for everyone.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `max-lines` | integer — lines of history kept per pane (a negative value means `0`: no scrollback) | `10000` | ≥ 0.1.0 |
| `max-bytes` | integer — byte ceiling on that history (negative means `0`) | `33554432` (32 MiB) | ≥ 0.1.0 |
| `scroll-on-input` | boolean — when you have scrolled up into history, typing or pasting into the pane snaps the view back to the newest line (`#false` keeps it parked while the input still goes through). Only the primary screen follows; the alternate screen is left to the full-screen program on it | `#true` | ≥ 0.1.0 |

## `layout`

| Key | Value / type | Default | Since |
|---|---|---|---|
| `new-pane-direction` | `"left"` \| `"right"` \| `"up"` \| `"down"` — which side `new-pane` opens on, both the keybinding and `koshi new-pane`. Read by each client for itself, so two terminals viewing one session can differ. The `new-pane-<side>` keybindings and an explicit `--direction` name their own side and ignore this | `"right"` | ≥ 0.1.0 |

## `mouse`

Each terminal reads these for itself, since the mouse is its own.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `border-resize` | boolean — drag a pane border to resize it | `#true` | ≥ 0.1.0 |
| `scroll-lines` | integer — lines per wheel notch | `3` | ≥ 0.1.0 |
| `wheel` | `"scroll-scrollback"` (scroll koshi's history) \| `"ignore"` | `"scroll-scrollback"` | ≥ 0.1.0 |

## `copy`

Each terminal reads this for itself: the copy is made where the selection was
dragged.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `trim-trailing-whitespace` | boolean — drop trailing blanks from copied lines | `#true` | ≥ 0.1.0 |

## `terminal`

The session reads these: it starts the programs in the panes, so it is the one
that decides what they are told.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `term` | string — the `TERM` value child programs see | `"xterm-256color"` | ≥ 0.1.0 |
| `colorterm` | string — the `COLORTERM` value child programs see | `"truecolor"` | ≥ 0.1.0 |
| `default-shell` | string — the shell to launch | your `$SHELL` (`%COMSPEC%` on Windows) | ≥ 0.1.0 |

## `logging`

Neither side owns these: every koshi process reads them for its own log file.

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

### Crash reports

A crash report is separate from the log file. No setting turns it on or off.

If Koshi panics while you have a session open, it restores your terminal and
then writes `crash-<seconds-since-1970>.txt` in the data directory —
`~/.local/share/koshi` on Linux, `~/Library/Application Support/koshi` on
macOS, `%APPDATA%\koshi\data` on Windows. Attach that file to a bug report.

The file holds the Koshi version, the operating system and processor, the time,
the panic message, the source line that panicked, and the stack. Koshi reads
only those from the panic. It never reads pane content, scrollback, or your
keystrokes into the report.

Example: a panic at 2026-08-08 12:00:00 UTC writes `crash-1786190400.txt`.

## `update`

Self-update settings. Each installed koshi reads these from its own `koshi.kdl`
and updates itself. A bad value here drops the whole `koshi.kdl` for that
launch.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `auto-check` | boolean — check GitHub for a newer koshi at startup | `#true` | ≥ 0.1.0 |
| `check-interval-days` | integer — days between checks | `14` | ≥ 0.1.0 |
| `allow-prerelease` | boolean — offer pre-release builds too | `#false` | ≥ 0.1.0 |

## `allow-beta-features`

Some features are finished code that has not been used enough yet to be turned
on for everyone. Those are off unless you say otherwise. Turning this on runs
all of them; there is no per-feature switch.

Every koshi process reads this from your `koshi.kdl` when it starts, so the
interactive session and the `koshi` commands you type all get the same answer.

A beta feature you have not turned on refuses and says so, naming itself and the
line to add:

```text
koshi: `koshi <command>` is a beta feature and did nothing; add a top-level
`allow-beta-features #true` line to koshi.kdl to run it
```

Nothing crashes and nothing is lost; the command exits non-zero having done
nothing.

**0.2.0 marks no feature beta.** Every command in this release runs whether this
setting is on or off. `koshi`, `koshi attach` and `koshi --headless` were beta
before 0.2.0 and are now on for everyone. The setting stays for the features
that are marked beta next.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `allow-beta-features` | boolean — run features still marked beta | `#false` | ≥ 0.2.0 |

## `auto-close-session`

A terminal leaving a session normally leaves the session running with nothing
attached to it, so `koshi attach` can rejoin it later. Turning this on ends the
session once the last terminal leaves.

Koshi counts the terminals after the one that left is gone. If any are still
attached, the session keeps running; only an empty session is ended.

Ending it asks every program in the session to stop, waits up to three seconds,
then kills whatever has not exited. A shell writes its history and an editor
writes its swap file in that window. On Windows a program cannot be asked to
stop, so there is no window and everything is killed at once.
`koshi kill-session` skips the wait.

The session reads this, not each terminal: the session server takes the answer
from the `koshi.kdl` it read when the session started, so a terminal that
attaches later cannot change it from its own file.

Every way of leaving counts: the quit keybinding (`<leader>q` by default),
`koshi detach`, closing the terminal, and moving the terminal to another
session with `koshi attach <session>` from inside a pane. A terminal that moves
away has left, so a session it leaves empty ends.

Quit leaves the session; it never ends one on its own. With this setting off,
`<leader>q` detaches your terminal and the session keeps running. With it on,
`<leader>q` ends the session only when no other terminal is attached. To end a
session whatever this setting says, run `koshi kill-session`.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `auto-close-session` | boolean — end the session when its last terminal leaves | `#false` | ≥ 0.2.0 |

## `allow-other-users`

Your sessions are yours alone unless you say otherwise: no other user of this
machine can see them or reach them. Turning this on lets every other user
logged in to the same machine list your sessions, attach to them, and kill
them.

Both files have to say so. Your `koshi.kdl` is what opens your sessions to
other users; their own `koshi.kdl` is what makes their `koshi` look for
sessions that are not theirs. A user who leaves it off sees only their own
sessions, whatever your file says.

Turn it on for a machine several people share on purpose — a build box, a lab
machine, a pair-programming host. Leave it off on a laptop.

The programs inside a session keep running as the user who started the session,
whoever attaches. Attaching never hands anyone your account; it hands them a
view of, and typing into, panes that still run as you.

The session reads this, and so does every `koshi` command you type. The session
reads `koshi.kdl` again for every connection and every request another user
makes, so turning it off shuts those users out without a restart: a new
connection is refused, and a terminal already attached is dropped the next time
it types. Each command reads the file again as it runs, so a listing shows what
your file says at that moment. Turning it on reaches the sessions you start
after the change. A running session keeps the socket it already has until it
restarts. `koshi update` restarts every session it finds, and a restarted
session reads this key again and binds where your file says at that moment.

A session started with `koshi --headless --allow-other-users` keeps other users
for its whole life. That session never reads this key.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `allow-other-users` | boolean — let other users of this machine reach your sessions | `#false` | ≥ 0.3.0 |

## `shared-sessions-dir`

Where the session sockets other users reach are kept. Set it to a directory
every user who shares the machine can enter, such as `/var/run/koshi`. Leave it
out and koshi uses the machine-wide directory for the platform: `/tmp/koshi` on
Linux and macOS, `%ProgramData%\koshi` on Windows.

Every user who shares the machine has to name the same directory. A user whose
file names a different one looks in that one and finds nobody.

This only says where the sockets go. Nobody else reaches them until
`allow-other-users` is on.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `shared-sessions-dir` | string — directory the shared session sockets live in | `/tmp/koshi`, `%ProgramData%\koshi` on Windows | ≥ 0.3.0 |

## `remote-listen`

`remote-listen "0.0.0.0:7654"` names the address the remote listener binds, and
does nothing else: writing this line opens no port and makes this machine
reachable by nobody. The port opens the first time you run `koshi share grant`
and answer yes to the offer it makes, and on every start after that.

`allow-other-users` is a separate switch, about other users logged in to this
same machine. Neither key turns the other on.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `remote-listen` | string — host:port the remote TLS listener binds | unset — nothing binds | ≥ 0.3.0 |

## Full example

This shows every app setting. Fixed values match defaults. `default-shell`,
`remote-listen` and `shared-sessions-dir` are commented out — they have no
fixed default. `default-shell` comes from `$SHELL` or `%COMSPEC%`,
`remote-listen` is unset, and the shared sessions directory is `/tmp/koshi` on
Linux and macOS, `%ProgramData%\koshi` on Windows.

```kdl
// koshi.kdl — the complete default configuration.
version 1

theme "default"
allow-beta-features #false
allow-other-users #false
// remote-listen "0.0.0.0:7654"  // sets the address; opens no port on its own
// shared-sessions-dir "/var/run/koshi"  // optional override
auto-close-session #false

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
