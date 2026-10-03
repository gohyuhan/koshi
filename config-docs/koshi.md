# `koshi.kdl` — app settings

Main settings for theme, panes, scrollback, layout, mouse, terminal values,
logging, updates, beta features, session closing, and who else on this machine
may reach your sessions. `version` is required. Other settings are optional.

**Where it goes:** directly in the config directory — `~/.config/koshi/koshi.kdl`
on Linux, `~/Library/Application Support/koshi/koshi.kdl` on macOS,
`%APPDATA%\koshi\config\koshi.kdl` on Windows. See [README](README.md#where-the-files-go).

**Bad fields:** startup skips them, keeps their defaults, and logs each one.
`koshi config check` rejects them. `koshi config migrate` keeps them in the
migrated file. A bad value in `update` rejects the whole app file for that
launch.

Settings use blocks. `theme`, `image-support`, `reduced-motion`,
`stay-in-pane-placement-mode-after-placement`, `allow-beta-features`, `allow-other-users`,
`remote-listen`, `remote-reconnect`, `shared-sessions-dir` and
`auto-close-session` are top-level.

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
| `extended-keys` | `"on-request"` or `"always"` — what a pane program receives for a key whose legacy bytes another key also sends | `"on-request"` | ≥ 0.5.0 |

### `extended-keys`

`extended-keys` sets what a pane program receives for a key whose legacy
bytes another key also sends. The default is `"on-request"`.

```kdl
terminal {
    extended-keys "always"
}
```

#### Keys that share legacy bytes

With legacy key encoding, some keys send the same bytes as another key.
Shift+Enter and Enter both send `0x0d`, so the program cannot tell them apart.
Six bytes are shared. One key keeps each byte, and every other key that sends
it shares it:

| Byte | The key that keeps it | The keys that share it |
|---|---|---|
| `0x0d` | Enter | Shift+Enter, Ctrl+Enter, Ctrl+m |
| `0x09` | Tab | Ctrl+Tab, Ctrl+i |
| `0x1b` | Escape | Shift+Escape, Ctrl+Escape, Ctrl+[, Ctrl+3 |
| `0x7f` | Backspace | Shift+Backspace, Ctrl+8, Ctrl+? |
| `0x08` | Ctrl+h | Ctrl+Shift+h, Ctrl+Backspace |
| `0x00` | Ctrl+Space | Ctrl+Shift+Space, Ctrl+2, Ctrl+@ |

The Ctrl+Shift form of each Ctrl key in the right-hand column shares the byte
too, except Ctrl+Shift+Tab. A capital letter is the letter with Shift: Ctrl+M
is Ctrl+Shift+m.

Super changes no legacy byte. With Super held, every key in the table shares
its byte, the key that keeps it included: Super+Enter sends `0x0d` like Enter.

Alt puts `ESC` in front of the byte. With Alt held, the key that keeps a byte
keeps `ESC` and that byte: Alt+Enter keeps `ESC 0x0d`. Every other key that
shares the byte, and every key in the table with Super held, shares those two
bytes with Alt held: Alt+Shift+Enter, Alt+Ctrl+m and Alt+Super+Enter all send
`ESC 0x0d`.

The key that keeps a byte sends it under both values: Enter sends `0x0d`, Tab
`0x09`, Escape `0x1b`, Backspace `0x7f`, Ctrl+h `0x08` and Ctrl+Space `0x00`,
and Alt+Enter sends `ESC 0x0d`.

#### The `CSI u` form

The `CSI u` form names the key and the modifiers. Shift+Enter in this form is
`ESC [ 13 ; 2 u`: key 13 is Enter, and modifier 2 is Shift.

A program asks for this form when it writes `ESC [ > <number> u` to its
terminal. Koshi reads that request from the program's output. Each pane keeps
its own request, and the full-screen view of a pane keeps a request separate
from its normal view.

The number is a sum. Each part turns on one kind of detail:

| Number | What the program receives |
|---|---|
| 1 | an escape code for every key that produces no text, except Enter, Tab and Backspace: Escape and Ctrl+a arrive in the `CSI u` form |
| 2 | a report when a key repeats and when it is released |
| 4 | the shifted letter and the base-layout letter as well |
| 8 | an escape code for every key, text keys, Enter, Tab and Backspace included |
| 16 | the text the key produced |

Example: `ESC [ > 11 u` asks for 1, 2 and 8.

#### The two values

| Value | A program that asked | A program that did not ask |
|---|---|---|
| `"on-request"` (default) | the detail it asked for | legacy bytes for every key |
| `"always"` | the detail it asked for, and the `CSI u` form for every key that shares legacy bytes | legacy bytes, except the `CSI u` form for every key that shares legacy bytes |

Under `"on-request"`, a program that asked with `1` alone receives `0x0d` for
Shift+Enter. With `8`, it receives `ESC [ 13 ; 2 u`. Under `"always"`, it
receives `ESC [ 13 ; 2 u` with `1` alone.

`"always"` changes only the keys that share legacy bytes. Every other key sends
the same bytes under both values:

```
Tab           -> 0x09           under both values
Enter         -> 0x0d           under both values
typing "a"    -> a              under both values
Up arrow      -> ESC [ A        under both values
Shift+Tab     -> ESC [ Z        under both values
Ctrl+Right    -> ESC [ 1;5 C    under both values

Alt+Enter     -> ESC 0x0d       under both values

Shift+Enter   -> ESC [ 13;2 u   under "always", 0x0d under "on-request"
Ctrl+i        -> ESC [ 105;5 u  under "always", 0x09 under "on-request"
Alt+Ctrl+m    -> ESC [ 109;7 u  under "always", ESC 0x0d under "on-request"
```

Under `"always"`, a program that does not read the `CSI u` form receives the
keys that share legacy bytes as bytes it does not know. In bash 3.2 and zsh 5.9, typing `ab`,
Shift+Enter, `cd` gives the command line `ab3;2ucd`, and typing `ab`,
Shift+Backspace, `cd` gives `ab27;2ucd`.

#### Your own terminal

Koshi writes `ESC [ > 31 u` to the terminal it runs in, and receives each key
as that terminal reports it. A terminal that reports Shift+Enter as `0x0d`
gives koshi no Shift, so a pane receives Shift+Enter as Enter under both
values.

To check your terminal, run this in the terminal itself, not inside koshi,
then press Shift+Enter once within 2 seconds:

```sh
stty -icanon -icrnl -echo min 0 time 0; printf '\033[>8u'; sleep 2; dd bs=64 count=1 2>/dev/null | cat -v; echo; printf '\033[<u'; stty sane
```

The command prints every byte the terminal sent in those 2 seconds. A terminal
that reports Shift+Enter prints `^[[13;2u` among them, and it can also print a
report for the Shift key alone, such as `^[[57441;2u`. A terminal that does not
report Shift+Enter prints `^M`. The command restores your terminal before it
exits.

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

Self-update settings. Each installed koshi reads these from its own
`koshi.kdl`. `koshi update` installs a newer release the way that koshi was
installed: a Homebrew install runs `brew upgrade`, and a build from source
downloads nothing. A bad value here drops the whole `koshi.kdl` for that
launch.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `auto-check` | boolean — check GitHub for a newer koshi at startup; a build from source checks nothing | `#true` | ≥ 0.1.0 |
| `check-interval-days` | integer — days between checks | `14` | ≥ 0.1.0 |
| `allow-prerelease` | boolean — offer pre-release builds too; a Homebrew install takes stable releases only | `#false` | ≥ 0.1.0 |

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

**0.5.0-pr.1 marks no feature beta.** Every command in this release runs whether this
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

## `reduced-motion`

Controls placement interpolation for this viewer. With `#false`, the pane
placement preview slides to each new destination, and a pane that another viewer
places slides to its new place on this screen in 160 ms. `#true` shows the
selected destination and confirmation state without moving the preview through
intermediate rectangles, and draws another viewer's accepted placement at its
final place at once.

Example: another viewer swaps panes `A` and `B` in the tab you watch. With
`reduced-motion #false`, `A` and `B` slide past each other. With
`reduced-motion #true`, they change places in one frame.

| Setting | Meaning | Default | Since |
|---|---|---|---|
| `reduced-motion` | boolean — skip placement preview interpolation and the slide after another viewer's accepted placement | `#false` | ≥ 0.5.0 |

## `stay-in-pane-placement-mode-after-placement`

Controls what pane placement mode does after the session accepts a placement.
Each terminal reads this for itself.

- `#true`: pane placement mode stays on, so you can place the next pane. This
  holds after Enter and after a mouse drop. For example, `<C-p> m`, Right, Enter
  swaps the pane with its right neighbor, and the tab bar still shows
  `PLACE PANE`.
- `#false`: pane placement mode ends once the new layout arrives. The same keys
  swap the panes, and the tab bar returns to `BASE`.

A placement the session rejects, such as one built on a layout that changed
before it arrived, leaves pane placement mode as it was. Pane placement mode
opened with `<C-p> m` stays on, and a drag started on a pane's grab handle ends.

| Setting | Meaning | Default | Since |
|---|---|---|---|
| `stay-in-pane-placement-mode-after-placement` | boolean — keep pane placement mode on after a placement is accepted | `#true` | ≥ 0.5.0 |

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
restarts. `koshi restart-servers` restarts every session, and so does `koshi
update` on a build from source; `koshi update` on a release install restarts
every session that does not run the installed version. A restarted session
reads this key again and binds where your file says at that moment.

A session started with `koshi --headless --allow-other-users` keeps other users
for its whole life. That session never reads this key.

While this is on, your `koshi` lists the other users' sessions with limits:

- A session socket counts only when it is a socket, owned by the user who owns
  the folder holding it. A plain file or a link with a session's name is
  skipped.
- Every entry of the shared directory is read, up to 65,536 entries in all,
  counting the entries of each user's folder that is opened. An entry whose
  name is not a user id, such as `notes`, and a file in a user's folder whose
  name is not `session-<uuid>.sock`, are skipped without being opened.
- On Unix, at most 256 user folders are opened. A shared directory that holds
  one entry or one user folder past either limit is not read further. A
  listing shows the sessions read before the limit, names the directory on
  standard error and exits with code 4, for example `koshi: some sessions were
  not asked: /tmp/koshi could not be read: it holds more than 256 user
  folders`. A lookup by name is refused and names it the same way, and koshi
  attaches to none of those sessions by name until the whole directory can be
  read.
- At most 256 sessions of one user are listed, 256 in all on Windows. Each one
  past that is not asked: standard error names how many, and a listing counts
  them among the sessions that did not answer.
- A lookup by session id reads only that session's own path in each user's
  folder, within the same limits.
- A session id that two sockets advertise is reached through neither.
  `koshi attach <id>` is refused with `session <id> is advertised 2 times in
  the shared directory, by user ids 1001, 1002; koshi reaches none of them`,
  and a listing counts that session among the sessions that did not answer.
- A socket that another user names after one of your session ids is never
  taken for that session, also while that session restarts.
- `koshi list-sessions` and `koshi server-version` ask up to 16 sessions at
  the same time. `koshi list-sessions` stops waiting 5 seconds after it starts, and
  `koshi server-version` 5 seconds after it checked the router. A session that
  has not answered by then is named on standard error and left out of the
  listing.
- A user's folder that its owner closed to you, such as one at mode `0700`,
  advertises nothing. Its sessions are not listed and not counted.
- A read of the shared directory that fails another way, such as with
  `Input/output error`, is named on standard error, and a listing exits with
  code 4. A lookup is refused and names the path, for example ``cannot tell
  whether `quiet-lake` is unique (/tmp/koshi/1002 could not be read:
  Input/output error (os error 5))``.
- The router asks at most 16 of the other users' sessions at once, and a
  lookup by session id asks only that session. A session past that limit is
  not asked. A lookup by its id is refused with `session <id> is running but
  did not answer: 16 sessions other local users started are already being
  asked; run the command again`. A lookup by name counts it among the running
  sessions that did not answer.
- A lookup by name that matches one session another user started, while any
  other session did not answer, is refused with ``cannot tell whether
  `quiet-lake` is unique (1 running session did not answer)``. A session of
  yours with that name is used even then.

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
The value is an IP address and a port: IPv4 such as `192.168.1.20:7654`, or
IPv6 in brackets such as `[::1]:7654`. A host name, such as
`laptop.local:7654`, is ignored with a warning, and nothing binds. `0.0.0.0`
and `[::]` accept connections on every IPv4 or IPv6 address of this machine.
`koshi share grant` then names each of those addresses in its connect command,
as `koshi share grant` in `cli.md` shows.

`allow-other-users` is a separate switch, about other users logged in to this
same machine. Neither key turns the other on.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `remote-listen` | string — IP address and port the remote TLS listener binds, such as `0.0.0.0:7654` | unset — nothing binds | ≥ 0.3.0 |

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
reduced-motion #false
stay-in-pane-placement-mode-after-placement #true
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
