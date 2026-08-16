# koshi command line

This page lists commands that work now. Run `koshi <command> --help` for every
flag and accepted value.

## Starting koshi

| Command | Result |
|---|---|
| `koshi` | Open one session, tab, and shell pane |
| `koshi --profile <NAME>` | Open `profile/<NAME>.kdl` |
| `koshi --headless` | Open a session with no terminal attached, print its id, and return to the shell |
| `koshi --headless --allow-other-users` | Open that session so the other users of this machine may reach it |
| `koshi update` | Check for and install the latest release |

`koshi update` then restarts each running session that can into the new release,
and after them the background process that tracks sessions. A session keeps its
panes, the programs running in them and their scrollback, and an attached
terminal rejoins the session on its own.

`koshi update` names every session that did not move on standard error, and that
session keeps the old build until you end it and start it again. A session
refuses the restart when a pane's program stopped reading its input, when a pane
has no terminal to carry, when this machine cannot run the new binary, or when
the new binary does not read the resume file this build writes. A session
running a koshi with no restart at all, one that still reports the old version
after the restart, and one that answers nothing within ten seconds are all
reported the same way.

`--headless` prints `[SESSION ID]: session-<uuid>` and exits. Nothing is drawn.
Attach to it later with `koshi attach session-<uuid>`.

`--allow-other-users` goes only with `--headless`. The session it starts serves
the other users of this machine for its whole life, whatever `koshi.kdl` says.

## Configuration

| Command | Result |
|---|---|
| `koshi config path` | Print the config directory for this platform |
| `koshi config explain <KEY>` | Show one file-qualified key's file, default, and meaning |
| `koshi config check` | Validate every present config file without changing it |
| `koshi config migrate` | Validate all files, then move old schemas to the newest supported version |

Explain keys include their file kind: `koshi.pane.min-cols`,
`keybinding.chord-timeout-ms`, `theme.colors.accent`, and `profile.version`.
An unknown key exits 2 and suggests the nearest known key.

`check` and `migrate` scan `koshi.kdl`, `keybinding.kdl`, `themes/*.kdl`, and
`profile/*.kdl`. Migration does not repair bad KDL or bad fields. Current
schema version is `1`, so valid version `1` files stay unchanged.

Each path must be a regular file or a symlink to one. Both commands report all
read and schema errors before migration writes anything. Migration keeps the
symlink and updates its target.

Migration replaces files one at a time. If a write fails, the error lists files
already migrated and says the failing file may also contain migrated data.

## Choosing a target

Inside a koshi pane, an omitted target means that pane's session and current
view. Outside koshi, explicit `--session`, `--tab`, `--pane`, or `--client`
flags choose the owner. With no explicit target, exactly one running session
may be used; zero or several sessions fail.

Example: one running session + `koshi new-tab` results in a tab in that
session. Two running sessions + the same command fails with `several sessions
are running; name one with --session <name-or-id>`.

Session and tab flags that say `NAME_OR_ID` accept either their generated name
or printed id. A value that reads as an id is always the id — it never falls
back to a name lookup. A name several targets share is refused, and the error
lists every matching id. Pane and client flags use printed ids.

## Created ids

Create commands print ids on stdout in creation order:

```text
koshi new-pane
[PANE ID]: pane-<uuid>

koshi new-tab
[TAB ID]: tab-<uuid>
[PANE ID]: pane-<uuid>

koshi --headless
[SESSION ID]: session-<uuid>
```

`koshi run -- htop` prints one pane id. Commands that create nothing print no
id line.

## Sessions and discovery

