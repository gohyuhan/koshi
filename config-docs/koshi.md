# `koshi.kdl` — app settings

Main settings for theme, panes, scrollback, layout, mouse, terminal values,
logging, updates, beta features, session closing, who else on this machine may
reach your sessions, and whether programs in panes may write your clipboard.
`version` is required. Other settings are optional.

**Where it goes:** directly in the config directory — `~/.config/koshi/koshi.kdl`
on Linux, `~/Library/Application Support/koshi/koshi.kdl` on macOS,
`%APPDATA%\koshi\config\koshi.kdl` on Windows. See [README](README.md#where-the-files-go).

**Bad fields:** startup skips them, keeps their defaults, and logs each one.
`koshi config check` rejects them. `koshi config migrate` keeps them in the
migrated file. A bad value in `update` rejects the whole app file for that
launch.

Settings use blocks. `theme`, `image-support`, `reduced-motion`,
`stay-in-pane-placement-mode-after-placement`, `allow-beta-features`, `allow-other-users`,
`remote-listen`, `remote-reconnect`, `remote-link-timeout-ms`,
`max-frames-per-second`, `shared-sessions-dir` and `auto-close-session` are
top-level.

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

A drag on the corner of a tiled pane moves each of the two borders that meet
there and have a pane beside them.

A program in a pane that asked for the mouse gets one release for each press
that koshi sent it, and no drag or release without a press. If one of these
comes before your release, koshi sends the release itself, at the cell your
pointer was over at the last press or drag koshi sent. A cell past the edge of
the program's screen goes to the nearest edge cell:

- pane placement mode opens;
- the pane leaves your view;
- your terminal's connection to the session ends: you detach, your terminal
  closes, the link drops, or the session restarts;
- you press the same button again;
- a press of another button reaches the program.

A left press ends a koshi drag that is still held. The highlight is not
copied, the border stays where it is, and a grab-handle drag sends no move.

Example: you drag in `vim`, and a key opens pane placement mode before you let
go. `vim` gets a left release at the cell of your last drag.

A floating pane takes the mouse on every cell it covers, the margin around
the tiled panes too. A wheel notch on any of those cells, the titlebar and the
border included, goes to that floating pane.

On a floating pane's titlebar row:

- Drag the grab handle `⣿⣿` to move the pane in your own view. The pane follows
  the pointer and stays inside the pane area. The move is sent when you
  release. A release where you pressed, a release that leaves the pane where
  it started, `Esc` during the drag, pane placement mode opening during the
  drag, or a focus loss that your terminal reports sends no move. `Esc`
  during the drag also drops a key sequence you started. A pane you pinned
  does not move. Until the session has moved the pane you released, a press
  on a grab handle starts no new move.
- `[ ]` pins the pane at its cell, and `[⚪]` unpins it. `–` minimizes the
  pane, and `✕` closes it.

A pane you minimized keeps its program running. `<C-p> <Tab>` steps through
the floating panes, and shows and focuses each pane it lands on
(`keybinding.md`, *Floating pane actions*). `koshi restore-pane --all` shows
every floating pane you minimized.

Drag a floating pane's border to resize it, up to the edge of your pane area
on that side. A drag on a corner moves the two borders that meet there. A pane
you pinned resizes from its right and bottom borders only. A pinned pane that
does not fit at its pinned cell is drawn left of or above that cell, against
the pane area edge. A drag on its right or bottom border changes its size by
the cells you drag. Until the pane fits at its pinned cell, that border stays
at the edge and the opposite edge moves. A press on the titlebar, the grab
handle or the border also focuses the pane and shows it above your other
floating panes; the three buttons do not.

Example: a `40x12` floating pane at column `30` of an `80`-column pane area,
its right border dragged 15 columns right, becomes `50` columns wide.

Example: the bottom-right corner of a `40x12` floating pane, dragged 3 columns
right and 2 rows down, makes it `43x14`. On a pinned pane, a drag on the
top-left corner resizes nothing, and a drag on the bottom-left corner moves
the bottom border only.

Example: a `40`-column pane pinned at column `20` of a `50`-column pane area is
drawn at column `10`. Its right border dragged 3 columns left makes it `37`
columns wide, drawn at column `13`.

## `copy`

Each terminal reads this for itself: the copy is made where the selection was
dragged.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `trim-trailing-whitespace` | boolean — drop trailing blanks from copied lines | `#true` | ≥ 0.1.0 |

## `pane-clipboard-writes`

A program in a pane can copy text to the clipboard in two ways:

- It writes `OSC 52`, the escape sequence that sets the clipboard of the
  terminal it runs in. Example: `printf '\033]52;c;aGk=\a'` copies `hi`.
- It runs a clipboard tool: `pbcopy`, `xclip`, `xsel` or `wl-copy`.

This block decides whether koshi passes those copies on to your terminal. The
session reads it: the session server takes it from the `koshi.kdl` it read when
the session started, and reads it again when the session restarts into a new
build, such as the one `koshi update` starts.

With `allow #false`, the default, an `OSC 52` copy reaches no clipboard, and a
clipboard tool runs as it does outside koshi.

With `allow #true`, an `OSC 52` copy goes to the terminal of every client whose
focused pane is the pane that wrote it. That terminal sets its own clipboard
when it reads `OSC 52`. The copy keeps the clipboards it names: `c` for the
clipboard, `p` for the primary selection, `q` for the secondary selection, `s`
for the selection, and `0` to `7` for the cut buffers. A copy that names none,
such as `ESC ] 52 ; ; aGk= BEL`, lets that terminal choose. The copy keeps the
decoded bytes unchanged, UTF-8 text or not: `ESC ] 52 ; c ; /w== BEL` copies
the one byte `0xff`. A copy longer than `max-copy-bytes` reaches no terminal.
An `OSC 52` request to read the clipboard gets no answer.

On Linux and macOS, `allow #true` also puts a folder of stand-in clipboard
tools first on the `PATH` of each pane started while it is on. The folder holds
one stand-in for each of `pbcopy`, `xclip`, `xsel` and `wl-copy` that the
session finds on its own `PATH` when it starts. A copy made through a stand-in
goes to the terminal of each client on another machine, a terminal that joined
with `koshi attach --remote`, whose focused pane is that pane. The real tool
runs too, with the same arguments and the same input, unless every client that
focuses the pane is on another machine and each one's link took the copy. A
link that dropped still takes copies until koshi notices the drop, as
[`remote-link-timeout-ms`](#remote-link-timeout-ms) describes, but its terminal
does not get them. Example: when the only client that focuses the pane is a
laptop whose network dropped, the laptop's link takes the copy, the real tool
does not run, and the copy reaches no clipboard.

Example: `printf hi | xclip -selection clipboard` in a pane that only a
terminal on another machine focuses puts `hi` on that terminal's clipboard, and
the real `xclip` does not run. In a pane that your terminal on the session's
machine focuses, the real `xclip` runs as it does outside koshi.

A terminal on another machine gets every copy as text. Example: `pbcopy` input
that begins with an RTF header, which the real `pbcopy` keeps as rich text,
arrives there as its RTF source.

The real tool runs alone, as it does outside koshi, for these calls:

- a read, a clear, or a copy of another type than plain text, such as
  `xclip -o`, `wl-copy --clear` or `xclip -target image/png`;
- a copy longer than `max-copy-bytes`, and a copy that is not UTF-8 text;
- a copy that the session does not take within 5 seconds. A copy goes to
  the session in parts of up to 12582774 bytes as the stand-in reads it, and
  the session has 5 seconds to take each part.

When the session took the whole copy and gives no answer within 5 seconds
more, the stand-in prints that failure and exits with code 1. The real tool
does not run, and the copy may still reach the terminal on the other machine.

The stand-in keeps every byte of input it read until the copy ends, at most
`max-copy-bytes` plus 12582775 bytes. When the real tool runs, it reads those
bytes, then the rest of the input.

No stand-in sees these copies:

- a tool started by its full path, such as `/usr/bin/xclip`;
- a tool started through `sudo`, or inside a container;
- a tool in a folder that a shell startup file puts in front of the stand-ins
  on `PATH`;
- a tool installed after the session started, until the session restarts into
  a new build;
- a copy in a pane started before `allow #true` took effect.

Example: on macOS, a login shell, such as `zsh -l`, reads `/etc/zprofile`, and
that file runs `path_helper`. `path_helper` puts the folders that `/etc/paths`
lists, `/usr/bin` among them, in front of the stand-ins, and `pbcopy` then runs
from `/usr/bin`. The shell that koshi starts in a new pane is not a login shell.

On Windows, no stand-in replaces a clipboard tool: `clip.exe` copies on the
session's machine only.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `allow` | boolean — pass the clipboard copies that programs in panes make on to your terminal | `#false` | ≥ 0.6.0 |
| `max-copy-bytes` | integer, at least `1` — the longest copy in decoded bytes | `8388608` (8 MiB) | ≥ 0.6.0 |

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

### Non-ASCII typing in a pane with no UTF-8 locale

Koshi writes the keys of one read of your terminal to the pane in one write,
in the order you typed them. Typing `é` writes the two bytes `C3 A9`.

Bash, and every other program that uses GNU readline, can read these bytes as
Meta keys. Readline does this when the pane's locale is `C` or `POSIX`, which
is the locale when `LC_ALL`, `LC_CTYPE` and `LANG` are all unset. Servers and
containers often have no locale set. In that locale, readline's `convert-meta`
setting is on: a byte of `0x80` or more that readline reads as the first byte
after its input queue is empty becomes `ESC` and the byte with its top bit
cleared. A byte of `0x80` or more that is already queued behind a typed
character goes into the line as it is.

Example: typing `echo é-ü` one key at a time gives this:

| Key | Bytes | Readline reads | Result |
|---|---|---|---|
| `é` | `C3 A9` | `M-C`, then `M-)` | `M-C` runs `capitalize-word`; `M-)` is unbound, and the `é` is lost |
| `-` | `2D` | `-` | `-` goes into the line |
| `ü` | `C3 BC` | `M-C`, then `M-<` | `M-<` puts the oldest history line on the command line |

When `echo é-ü` arrives in one write, such as from `koshi input`, every byte
after the `e` is queued, and the line reads `echo é-ü`. Typing one key at a
time gives readline the same bytes in every terminal and in tmux.

To type non-ASCII text in such a pane, do one of these:

1. Set a UTF-8 locale for the shell in the pane, such as
   `export LANG=C.UTF-8`. `locale -a` lists the locales the system has.
2. Add these lines to `~/.inputrc`, then start a new shell:

   ```
   set convert-meta off
   set output-meta on
   ```

   `convert-meta off` keeps every byte as text. `output-meta on` shows `é` on
   the command line in place of `\303\251`.

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
| `auto-check` | boolean — check GitHub for a newer koshi in the background while koshi runs, and offer a newer release found at the next start; a build from source checks nothing | `#true` | ≥ 0.1.0 |
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

**0.5.0 marks no feature beta.** Every command in this release runs whether this
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

A link drops when it breaks, or when no byte arrives over it for
[`remote-link-timeout-ms`](#remote-link-timeout-ms).

On, a link that drops dials that machine again — after 1 second, then 2, 4, 8,
and 8 before every dial after that — for up to 120 seconds. While it waits, the
tab strip reads a tag shaped like `RECONNECTING (attempt 4, retry in 8s)`: the
first number is the dial it is about to make, and the second is the seconds left
before that dial. The seconds count down by one each second. While a dial runs,
the tag reads `RECONNECTING (attempt 4, dialing)`. The attempt number rises by
one on every dial, so the fourth dial and every dial after it waits the full 8
seconds. Joining again puts back the tab you were on, the focused pane of
each tab, the fullscreened pane of each tab, and the scroll offset of each pane.
It also puts back your floating panes: which ones show, where each one sits,
which ones you pinned or minimized, and which one takes your keys. If that
floating pane closed while the link was down, your keys go to the shown floating
pane you focused most recently, else to the tab's focused pane.
Keys and pastes typed while the link is down are dropped. A resize is not sent
while the link is down; the link that comes back reports the size the terminal
has then.

The quit keybinding (`<leader>q` by default, in normal and locked mode) leaves
at once while the link is down, also while a dial runs. koshi then prints `left
session <session> while reconnecting`, then `the session continues without
you`, then the command that lists the session on that server and the command
that joins it again. It exits with status zero. With the link up, the quit
keybinding sends the quit to the session and leaves without waiting for the
session's answer.

A refusal no dial can change stops the dialing at once, without waiting the 120
seconds out: the certificate the server presents is not the expected one, the
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

## `remote-link-timeout-ms`

Each terminal reads this for itself. It applies only to a terminal viewing a
session on another machine, reached with `koshi attach --remote`. A terminal
viewing a session on this machine sends no ping and has no time limit.

The terminal sends the session a ping once every third of this time. The
session sends the terminal a pong whenever it has sent the terminal nothing
for a third of this time. A link over which no byte reaches the terminal for
this long has dropped: with `remote-reconnect` on, the terminal dials again,
and with it off, the terminal ends. A write from the terminal that puts no
byte on the link for this long ends the terminal's writing on that link.

The terminal gives the serving machine this value when it joins. That machine
closes its connection to the terminal once no byte arrives from the terminal
for this long, or the terminal takes no byte for this long. While the terminal
joins, that machine waits at least 20 seconds for the terminal's first request
to the session. The connection then leaves the `remote connections` count of
`koshi doctor`, and the session removes the terminal and keeps that terminal's
view. The terminal gets the view back when it joins again. That removal never
ends the session, also with `auto-close-session` on.

A paste keeps the link for as long as its bytes keep moving, also when it takes
longer than this time to send.

Example: with `remote-link-timeout-ms 30000`, the terminal sends a ping every
10 seconds. A laptop that loses its network shows `RECONNECTING` at most 30
seconds after the last byte arrived. The serving machine closes the laptop's
connection at most 30 seconds after the last byte from the laptop arrived. A
paste that takes two minutes to send over a slow link keeps the link.

The value is in milliseconds, from `1000` to `4294967295`. A value under `1000`
is ignored with the warning `must be at least 1000`. A value under one and a
half times the longest one-way delay of the link can drop a link that works.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `remote-link-timeout-ms` | integer, `1000` to `4294967295` — the milliseconds with no byte from the other side after which a link to a session on another machine has dropped | `30000` | ≥ 0.6.0 |

## `max-frames-per-second`

Each terminal reads this for itself. It is the most frames a second the session
sends this terminal. Two terminals that view one session can set different
values, and each one gets frames at its own rate.

The session sends the terminal a frame at most once per second divided by this
value, rounded up to the next nanosecond. A screen that changes faster than
that reaches the terminal as its newest state at each frame. The frame after
your own key, paste or mouse event goes at once, without that wait. The
pane placement preview and the pane slides that
[`reduced-motion`](#reduced-motion) describes step once per the same interval.

Example: with `max-frames-per-second 60`, `cat` of a large log in a pane sends
the terminal at most 60 frames a second, one at most every 16666667
nanoseconds. A key you type during the flood shows in the next frame, with no
wait.

The value is an integer from `30` to `120`. A value outside that range is
ignored with the warning `must be between 30 and 120`. A change applies at the
next `koshi attach`, and when the session moves this terminal to another
session. A link that drops and comes back keeps the value its attach read.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `max-frames-per-second` | integer, `30` to `120` — the most frames a second the session sends this terminal | `60` | ≥ 0.6.0 |

## `reduced-motion`

Controls placement interpolation for this viewer. With `#false`, the pane
placement preview slides to each new destination, a pane that another viewer
places slides to its new place on this screen in 160 ms, and a pane that this
viewer undocks slides from its tiled place to its floating place in 160 ms.
`#true` shows the selected destination and confirmation state without moving
the preview through intermediate rectangles, and draws another viewer's
accepted placement and an undocked pane at their final places at once.

Example: another viewer swaps panes `A` and `B` in the tab you watch. With
`reduced-motion #false`, `A` and `B` slide past each other. With
`reduced-motion #true`, they change places in one frame.

| Setting | Meaning | Default | Since |
|---|---|---|---|
| `reduced-motion` | boolean — skip placement preview interpolation, the slide after another viewer's accepted placement, and the slide of an undocked pane | `#false` | ≥ 0.5.0 |

## `stay-in-pane-placement-mode-after-placement`

Controls what pane placement mode opened with a key, such as `<C-p> m`, does
after the session accepts a placement. Each terminal reads this for itself.

- `#true`: pane placement mode stays on, so you can place the next pane. This
  holds after Enter and after a mouse drop. For example, `<C-p> m`, Right, Enter
  swaps the pane with its right neighbor, and the tab bar still shows
  `PLACE PANE`.
- `#false`: pane placement mode ends once the new layout arrives. The same keys
  swap the panes, and the tab bar returns to `BASE`.

A placement the session rejects, such as one built on a layout that changed
before it arrived, leaves that mode on.

The setting does not apply to a drag started on a pane's grab handle outside
pane placement mode. That drag ends with its drop, and the tab bar shows the
mode you were in before the drag, such as `BASE` or `LOCK`, whether the session
accepts or rejects the placement. For example, in normal mode, a drag of the
left pane's grab handle onto the right pane swaps the two panes, and the keys
you type next go to the focused pane.

The setting also covers floating pane move mode (`MOVE FLOAT`). `#true` keeps
the mode on after the session runs a floating pane move sent with Enter or a
mouse release. `#false` ends the mode once the session runs the move. A move
the session rejects leaves the mode on. For example, `<C-p> m`, `f`, Right,
Enter moves the floating pane one cell right, and with `#true` the tab bar
still shows `MOVE FLOAT`.

| Setting | Meaning | Default | Since |
|---|---|---|---|
| `stay-in-pane-placement-mode-after-placement` | boolean — keep pane placement mode opened with a key on after a placement is accepted, and floating pane move mode on after a move is run (≥ 0.6.0) | `#true` | ≥ 0.5.0 |

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
away has left, so a session it leaves empty ends. On Linux and macOS, a
`SIGHUP`, `SIGTERM`, `SIGINT` or `SIGQUIT`, such as one that `kill` sends, is a
leaving too, unless koshi ignores that signal, as `nohup` makes it ignore
`SIGHUP`. koshi restores the terminal, then ends by that signal. On Windows, a
Ctrl+C or Ctrl+Break signal is a leaving too, and koshi exits with
`0xC000013A` after it restores the terminal. Closing the terminal is a leaving
either way. On Linux and macOS, koshi ignores `SIGTSTP`, `SIGTTIN` and
`SIGTTOU` while it holds the terminal: a `kill -TSTP` does not stop it, and is
not a leaving.

A terminal on another machine whose link drops has not left, so the session
keeps running, also when that terminal was the last one. That terminal can
join again. A terminal that ends with no word to the session, such as one
killed with `kill -9`, counts as a dropped link when it is on another machine,
and as a leaving when it is on this machine. If koshi cannot set up a terminal
after the session accepts it, such as a terminal that cannot enter raw mode,
that terminal has not left, so the session keeps running.

A restart into a new build, such as the one `koshi update` starts, is not a
leaving. Each attached terminal reconnects by itself. Keys and pastes typed
while it reconnects go to the restarted session once the terminal joins it, in
the order they were typed. A terminal holds at most 1024 of them, and at most
one frame of pasted text; past either limit, everything typed during that
restart is dropped. The quit keybinding leaves at once. A terminal that does not
come back is dropped, for example a terminal that runs koshi 0.4.0, which
cannot attach to a newer session. A terminal on this machine that ends while
it joins the restarted session, before it has sent anything there, has not
left either. The session keeps running even when no terminal is left; rejoin
it with `koshi attach <session>`.

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

This works on macOS and Linux only. On Windows koshi reads the key without an
error and does nothing with it: only you reach your sessions, and your `koshi`
lists no session of another user.

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
- At most 256 user folders are opened. A shared directory that holds
  one entry or one user folder past either limit is not read further. A
  listing shows the sessions read before the limit, names the directory on
  standard error and exits with code 4, for example `koshi: some sessions were
  not asked: /tmp/koshi could not be read: it holds more than 256 user
  folders`. A lookup by name is refused and names it the same way, and koshi
  attaches to none of those sessions by name until the whole directory can be
  read.
- At most 256 sessions of one user are listed. Each one
  past that is not asked: standard error names how many, and a listing counts
  them among the sessions that did not answer.
- A lookup by session id reads only that session's own path in each user's
  folder, within the same limits. A lookup by `<user id>/<session id>`, such
  as `1001/session-3f2a…`, reads that path only in the folders that user owns,
  and reads no file of yours.
- A session id that two sockets advertise is reached through neither by its
  id alone. `koshi attach <id>` is refused with `session <id> is advertised 2
  times in the shared directory, by user ids 1001, 1002; koshi reaches none of
  them; name the owner to reach one, such as 1001/<id>`, and a listing counts
  that session among the sessions that did not answer. The example names the
  lowest user id whose folders advertise the id once. When the folders of every
  owner advertise it more than once, the sentence ends at `none of them`.
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
| `allow-other-users` | boolean — let other users of this machine reach your sessions; macOS and Linux only, no effect on Windows | `#false` | ≥ 0.3.0 |

## `shared-sessions-dir`

Where the session sockets other users reach are kept. Set it to a directory
every user who shares the machine can enter, such as `/var/run/koshi`. Leave it
out and koshi uses `/tmp/koshi`. On Windows koshi reads the key without an
error and does nothing with it.

Every user who shares the machine has to name the same directory. A user whose
file names a different one looks in that one and finds nobody.

This only says where the sockets go. Nobody else reaches them until
`allow-other-users` is on.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `shared-sessions-dir` | string — directory the shared session sockets live in; no effect on Windows | `/tmp/koshi` | ≥ 0.3.0 |

## `remote-listen`

`remote-listen "0.0.0.0:7654"` names the address the remote listener binds, and
does nothing else: writing this line opens no port and makes this machine
reachable by nobody. The port opens the first time you run `koshi share open`,
or answer yes to the offer `koshi share grant` makes, and on every start after
that.
The value is an IP address and a port: IPv4 such as `192.0.2.20:7654`, or
IPv6 in brackets such as `[::1]:7654`. A host name, such as
`laptop.local:7654`, is ignored with a warning, and nothing binds. `0.0.0.0`
and `[::]` accept connections on every IPv4 or IPv6 address of this machine.
`koshi share grant` and `koshi share open` then name each of those addresses
in their connect command, as `koshi share grant` in `cli.md` shows.

The router reads this line when it starts, and reads it again only when it
starts again. A line you add, change, or remove while the router runs applies
after `koshi restart-servers`, which restarts every session and the router with
their panes kept. Until then the router keeps the address it read, and a
listener that is open keeps serving on it. `koshi share grant`, `koshi share
open` and `koshi doctor` compare that address with the line in `koshi.kdl`, and
name the restart when the two differ:

```text
koshi: /home/user/.config/koshi/koshi.kdl names the remote listen address 127.0.0.1:7654, and the running router read no remote listen address from it when it started. Run `koshi restart-servers` so the router reads it again, then run `koshi share open`.
```

The router reads the `koshi.kdl` of the environment that started it. When it
reads another `koshi.kdl` than the `koshi` command you run, the message names
both files, and the line to change is the one in the router's file.

`allow-other-users` is a separate switch, about other users logged in to this
same machine. Neither key turns the other on.

| Key | Value / type | Default | Since |
|---|---|---|---|
| `remote-listen` | string — IP address and port the remote TLS listener binds, such as `0.0.0.0:7654` | unset — nothing binds | ≥ 0.3.0 |

## Full example

This shows every app setting. Fixed values match defaults. `default-shell`,
`remote-listen` and `shared-sessions-dir` are commented out — they have no
fixed default. `default-shell` comes from `$SHELL` or `%COMSPEC%`,
`remote-listen` is unset, and the shared sessions directory is `/tmp/koshi`.

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
remote-link-timeout-ms 30000
max-frames-per-second 60

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

pane-clipboard-writes {
    allow #false
    max-copy-bytes 8388608   // 8 MiB
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
