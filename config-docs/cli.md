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
session. Two running sessions + the same command fails because koshi cannot
choose safely.

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