| Command | Result |
|---|---|
| `koshi list-sessions` | List session ids and names |
| `koshi attach [NAME_OR_ID]` | Attach this terminal to that session |
| `koshi detach [CLIENT_OR_SESSION]` | Detach one terminal; the session keeps running |
| `koshi detach --all [NAME_OR_ID]` | Detach every terminal of that session |
| `koshi kill-session [NAME_OR_ID]` | End that session, or the only running one |
| `koshi list-tabs [--session <NAME_OR_ID>]` | List tab ids, names, and owning sessions |
| `koshi list-panes [--session <NAME_OR_ID>]` | List pane, tab, and session ids and names |
| `koshi list-clients [--session <NAME_OR_ID>]` | List client ids and owning sessions |
| `koshi inspect session <NAME_OR_ID>` | Show one session's full record |
| `koshi inspect tab <NAME_OR_ID>` | Show one tab's full record |
| `koshi inspect pane <PANE_ID>` | Show one pane's full record |
| `koshi inspect client <CLIENT_ID>` | Show one client's full record |

Every list and inspect command accepts `--format table` or `--format json`.
Table is the default.

`kill-session` takes the session id or its exact generated name; an id goes
straight to that session with no lookup. With no argument, it works only when
exactly one session is running. An unknown name or id exits 3; an unreachable
control socket exits 4.

`attach` run outside koshi opens that session in this terminal. Run inside a
koshi pane, it moves this terminal to the named session instead. With no
argument and several sessions running, it numbers them and reads your answer:

```text
koshi attach
1) amber-fox session-3f2a…
2) quiet-heron session-91c4…
attach to which session? [1-2]
```

`detach` leaves the session running with its panes untouched. Bare `koshi
detach` works only inside a koshi pane and detaches that terminal. Outside one,
name the target: `koshi detach session-3f2a…` takes a client id, a session id,
or a session name. `--all` detaches every terminal of one session.

A session left with no terminal keeps running unless `auto-close-session` is on
in `koshi.kdl`, which ends it once the last terminal leaves.

## Panes

| Command | Main flags | Result |
|---|---|---|
| `koshi new-pane` | `--direction`, `--stacked`, `--pane`, `--tab`, `--session`, `--client` | Open a shell pane |
| `koshi run -- <COMMAND>...` | Same placement flags as `new-pane` | Open a pane running the command |
| `koshi close-pane` | `--pane`, `--force` | Close a pane |
| `koshi resize-pane` | `--direction`, `--size`, `--pane` | Move one border by signed cell count |
| `koshi focus-pane` | `--pane`, `--client` | Focus a pane |
| `koshi toggle-pane-fullscreen` | None | Toggle the focused pane's fullscreen view |
| `koshi input "<TEXT>"` | `--pane`, `--no-enter` | Type text; Enter follows unless held back |

Directions: `right`, `down`, `left`, `up`. A positive resize grows toward the
direction; a negative resize shrinks from that side.

Example: `koshi input --pane pane-… --no-enter "git status"` leaves
`git status` at that pane's prompt without running it.

## Tabs

| Command | Main flags | Result |
|---|---|---|
| `koshi new-tab` | `--session <NAME_OR_ID>` | Open a tab with one shell pane |
| `koshi close-tab` | `--tab <NAME_OR_ID>`, `--session <NAME_OR_ID>`, `--force` | Close a tab |
| `koshi next-tab` | `--client` | Focus the next tab |
| `koshi previous-tab` | `--client` | Focus the previous tab |
| `koshi focus-tab` | `--index` or `--tab <NAME_OR_ID>`, optional `--client` | Focus one tab |
| `koshi move-tab` | `--index`, optional `--tab <NAME_OR_ID>` | Move one tab to a zero-based index |

## Input lock

| Command | Result |
|---|---|
| `koshi lock [--client <CLIENT_ID>]` | Send keys straight to the pane |
| `koshi unlock [--client <CLIENT_ID>]` | Restore koshi shortcuts |
| `koshi toggle-lock [--client <CLIENT_ID>]` | Toggle locked input |

## Actions and shortcuts

