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

Settings use blocks. `theme`, `image-support`, `allow-beta-features`,
`allow-other-users`, `remote-listen`, `remote-reconnect`, `shared-sessions-dir`
and `auto-close-session` are top-level.

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
| `gap` | integer — blank cells between two panes that meet along a horizontal or vertical split; stacked panes stay contiguous | `0` | ≥ 0.4.0 |

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

Each terminal reads these for itself.

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
| `extended-keys` | `"on-request"` or `"always"` — whether keys like Shift+Enter reach programs that never ask for them | `"on-request"` | ≥ 0.5.0 |

### `extended-keys`

**Short version:** if Shift+Enter does nothing useful in a program you run inside
koshi, set this to `"always"`.

```kdl
terminal {
    extended-keys "always"
}
```

#### The problem

Press Shift+Enter in a terminal and the program you are running receives the byte
`0x0d`. Press plain Enter and it receives `0x0d`. The same byte. The program has
no way to know you held Shift.

This is not a koshi bug. Terminals have worked this way since the 1970s. Six
bytes carry more than one key, and these are the combinations people actually
press:

| What you press | What the program receives | The key that already owns those bytes |
|---|---|---|
| Shift+Enter | `0x0d` | Enter |
| Ctrl+Enter | `0x0d` | Enter |
| Ctrl+m | `0x0d` | Enter |
| Ctrl+i | `0x09` | Tab |
| Ctrl+[ | `0x1b` | Escape |
| Shift+Escape | `0x1b` | Escape |
| Ctrl+Backspace | `0x08` | Ctrl+h |
| Shift+Backspace | `0x7f` | Backspace |

The right-hand column is the key that keeps those bytes. Enter stays `0x0d`, Tab
stays `0x09`, Escape stays `0x1b`, Ctrl+h stays `0x08` and Backspace stays
`0x7f`. Only the key in the left-hand column is the one with no way to announce
itself.

A few rarer combinations collide too, for the same reason: Ctrl+Tab and
Ctrl+Escape (the modifier changes nothing), Ctrl+@ and Ctrl+2 (both `0x00`),
Ctrl+3 (`0x1b`), and Ctrl+8 and Ctrl+? (both `0x7f`).

So a chat-style program cannot use Shift+Enter for "new line" and Enter for
"send", because both keys arrive identically.

#### The fix, and why it is not automatic

There is a newer way to send keys that names the key and the modifiers instead
of squeezing them into one byte. Shift+Enter becomes `ESC [ 13 ; 2 u` — "key 13,
modifier 2", which reads as Enter plus Shift.

Koshi does not send that form to every program, because a program that does not
understand it would see garbage. So the rule is: a program gets the new form
after it asks for it.

**How a program asks.** A program does not only receive keys from its terminal.
It also writes back to it. That is how it switches to a full-screen view, turns
on mouse reporting, or asks for the new key form:

| The program writes | It is telling koshi |
|---|---|
| `ESC [ ? 1049 h` | switch to the full-screen view |
| `ESC [ ? 1003 h` | start sending me mouse events |
| `ESC [ ? 2004 h` | mark the text I paste |
| `ESC [ > 1 u` | send me keys in the new form |

The last line is the request. Koshi reads it off the program's own output, the
same output it paints on your screen. Koshi itself does exactly this to the
terminal it runs inside.

Neovim, Helix and Kakoune ask. Bash, Zsh and most command-line tools do not.

**A program asks for as much or as little as it wants.** The number in the
request is a sum, and each part switches on one kind of detail:

| Number | What the program is asking for |
|---|---|
| 1 | tell keys apart that otherwise share bytes — **except Enter, Tab and Backspace** |
| 2 | tell me when a key repeats and when it is released |
| 4 | tell me the shifted and base-layout letters as well |
| 8 | send every key in the new form, Enter, Tab and Backspace included |
| 16 | include the text the key produced |

A program adds up the parts it wants: `ESC [ > 1 u` asks for the first only,
`ESC [ > 11 u` asks for 1, 2 and 8 together.

**This is why asking is not always enough.** Number 1 deliberately leaves Enter,
Tab and Backspace alone, so that a shell still works if a crashed program left
the mode switched on. A program that asks with `1` alone still receives `0x0d`
for Shift+Enter — the exact problem it was trying to solve. Only number 8 covers
those three keys.

#### Why the setting exists

Some programs read the new form but never ask for it. Claude Code is one: it
understands `ESC [ 13 ; 2 u` perfectly, and it sends no request. Koshi cannot
tell such a program apart from `bash`, because the request is the only signal
there is — a program has no way to say "I understand it" other than asking.

That is what you are deciding with this setting.

| Value | What a program that asks gets | What a program that never asks gets |
|---|---|---|
| `"on-request"` (default) | exactly the detail it asked for | the old bytes — Shift+Enter arrives as Enter |
| `"always"` | the detail it asked for, plus the colliding keys above | the old bytes, except the colliding keys above |

`"always"` adds to a request, it never replaces one. A program that asked with
`1` alone keeps everything that answer gave it, and gains Shift+Enter and the
other seven.

#### What `"always"` changes, exactly

It changes the colliding keys, and nothing else. The rule is exact: a key changes
only when its old bytes are bytes another key also sends. Every other key is
byte-for-byte identical in both settings:

```
Tab           -> 0x09          unchanged
Enter         -> 0x0d          unchanged
typing "a"    -> a             unchanged
Up arrow      -> ESC [ A       unchanged
Shift+Tab     -> ESC [ Z       unchanged
Ctrl+Right    -> ESC [ 1;5 C   unchanged

Shift+Enter   -> ESC [ 13;2 u  was 0x0d
Ctrl+i        -> ESC [ 105;5 u was 0x09
```

#### The cost of `"always"`

A program that does not understand the new form stops understanding the colliding
keys:

- In `bash`, Shift+Enter today runs the command. With `"always"` it does
  nothing.
- In an editor that has not asked for the new form, Ctrl+[ stops acting as
  Escape and Ctrl+i stops acting as Tab.

That is the whole trade. Nothing else is affected.

#### Your own terminal has to support it too

Koshi can only pass on a key that your terminal reports in the first place. Ask
your terminal for the new form at startup, and if it does not support it, it
reports Shift+Enter as plain Enter and koshi never learns you held Shift. No
setting can recover that.

Terminals that support it include kitty, Ghostty, WezTerm, foot and Alacritty.
Apple Terminal does not.

To check yours, run this in the terminal itself — not inside koshi — press
Shift+Enter, then press Ctrl+C:

```sh
stty -icanon -echo min 1 time 0; printf '\033[>1u'; cat -v
```

`^[[13;2u` means your terminal supports it. `^M` means it does not. Then restore
your terminal:

```sh
printf '\033[<u'; stty sane
```

## `logging`

Neither side owns these: every koshi process reads them for its own log file.

Koshi writes `logs/koshi-log-<uuid>.log` below the state directory, one file
per session, named by the session's bare UUID. Disabled logging creates no log
file.

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

**0.4.0 marks no feature beta.** Every command in this release runs whether this
setting is on or off. `koshi`, `koshi attach` and `koshi --headless` were beta
before 0.2.0 and are on for everyone since. The setting stays for the features
that are marked beta next.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `allow-beta-features` | boolean — run features still marked beta | `#false` | ≥ 0.2.0 |

## `image-support`

Each terminal reads this for itself. On, the terminal probes for Kitty, iTerm2,
or Sixel output and paints decoded images with the selected protocol. Off, the
terminal keeps the text and image placeholders but sends no native image output.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `image-support` | boolean — send native image output to the terminal | `#true` | ≥ 0.4.0 |

## `remote-reconnect`

Each terminal reads this for itself. It applies only to a terminal viewing a
session on another machine, reached with `koshi attach --remote`.

On, a link that drops dials that machine again — after 1 second, then 2, 4, 8,
and 8 before every dial after that — for up to 120 seconds. While it waits, the
tab strip reads a tag shaped like `RECONNECTING (attempt 4, retry in 8s)`: the
first number is the dial it is about to make, and the second is the seconds left
before that dial. The seconds count down by one each second. The attempt number
rises by one on every dial, so the fourth dial and every dial after it waits the
full 8 seconds. Joining again puts back the tab you were on, the focused pane of
each tab, the fullscreened pane of each tab, and the scroll offset of each pane.
Keys typed while the link is down are dropped; a resize is kept.

A refusal no dial can change stops the dialing at once, without waiting the 120
seconds out: the certificate the server presents is not the pinned one, the
server does not admit the token, the token does not reach the session, or the
two builds share no protocol version. Every identical dial gets that same
answer, so no dial follows it.

When the dialing stops, koshi puts your terminal back the way it found it.
Then it prints the cause it stopped on, then `the session continues without
you`, then the command that lists the session on that server and the command
that joins it again — `koshi attach --remote <server> <session>`. It exits with
a non-zero status.

Off, a dropped link ends the terminal, printing how to attach again by hand.

A link to a session on this machine ends the terminal either way.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `remote-reconnect` | boolean — dial a session on another machine again when the link drops | `#true` | ≥ 0.3.0 |

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
version 2

theme "default"
allow-beta-features #false
allow-other-users #false
// remote-listen "0.0.0.0:7654"  // sets the address; opens no port on its own
// shared-sessions-dir "/var/run/koshi"  // optional override
auto-close-session #false
image-support #true
remote-reconnect #true

pane {
    min-cols 2
    min-rows 1
    gap 0
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
    extended-keys "on-request"
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