| Command | Result |
|---|---|
| `koshi actions list [--format table\|json]` | List supported actions |
| `koshi actions explain <ACTION> [--format table\|json]` | Explain one action |
| `koshi keys list [--mode <MODE>] [--scope default\|user]` | List effective shortcuts |
| `koshi keys describe "<KEY_SEQUENCE>"` | Explain one shortcut |
| `koshi keys conflicts` | Report clashes, dead shortcuts, and warnings |
| `koshi keys validate <PATH>` | Check a shortcut file without applying it |

## Remote access

| Command | Result |
|---|---|
| `koshi share grant <IDENTITY> [--session <SESSION>] [--expires <DURATION>\|never]` | Grant an identity a remote access token |
| `koshi share revoke <IDENTITY> [--session <SESSION>]` | Revoke the tokens an identity holds |
| `koshi share list [--session <SESSION>] [--format table\|json]` | List the tokens granted on this machine |
| `koshi attach --remote <SERVER> [--save-as <NAME>] [SESSION]` | Attach to a session on the machine `SERVER` names |
| `koshi remote list [--format table\|json]` | List the servers this machine has saved |
| `koshi remote forget <SERVER>` | Drop one saved server |
| `koshi remote set-secret <SERVER>` | Replace the secret of one saved server |

The first three run on the machine holding the sessions. The last four run on
the machine connecting to it.

An absent `--session` reads one way on `grant` and another way on `revoke`.
`koshi share grant alice` gives alice one token that reaches every session on
this machine. `koshi share revoke alice` stops every grant alice holds: the one
that reaches every session, and each one that reaches a single session.

`koshi share list --session quiet-lake` lists every grant that reaches the
session `quiet-lake` — the grants scoped to that session, and the grants that
reach every session on this machine, whose `scope` column reads `host`. It
answers "who can get into this session". `koshi share list` with no `--session`
lists every grant this machine has made.

An identity holds at most one grant per scope. Granting the same identity on
the same scope again hands out a fresh token and takes the place of the old
one, so a second `koshi share grant alice` leaves alice with exactly one
host-wide token — the new one. When the grant it replaced was still standing,
the output says so before printing the new token:

```text
the token alice already held on host stopped working.
```

That line is absent when the grant it replaced had already been revoked or had
already expired.

`--session` takes a session id or a display name. A name that matches two
running sessions is refused, and the error lists every matching id.

`--expires` defaults to `24h`. It takes a count and one unit letter — `30s`,
`15m`, `24h`, `7d` — or the word `never`. The count is read as written, so
`+1h` and `007h` are both taken as the number they spell.

A count of `0` is taken as written too: the grant runs out at the instant it is
made, so `koshi share grant alice --expires 0s` prints a token that admits
nothing. Revoke it or grant again to hand alice one that works.

A length koshi cannot represent is refused, and no token is granted:

```text
koshi share grant alice --expires 18446744073709551615d
```

is refused by the command: the count times its unit does not fit the length
koshi carries.

```text
koshi share grant alice --expires 10000000000000000000s
```

is refused by the router: the expiry lands further ahead than this machine's
clock can represent.

A grant prints its token once, so copy it from that one printing. Anyone
holding the token can run anything the granting user can.

A listen address in `koshi.kdl` sets the address; it does not open the port.
With an address set and remote access still off, `koshi share grant` says so
and offers to switch it on:

```text
remote access is off.
turn it on and open 0.0.0.0:7654? [y/N]
```

A typed `y` opens the port, and it opens again on every start after that. Any
other answer leaves it shut and still prints the token. With no address in
`koshi.kdl` there is nothing to offer, and the grant says the token cannot be
used to connect yet.

`koshi share revoke alice` ends the connections alice's tokens opened, at once.
Her connection stops and no further frame reaches her; her next command is not
merely refused. Granting alice again on the same scope ends the old
connection the same way, since the replaced token stopped working. A token that
runs out on its own is different: it stops a new connection from opening and
never interrupts one already attached.

The `koshi share list` columns read:

| Column | Meaning |
|---|---|
| `identity` | Who the grant was handed to |
| `scope` | `host` when the grant reaches every session on this machine, else the id of the one session it reaches |
| `issued` | When the grant was made |
| `expires` | When the grant stops working on its own |
| `last_used` | When a presented token last reached a session through this grant |
| `revoked` | When an operator stopped the grant |

In table cells a time prints as whole seconds since the Unix epoch, and an
absent value prints as `-`.

### Connecting to another machine

`--remote` names the machine an invocation talks to, by the `host:port` it
listens on or the name it was saved under. Everything after that — how a
session is named, how a missing name is resolved, what is refused — runs
against that machine unchanged.

The first connection to a server names its address, and `--save-as` gives it a
short name:

```text
koshi attach --remote laptop.local:7654 --save-as work web
```

After that the name stands in for the address, and nothing is retyped:

```text
koshi attach --remote work web
```

The secret never appears on a command line. koshi reads it from the
environment variable `KOSHI_REMOTE_SECRET`, and with that unset asks for it at
the terminal without printing what is typed. Every argument after the program
name is readable by other users of the machine, so no flag takes a secret.

On the first connection koshi records the fingerprint of the certificate the
server presented — the sha256 of it, as 64 lowercase hex characters — and
pins it. A later connection presenting a different certificate is always
refused, and the refusal names the address and both fingerprints. When the
server really was reinstalled, run `koshi remote forget <SERVER>` and connect
again to pin the new one.

A first connection saves the address, the secret, the pinned fingerprint, and
the name given by `--save-as`. The store lives on the connecting machine and is
readable only by its owner. `koshi remote list` prints the name, address,
fingerprint and last-used time of each saved server, and never a secret. Once
the serving machine grants a fresh secret, `koshi remote set-secret <SERVER>`
replaces the saved one; it reads the new secret the same way a connection does.

Bare `koshi attach` lists the sessions on every reachable saved server beside
this machine's own, each row naming the server it belongs to. The remote check
waits two seconds in total, not two seconds per server, so one unreachable
machine cannot slow the list. A server not heard from inside that wait is left
out. A server that answers and refuses the saved secret is not hidden — it
prints the command that replaces that secret:

```text
work: the saved secret was refused; run `koshi remote set-secret work`
```

`--remote` never creates a session, and it takes `attach` or an action verb.
Bare `koshi --remote work` names nothing to run, and `koshi share --remote
work` is not carried to the other machine; both are refused:

```text
--remote needs a command, such as `koshi attach --remote <server>`
```

Tokens are granted only from the machine holding the sessions.

## Versions

| Command | Result |
|---|---|
| `koshi version [--format table\|json]` | Print the build of the koshi program you just ran |
| `koshi server-version [--session <NAME_OR_ID>] [--format table\|json]` | Print the build each running koshi server runs |

`koshi version` prints the same line as `koshi --version`.

```text
koshi version
koshi 0.2.0
```

These two answers differ while an update rolls out. `koshi update` installs a
new binary, the router restarts into it, and each session server replaces its
own image one at a time. Until every swap lands, the program your shell runs is
a newer build than the process answering it:

```text
koshi server-version
kind     session                                       version
router   -                                             0.2.0
session  session-3f2a1c94-8e7b-4d15-9a02-6c5138ef7b40  0.2.0
session  session-91c4de07-2b53-41a8-bf6e-70d9a2c81f35  0.1.0
```

The version column reads:

| Cell | Meaning |
|---|---|
| a build, like `0.2.0` | The server answered and named it |
| `unknown` | The server answered and is too old to name its build |
| `not running` | Nothing is listening there |
| `unreachable` | The server could not be asked; the reason prints on standard error |

A server that could not be asked does not sink the rest of the answer: the
other rows still print, and the command exits 4 so a script reading only the
rows never takes a partial answer for the whole picture. Everything answering
exits 0, including a machine running nothing at all.

`--session` reports that one session and leaves out the router. It takes the
session id or its exact generated name. A name must match exactly one running
session.

## Checking the installation

| Command | Result |
|---|---|
| `koshi doctor [--format table\|json]` | Check this machine's koshi installation |

```text
koshi doctor
check               verdict  reason                                                                                help
config              ok       3 config files validated                                                              -
shell               ok       a new pane runs /bin/zsh                                                              -
terminal            warn     TERM is not set                                                                       set TERM before running koshi, for example TERM=xterm-256color
runtime directory   ok       /run/user/1000/koshi is ready                                                         -
log directory       ok       /home/you/.local/state/koshi/logs is writable and logging is off                      -
plugins directory   ok       /home/you/.config/koshi/plugins is readable                                           -
router              ok       no koshi is running                                                                   -
session directory   ok       sessions are advertised in /run/user/1000/koshi (mode 700), which only you may reach  -
remote access       ok       koshi.kdl names no remote listen address, and this machine holds 0 standing grants    -
remote connections  ok       no koshi is running, so nothing from another machine is connected                     -
```

The verdict column reads:

| Cell | Meaning |
|---|---|
| `ok` | The check found what it looks for |
| `warn` | The check found something that still works and is worth reading |
| `fail` | The check found something koshi cannot work through |

The whole answer prints either way: a run holding a `fail` row exits 1, and a
run of only `ok` and `warn` rows exits 0.

The checks run in this order:

| Check | What it reads |
|---|---|
| `config` | Every config file in the config directory, validated the way `koshi config check` validates it |
| `shell` | `koshi.kdl`'s `terminal.default-shell`, else `SHELL` on Linux and macOS and `COMSPEC` on Windows, and whether the program it names exists |
| `terminal` | `TERM` and `COLORTERM` |
| `runtime directory` | The runtime directory: that it can be read, and that it is private |
| `log directory` | The log directory: that a file can be written there, and whether `koshi.kdl` turns logging on |
| `plugins directory` | The plugins directory: that it exists and can be read |
| `router` | Whether a router answers on its control socket |
| `session directory` | Where sessions are advertised, and who may reach that directory |
| `remote access` | `koshi.kdl`'s remote listen address, and how many access grants still stand |
| `remote connections` | How many open connections the running router holds from another machine |

The last three rows report facts and rate nothing. The `plugins directory` row
reads the directory and opens no plugin. `koshi doctor` starts no koshi and
creates no directory. The `log directory` row writes one empty file in the log
directory and removes it again, which is how it reports whether that directory
can be written.

The `router` row is the only row that rates the running router. A router whose
build has no such question is `warn`; a router that is listening and does not
answer is `fail`. Either way the `remote connections` row reads `the running
router did not answer, so this is not known`.

A router that answers but whose build reports no count reads `the running
router reports no count, so this is not known`. A count of `0` prints only
when the router sent one.

A row whose `reason` is shortened to fit the table carries the whole text in a
`detail` field, which `--format json` prints and the table leaves out. Every
other row has `"detail": null`.

## Debugging

| Command | Result |
|---|---|
| `koshi debug dump-state [--format table\|json]` | Print every running session's sessions, tabs, panes, and clients |
| `koshi debug dump-layout [--tab <NAME_OR_ID>] [--format table\|json]` | Print each tab's split tree, solved rectangles, panes with no room, stacks, and per-client focus |

A pane's command arguments print as `***`; the program name stays visible.
`koshi inspect pane` shows the command in full.

Example: a pane running `mysql -pHUNTER2` prints as `mysql ***`.

Every client viewing one tab shares one set of sizes: the tab solves against
the smallest viewing terminal on each axis, minus the top tab bar row and the
bottom hint row. Two clients on one tab, one 80x24 and one 120x40, both print
`viewport 80x22`. What is per client is the view: one client tiled and one with
a pane fullscreen give that tab two sets of rectangles. A tab no client is
viewing prints its tree and no rectangles.

A session that started before you installed this Koshi cannot report its
layout. `dump-layout` says so and names what to do: restart that session, or
run `dump-state`, which every session answers.
