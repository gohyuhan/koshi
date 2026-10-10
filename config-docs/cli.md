# koshi command line

This page lists commands that work now. Run `koshi <command> --help` for every
flag and accepted value.

A command that asks a question, such as one that ends with `[y/N]`, prints the
question and the lines that explain it on standard error, then reads the answer
from standard input.

## Starting koshi

| Command | Result |
|---|---|
| `koshi` | Open one session, tab, and shell pane |
| `koshi --profile <NAME>` | Open `profile/<NAME>.kdl` |
| `koshi --headless` | Open a session with no terminal attached, print its id, and return to the shell |
| `koshi --headless --allow-other-users` | Open that session so the other users of this machine may reach it |
| `koshi update` | Check for and install the latest release |
| `koshi restart-servers` | Restart the router and every running session into the koshi program on disk; every pane keeps running |

`koshi update` installs the release the way koshi was installed. It reads the
file that the path of the running koshi names, with every symbolic link
followed:

1. If the file sits in a Homebrew keg, such as
   `/opt/homebrew/Cellar/koshi/0.6.0/bin/koshi`, `koshi update` runs
   `brew upgrade gohyuhan/koshi/koshi` with the `brew` of that Homebrew. It
   asks GitHub nothing first: `brew` decides whether the formula has a newer
   version. A koshi from a formula pinned to one version, such as
   `koshi@0.5.0`, is not upgraded. The command names `brew install
   gohyuhan/koshi/koshi`, which moves to the newest koshi.
2. If the file sits in a Scoop app folder, such as
   `C:\Users\user\scoop\apps\koshi\0.5.0\koshi.exe`, `koshi update`
   downloads the newest release archive, checks its SHA-256 checksum, and
   replaces the file in place. Scoop still lists the version it installed.
   `scoop update koshi` skips koshi while any koshi from that install runs.
3. Every other release binary, such as one that `install.sh` or `install.ps1`
   placed, is replaced in place as for Scoop. If the path is a symbolic link,
   the file it names is replaced, and the link stays.
4. A koshi built from source downloads nothing. A build is a release build only
   when the release workflow built it.

When `koshi update` replaces the file in place, it first writes the new release
beside it: `<name>.koshi-update-<process id>` on Linux and macOS, such as
`koshi.koshi-update-5000`, and `koshi-staged-<process id>.exe` on Windows. Then
it renames that copy into place. `install.sh` and `install.ps1` name their
copies the same way. An update or install that is killed between the two steps
leaves its copy. The next `koshi update` deletes each such copy first. On
Windows, it also deletes each `koshi-update-<process id>.exe` copy that
`koshi update` of koshi 0.5.0 left. A copy stays while a process with the
process id in its name runs. `koshi update` of koshi 0.5.0 or older deletes no
such copy.

Before the rename, `koshi update` runs that copy with `--version`. If it does
not print `koshi <release version>` within 70 seconds, the update stops with
`koshi: update failed: ...`. The copy is deleted, and the installed koshi stays
as it was. What the new release writes on standard error shows above that line.
Example: a release that needs a newer glibc than the system has does not start,
and the update stops before it replaces anything.

If only root can write the folder, such as a root-owned `/usr/local/bin`,
`koshi update` writes the copy as root, in one `sudo` command. That command
also deletes the copies that ended updates left in the folder. Then
`koshi update` runs the check as the user, and renames the copy as root, in a
second `sudo` command. If `sudo` keeps no credentials between commands, it asks
for the password at each command. If the check or the rename fails, also when
`sudo` refuses the second command, `koshi update` deletes a copy that is still
there with `sudo rm -f`, which can ask for the password once more. If the copy
stays, the error names it and the `sudo rm -f` command that deletes it.

`install.sh` installs into `/usr/local/bin` the same way: it writes
`koshi.koshi-update-<process id of its shell>`, runs it with `--version`, then
renames that copy to `koshi`. It reads the version from the first line on
standard output, and what the copy writes on standard error shows on the
terminal. If only root can write the folder, `install.sh` runs the copy, the
mode change, and the rename through `sudo`, and the check as the user. If the
copy does not print `koshi <release version>`, or a step fails, `install.sh`
deletes its copy, and the installed `koshi` stays as it was. If Ctrl+C stops
the script, `install.sh` deletes its copy too, and the installed `koshi` is the
old release or the complete new one.

On Windows, the replacement renames the running `koshi.exe` to `koshi.old`, or
to `koshi.1.old`, `koshi.2.old` and onward while a koshi still runs from an
older backup. `install.ps1` does the same: it first moves the new `koshi.exe` to
`koshi-staged-<process id>.exe` beside the installed one, runs it with
`--version`, and stops when it does not print `koshi <release version>`. Each
interactive launch removes the backups that no koshi runs from. `koshi update`
of koshi 0.5.0 or older stops with `Access is denied` while a koshi still runs
from `koshi.old`. Rename that file to the next free backup name, such as
`koshi.1.old`, and run the update again.

`koshi update` and `install.ps1` hold a lock on the file `koshi.lock` beside
`koshi.exe` while they replace it. If another install holds that lock, each one
waits until it is free, and `koshi update` prints `koshi: waiting while another
koshi install holds <path>`. An interactive launch removes backups only while it
holds that lock. If an install holds the lock, the launch leaves the backups in
place. `koshi update` of koshi 0.5.0 or older, and a launch of one, take no
lock.

After every update, whether or not it installed a release, `koshi update`
restarts each running session, then the background process that tracks sessions.
It skips a server whose program file says it already runs the version that
`<program> --version` now prints, and prints `<session id> already runs koshi
0.6.0; its panes keep running`. It skips a session that runs from another
program file, and names it, such as `koshi: <session id> runs koshi 0.5.0 from
/opt/koshi/koshi, a program file this koshi does not replace; it keeps running
koshi 0.5.0`. A koshi built from source restarts every running server into the
koshi at its path. Each one has 20 seconds to answer the restart request and 20
seconds to come back on the new release. One that does not is reported as not
confirmed, and the update goes on. A session keeps its panes, the programs
running in them and their scrollback. A client from the installed build can
reattach to that session. The installed build migrates `version 1` KDL files to
`version 2` when a running session asks it which resume formats it reads, before
that session restarts. A session server or router migrates them again when it
starts. A file with an unknown key or a bad value migrates and keeps that
setting in its text. If a file cannot be read, holds KDL that does not parse, or
declares an unusable version, or the lock file `.migration.lock` in the config
directory cannot be opened or locked, migration writes no file and prints the
error on standard error. A session server also writes the error to its log when
`koshi.kdl` turns logging on. The session still restarts and the router still
starts, each with the files as they are, and the next start migrates them again.
`koshi config check` reports each file problem. It does not report a lock
problem. A file that does not parse applies no settings, as before the update.

Live session handoff is available for sessions started by koshi 0.3.0, 0.4.0,
0.5.0-pr.1, 0.5.0-pr.2, or 0.5.0. A session started by koshi 0.2.0 or one of
its pre-releases has no restart handoff, and an update that koshi 0.2.0 runs
leaves that session on koshi 0.2.0. `koshi update` and `koshi restart-servers`
from the installed build name such a session and ask whether to end it, as the
`koshi restart-servers` paragraphs below state. A koshi 0.1.0 window serves its
session from its own terminal. It keeps its build until that terminal closes.

While the router restarts, it first finishes the lookups and session starts it
already holds. A new `koshi attach` or a new session start that reaches it in
that time waits for the new router: up to 30 seconds, and longer while `koshi
update` holds its lock file `update.lock`. Then the command asks again. If the
router has not come back by then, the command fails with `the router is
restarting into a new build; run the command again`.

A client attached while the update runs comes back to its session by itself. If
the restarted session does not speak that client's protocol version, the client
waits for the router to restart too: up to 30 seconds, and longer while
`koshi update` holds the lock file `update.lock` in the runtime directory as it
restarts servers. Then it runs
`koshi attach <session id>` in the same terminal, with the koshi at the path the
client was started from. A client from koshi 0.5.0-pr.1 or earlier prints the
attach command instead. A client of a session on another machine prints the
refusal, then the attach command. If the restarted session refuses a client for
another reason, the client prints that refusal, then the attach command, such as
`the session restarted and refused this client: IPC unavailable: <reason>`. For
a session named `workspace`, start the installed build and run
`koshi attach workspace`.

An update run by koshi 0.4.0 or 0.5.0-pr.1 cannot confirm the restarts.
Koshi 0.4.0 cannot read the session and router files that the installed build
writes, and 0.5.0-pr.1 does not speak the installed build's protocol versions.
For each session and for the router, it prints that the restart was not
confirmed, or that it still reports its own version. Those sessions and the
router did restart into the installed build, and their panes keep running.
`koshi server-version` prints the build each one runs.

The router converts the remote listener's certificate, remote access record, and grants.
Koshi converts saved servers on their first read. For example, a saved server
keeps its secret and certificate pin, and an existing grant keeps its scope and
expiry.

A session whose saved state is partly damaged still comes back. A pane whose
screen could not be read comes back blank with a notice, and its program keeps
running. When the layout could not be read, each program comes back in a tab of
its own. When nothing could be brought back, the session starts one new shell
with a notice, and every program it ran is ended. A client from the installed
build can attach in each of these cases.

`koshi update` names every session that did not move on standard error, and that
session keeps the old build until you end it and start it again. A session
refuses the restart when a pane's program stopped reading its input, when a pane
has no terminal to carry, when this machine cannot run the new binary, or when
the new binary does not read the resume file this build writes. A session
running a koshi with no restart at all, one that still reports the old version
after the restart, and one that answers nothing within ten seconds are all
reported the same way.

`koshi restart-servers` restarts each running session, then the router, into the
koshi program on disk, as `koshi update` does for a koshi built from source. Use
it after koshi is installed another way, such as by a package manager. Each
server must come back on the version of the koshi that runs the command. The
command prints one line per server, such as
`session-3f2a1c94-8e7b-4d15-9a02-6c5138ef7b40 restarted into koshi 0.5.0; its
panes keep running`. If any server does not come back on that version, it exits
1 with `not every running koshi server now runs koshi 0.5.0; see the lines
above`.

A session or router that koshi 0.3.0 or 0.4.0 started does not restart by
itself when a package manager replaces its program file. A koshi command that
reaches such a server prints the failure, then `; the user who started it runs:
koshi restart-servers`. `koshi restart-servers` asks such a session to restart
into the program file it started from, in the request format of its release.
It ends such a router as the router paragraph below states, and starts a router
of the koshi that runs the command at once.

`koshi update` and `koshi restart-servers` also look for sessions in the
runtime directories that koshi 0.1.0 and 0.2.0 used: `$XDG_RUNTIME_DIR/koshi`
and `~/.local/share/koshi/run` on Linux, and
`~/Library/Application Support/koshi/run` on macOS. When `XDG_RUNTIME_DIR` is
unset or not an absolute path, the command looks in `/run/user/<uid>/koshi`
instead, the directory that a systemd login gives these releases. A session
that one of these releases started under another `XDG_RUNTIME_DIR` is found
when the command runs with that same value. On Windows, these releases used
the runtime directory of this koshi. A session started by koshi 0.2.0 or
one of its pre-releases cannot restart. When every other session was asked,
the command names each such session and asks once:

```text
koshi 0.6.0 cannot move session-3f2a1c94-8e7b-4d15-9a02-6c5138ef7b40, which koshi 0.2.0 started. End it and the programs in its panes? [y/N]
```

If the answer is `y` or `yes`, in any letter case, each session ends as `koshi
kill-session` ends it, together with the programs in its panes. Each one prints
`session-<uuid> ran koshi 0.2.0; koshi ended it and the programs in its panes`.
If the command runs in a pane of one of these sessions, that session ends after
the others, and the terminal of that pane closes with it. Every other answer,
and the end of standard input, keeps each session and its panes on koshi 0.2.0,
and prints `koshi: session-<uuid> keeps running koshi 0.2.0, and so do its
panes; run koshi restart-servers again to end it`. Then `koshi restart-servers`
exits 1.

A koshi 0.1.0 window serves its session from its own terminal and cannot
restart. For each open 0.1.0 window, the command prints a line that names the
window, leaves the window running, and exits 1. The window keeps its build until
its terminal closes. Koshi lists no session for the endpoint file that a closed
0.1.0 window left behind, and the router removes such a file from its runtime
directory.

`koshi list-sessions` does not list the sessions in a runtime directory of koshi
0.1.0 or 0.2.0. For each such directory where a session runs, it prints one line
on standard error, such as `koshi: 1 session that an older koshi started runs
from /home/user/.local/share/koshi/run, which this koshi does not list; run
koshi restart-servers to move it or end it`. For each such directory where a
koshi 0.1.0 window is open, it prints one more line, such as `koshi: 1 koshi
0.1.0 window runs from /home/user/.local/share/koshi/run; this koshi cannot
talk to it, and it ends when its terminal closes`.

A running session or router also restarts by itself when the koshi program it
started from now holds another koshi version. Each new connection, such as one
from `koshi attach` or `koshi list-sessions`, makes the server compare its program file
with the file it started from. If the file changed, the server runs `<program>
--version`, which has 70 seconds to print `koshi <version>`. A file that prints
the running version, or prints no version, is not run again at a connection
until it changes again. If the server refuses a command for its protocol
version, it runs `<program> --version` again, unless the last run of the same
file printed the running version. Example: sessions run koshi 0.5.0, and a package manager installs 0.6.0
at the same path. The next `koshi attach` makes the session restart into 0.6.0,
and its panes keep running. The program file is the path koshi was started as,
with its symbolic links kept. A package manager that points a link such as
`/usr/local/bin/koshi` at the new version's file changes that program file too,
and the router and every session restart into the new version. A session refuses
this restart for the reasons that `koshi update` names, keeps running its build,
and writes the reason to its log when `koshi.kdl` turns logging on. A session or
router whose restart failed runs `<program> --version` again at the first new
connection that arrives 30 seconds or more after the failure, even when the file
did not change, and restarts if the version still differs.

Each server writes a program file beside its endpoint file in the runtime
directory: `session-<uuid>.program` for a session, and `router.program` for the
router. It holds the server's process id, its koshi version, and its program
file, such as
`{"process_id":5000,"build_version":"0.5.0","program_path":"/usr/local/bin/koshi"}`.
The server writes it before it serves any connection, and removes it when it
stops serving. A program file counts only when it names the process that the
endpoint file names, and that process started before the file was written or
in the same second.

If a session of yours refuses the protocol version of a koshi command, the
command reads that session's program file. The command waits for the session to
restart only when the session may still restart into this koshi: it runs an
older koshi from the same program file, or from a program file that koshi cannot
compare with its own, or its program file is missing or cannot be read. The
command then waits up to 3 seconds for the restart to start, then up to 30
seconds for the session to come back, and then asks again. A command with a time
limit stops waiting at that limit, such as `koshi list-sessions`, which stops
waiting 5 seconds after it starts. A command does not wait for the session of
another user.

If the session does not come back, or the command does not wait, the command
prints the refusal, then `; `, then what the program file says. On koshi 0.6.0,
for the session `session-3f2a1c94-8e7b-4d15-9a02-6c5138ef7b40`:

| Program file | Text after the refusal |
|---|---|
| Newer koshi 0.7.0 | `it runs koshi 0.7.0 from /usr/local/bin/koshi, which is newer than this koshi 0.6.0; use /usr/local/bin/koshi for it` |
| Older koshi 0.5.0, same program file | `it runs koshi 0.5.0 and has not restarted into this koshi 0.6.0 yet; it tries again at each command from this koshi, and its log says what stopped it` |
| Older koshi 0.5.0, a program file that exists but that koshi cannot compare with its own | `it runs koshi 0.5.0 from /opt/koshi/koshi and has not restarted into this koshi 0.6.0 yet; it tries again at each command from this koshi, and its log says what stopped it` |
| Older koshi 0.5.0, a program file that no longer exists | `it runs koshi 0.5.0 from /opt/koshi/koshi, which no longer exists; end it with: koshi kill-session session-3f2a1c94-8e7b-4d15-9a02-6c5138ef7b40` |
| Older koshi 0.5.0, another program file | `it runs koshi 0.5.0 from /opt/koshi/koshi, a program file this koshi does not replace; use that koshi for it, or end it with: koshi kill-session session-3f2a1c94-8e7b-4d15-9a02-6c5138ef7b40` |
| The same version 0.6.0, or a version that is not semver | `it runs koshi 0.6.0 from /opt/koshi/koshi; use that koshi for it, or end it with: koshi kill-session session-3f2a1c94-8e7b-4d15-9a02-6c5138ef7b40` |
| No program file | `it runs a koshi older than 0.6.0 that cannot restart into it; end it with: koshi kill-session session-3f2a1c94-8e7b-4d15-9a02-6c5138ef7b40` |
| A program file that cannot be read | `koshi cannot tell which koshi it runs (<reason>); end it with: koshi kill-session session-3f2a1c94-8e7b-4d15-9a02-6c5138ef7b40` |

If the router refuses the protocol version, the command reads `router.program`
the same way. When the router may still restart, the command waits up to 3
seconds for a new router, then asks again. The text after the refusal ends with
`run: koshi restart-servers` where a session's ends with the `koshi
kill-session` command. `koshi update` and `koshi restart-servers` wait for a
session as stated above, for at most 20 seconds per session, and do not wait
for the router. `koshi server-version` and `koshi doctor` do not wait.

`koshi update` and `koshi restart-servers` never end a session that refuses
their protocol version. A session whose program file says it runs the installed
version, newer than the running koshi, counts as restarted. Every other one is
named with the text from its program file, such as `koshi:
session-3f2a1c94-8e7b-4d15-9a02-6c5138ef7b40 runs a koshi version this one
cannot talk to: <reason>; it runs a koshi older than 0.6.0 that cannot restart
into it; end it with: koshi kill-session
session-3f2a1c94-8e7b-4d15-9a02-6c5138ef7b40`. After a session restarts, a Hello
it refuses for its protocol version reads as the version its program file holds.
A router whose program file names a version newer than the running koshi keeps
running: the installed version counts as restarted, and any other newer version
prints `koshi: the running router runs koshi 0.7.0 from /usr/local/bin/koshi,
which is newer than this koshi 0.6.0; it keeps running; every session keeps
running`. Every other router that refuses their protocol version is ended if
koshi confirms its process, as `koshi kill-session` confirms the process of a
session. Only the router's own process ends. Every session keeps running, and
koshi starts a router of the koshi that runs the command at once, such as
`koshi ended the running router (process 5000), which ran a koshi version this
one cannot talk to, and started a router on koshi 0.6.0; every session keeps
running`. If that start fails, the next koshi command starts a router. If koshi
cannot confirm the process, it leaves that process running and prints the
command that ends it, such as `kill 5000`.

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
| `koshi config migrate` | Move every file on an older schema to the newest supported version |

Explain keys include their file kind: `koshi.pane.min-cols`,
`keybinding.chord-timeout-ms`, `theme.colors.accent`, and `profile.version`.
An unknown key exits 2 and suggests the nearest known key.

`check` and `migrate` scan `koshi.kdl`, `keybinding.kdl`, `themes/*.kdl`, and
`profile/*.kdl`. Current schema version is `2`. Version `1` files migrate to
version `2`. Version `2` files stay unchanged. Migration does not repair bad
fields: `version 1` followed by `made-up-key "x"` becomes `version 2` followed
by `made-up-key "x"`, and `check` still rejects it. A session server, a router,
and `koshi resume-support` run this migration before they read config; the
command also lets you run it directly.

Each path must be a regular file or a symlink to one. `check` reports every
read and schema error. `migrate` reports every read, KDL, and version error
before it writes anything. Migration keeps the symlink and updates its target.

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

`--client` names one viewer of a session, and a viewer-scoped command changes
only that viewer's screen. Example: terminals `client-1a2b…` and `client-3c4d…`
both watch the same tab. `koshi toggle-pane-fullscreen --client client-3c4d…`
zooms the focused pane on `client-3c4d…` alone, and `client-1a2b…` keeps its
panes tiled. A session started by koshi 0.3.0 cannot carry the named viewer, so
against one the command refuses instead of zooming the wrong viewer.

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
| `koshi list-sessions` | List session ids and names, here and on every saved server that answers |
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

`koshi list-sessions` names each session's machine in its `server` column:
`local` for a session on this machine, else the saved server it runs on. A bare
`koshi list-sessions` sweeps every saved server and appends what answered;
`koshi list-sessions --remote <server>` lists that one server's sessions alone.
A server that refused the saved secret, and a server that did not answer, are
named on standard error and their sessions are left out. On macOS, a local
session whose process runs but accepts no connection is named the same way, for
example `koshi: session session-3f2a1c94-8e7b-4d15-9a02-6c5138ef7b40 did not
answer: IPC unavailable: process 5000 runs but accepts no connection`.

Up to 16 sessions on this machine are asked at the same time, and the listing
stops waiting for them 5 seconds after it starts. A session that has not answered by
then is named on standard error and left out. So is a session that is
restarting into a new build, for example `koshi: session
session-3f2a1c94-8e7b-4d15-9a02-6c5138ef7b40 did not answer: IPC unavailable:
session session-3f2a1c94-8e7b-4d15-9a02-6c5138ef7b40 is restarting; ask again
in a moment`. The sessions that answered still print, and the command exits 4.

If a directory that holds sessions cannot be read, standard error names it,
for example `koshi: some sessions were not asked: /tmp/koshi/1002 could not be
read: Input/output error (os error 5)`. The command then exits 4. This applies
to your own runtime directory and to the shared directory of other users'
sessions. A folder that another user closed to you, such as one at mode `0700`,
is not a failure: it holds no session that you can reach. The shared directory
is not read past 65,536 entries or 256 user folders. One that holds more is
named the same way, for example `koshi: some sessions were not asked:
/tmp/koshi could not be read: it holds more than 256 user folders`, and only
the sessions read before the limit are listed.

`kill-session` takes the session id or its exact generated name; an id goes
straight to that session with no lookup. With no argument, it works only when
exactly one session is running. An unknown name or id exits 3.

`kill-session` ends a session of yours whatever koshi version it runs:

1. Koshi lists every process that runs under the session process. Then it
   sends the session a quit request, and the session has 5 seconds to answer.
2. If the session quits, koshi ends each listed process that still runs. An
   example is a `sleep 600 &` that a pane shell started in a process group of
   its own. Koshi prints `the session quit; koshi ended 1 process it left
   running`. On Windows, the session's panes run under a process of their
   own. In step 1, koshi also lists that process and every process under it.
   When the session quits, these processes have 2 seconds to end. Then koshi
   ends each one that still runs. If the process that holds the panes still
   runs, koshi also ends every process under it at that moment.
3. If the session does not quit, koshi ends the session process and every
   process under it. This includes a session that runs a koshi version this
   one cannot talk to, and a session that does not answer. Koshi prints, for
   example, `the session did not quit (IPC unavailable: the session did not
   answer in time); koshi ended its process 5000 and 3 processes under it`.

The count that koshi prints holds only the processes that koshi ended. A
process that ends before koshi ends it is not counted. For example, on
Windows, a pane runs `ping`, and the process that holds the panes ends with
`ping` within the 2 seconds: koshi ends nothing and prints nothing. On
Windows, a listed process that ends with the process that holds its pane,
after koshi ended that process, is counted.

On Linux and macOS, koshi sends each process `SIGHUP`, `SIGTERM` and
`SIGCONT`, and `SIGKILL` once 2 seconds pass if the process still runs. On Windows,
koshi ends each process at once, and also ends the process that holds the
session's panes.

Koshi ends a process only when it runs as you and started under the session
process, and no process between the two runs as another user. For example, a
pane runs a `root` process that starts `vim` as you: koshi leaves `vim`
running. On Linux and macOS, a process under the session process can also be
one that runs in the POSIX session of a pane. A POSIX session is the set of
processes that share one terminal, and a process stays in it after its parent
ends. For example, a script in a pane starts `sleep 600 &` and ends: the
`sleep` gets a new parent, and koshi still ends it. If the new parent is
process 1, or another process that the session process runs under, its user
does not matter. On Linux and macOS, koshi does not end a process that a
closed pane left running. For example, a pane shell starts `nohup sleep 600 &`
and exits, and the pane closes: koshi leaves that `sleep` running. On Windows,
a program that a pane starts is in a job of the process that holds the panes.
It ends when that process ends, also after its pane closed.

Koshi never ends another koshi process, such as a router or a session
that a pane started, or a process under one. Koshi ends the session process
only when that process runs the `koshi` program and started before the session
wrote its endpoint file. If koshi cannot confirm this, the session does not
quit, and a process with that id runs, the command exits 1. It names the
process and the command that ends it, for example `koshi cannot confirm that
process 5000 is session-…, and leaves it running. If it is, end it with: kill
5000`. While it runs, `kill-session`
ignores Ctrl+C and a terminal that closes.

A session that another user started gets the quit request alone. If that
session does not answer, the command exits 4.

`attach` run outside koshi opens that session in this terminal. Run inside a
koshi pane, it moves this terminal to the named session instead. With no
argument it lists the sessions running for this user and the sessions on every
saved server that answered, numbers them, and reads your answer. A session on a
saved server carries `(remote: <server>)`:

```text
koshi attach
1) amber-fox session-3f2a…
2) quiet-heron session-91c4… (remote: work)
attach to which session? [1-2]
```

A listing of exactly one session, on this machine, is attached without asking.
Every other listing asks, one session on a saved server included, whose prompt
reads `attach to which session? [1]`.

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
| `koshi resize-pane` | `--direction`, `--size`, `--pane`, `--client` | Move one border by signed cell count |
| `koshi move-pane` | `--direction`, `--pane` | Swap a pane with its visible neighbor in one step |
| `koshi place-pane` | `--pane`, `--tab`, `--direction`, `--client` | Insert a pane at one side of another tab's tiled layout |
| `koshi scroll-pane` | `--lines`, `--pane`, `--client` | Scroll one client's view of a pane by signed line count |
| `koshi focus-pane` | `--pane`, `--client` | Focus a pane |
| `koshi toggle-pane-fullscreen` | `--client <CLIENT_ID>` | Toggle the focused pane's fullscreen view |
| `koshi input "<TEXT>"` | `--pane`, `--no-enter` | Type text; Enter follows unless held back |

Directions: `right`, `down`, `left`, `up`. A positive resize grows toward the
direction; a negative resize shrinks from that side. On a floating pane,
`--client` names the client whose view keeps the edge opposite the moved border
in place. With no `--client`, the issuing client acts, else the session's only
attached client. With several attached clients and no `--client`, the resize is
refused. A floating pane grows only up to that client's pane area edge on the
moved side: a 40-column pane at column 20 of 80 columns, grown right by 30,
becomes 60 columns wide. A pane that client pinned keeps its pinned cell. On a
tiled pane, the resize ends the fullscreen view of the client `--client` names.
A positive `scroll-pane --lines` moves toward history.

Example: `koshi input --pane pane-… --no-enter "git status"` leaves
`git status` at that pane's prompt without running it.

## Tabs

| Command | Main flags | Result |
|---|---|---|
| `koshi new-tab` | `--session <NAME_OR_ID>`, `--client <CLIENT_ID>` | Open a tab with one shell pane |
| `koshi close-tab` | `--tab <NAME_OR_ID>`, `--session <NAME_OR_ID>`, `--force` | Close a tab |
| `koshi next-tab` | `--client` | Focus the next tab |
| `koshi previous-tab` | `--client` | Focus the previous tab |
| `koshi focus-tab` | `--index` or `--tab <NAME_OR_ID>`, optional `--client` | Focus one tab |
| `koshi move-tab` | `--index`, optional `--tab <NAME_OR_ID>` | Move one tab to a zero-based index |

`--client` on `new-tab` names the terminal that switches onto the new tab. With
one terminal attached the flag is optional. If two or more terminals are
attached and the command names no terminal, it fails with `several clients are
attached; name the target client`. A pane opened for a terminal names that
terminal. A pane the session started with names none, and neither does a
command from outside koshi.

Example: terminals `client-1a2b…` and `client-3c4d…` both watch session
`amber-fox`. `koshi new-tab --client client-3c4d…` opens a tab and moves
`client-3c4d…` onto it; `client-1a2b…` keeps the tab it was on.

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
| `koshi keys list [--mode <MODE>] [--scope default\|user\|session\|layout] [--format table\|json]` | List effective shortcuts |
| `koshi keys describe "<KEY_SEQUENCE>"` | Explain one shortcut |
| `koshi keys conflicts` | Report clashes, dead shortcuts, and warnings |
| `koshi keys validate <PATH>` | Check a shortcut file without applying it |

## Remote access

| Command | Result |
|---|---|
| `koshi share grant <IDENTITY> [--session <SESSION>] [--expires <DURATION>]` | Grant an identity a remote access token |
| `koshi share revoke <IDENTITY> [--session <SESSION>]` | Revoke the tokens an identity holds |
| `koshi share list [--session <SESSION>] [--format table\|json]` | List the tokens granted on this machine |
| `koshi attach --remote <SERVER> [--save-as <NAME>] [SESSION]` | Attach to a session on the machine `SERVER` names |
| `koshi list-sessions --remote <SERVER>` | List the sessions on the machine `SERVER` names |
| `koshi remote new` | Save a server, asking for its name, address and secret |
| `koshi remote edit <SERVER>` | Change one saved server's name, address or secret |
| `koshi remote list [--format table\|json]` | List the servers this machine has saved |
| `koshi remote forget <SERVER>` | Drop one saved server |
| `koshi remote set-secret <SERVER>` | Replace the secret of one saved server |

The three `share` verbs run on the machine holding the sessions. The rest run
on the machine connecting to it.

An absent `--session` reads one way on `grant` and another way on `revoke`.
`koshi share grant alice` gives alice one token that reaches every session on
this machine. `koshi share revoke alice` stops every grant alice holds: the one
that reaches every session, and each one that reaches a single session.

`koshi share revoke alice --session quiet-lake` stops the grant scoped to that
session. A host-wide grant reaches `quiet-lake` too, and no revoke stops a
host-wide grant for one session alone, so when alice holds one this asks before
it stops anything:

```text
alice also holds a host-wide grant, which reaches quiet-lake.
stopping the grant on quiet-lake alone leaves alice reaching it through the
host-wide one.
stopping both leaves alice reaching no session on this machine, not just
quiet-lake.
stop both the grant on that session and alice's host-wide grant? [y/N]
```

A yes stops both. A no stops neither and prints `nothing was revoked.`. Grants
alice holds on other sessions are untouched either way.

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

With the port open, the grant prints the command that connects from another
machine. The address in that command depends on `remote-listen`:

- `0.0.0.0:7654` gives one command for each IPv4 address of this machine, and
  `[::]:7654` gives one for each IPv6 address. Koshi lists the addresses of each
  network interface that is up. It leaves out loopback and link-local
  addresses, and puts first the address this machine sends to the internet
  from. Each command ends with the name of its interface. If this machine has no
  such address, the command shows `<this machine's address>` in place of one.
- One address of this machine, such as `192.168.1.20:7654`, gives one command
  with that address. A line then says that this machine accepts connections on
  that address only.
- A loopback address, such as `127.0.0.1:7654`, gives no command. A line says
  that no other machine can connect to it.

```text
connect from another machine, at the address of this one it can reach:
  koshi attach --remote 192.168.1.20:7654 --save-as alice [SESSION]   # en0
  koshi attach --remote 100.64.0.2:7654 --save-as alice [SESSION]   # utun3
set KOSHI_REMOTE_SECRET to the secret above, or paste it when asked.
```

Use the command with an address the other machine can reach. For example, a
laptop on the same Wi-Fi uses the `en0` address, and a laptop on a VPN uses the
`utun3` address.

`koshi share revoke alice` ends the connections alice's tokens opened, at once,
attached to a session or not. Her connection stops and no further frame reaches
her; her next command is not merely refused. Granting alice again on the same
scope replaces her token and ends the connections the replaced token opened,
the same way. A token that runs out on its own is different: it stops a new
connection from opening and never interrupts one already attached.

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
the terminal without printing what is typed. No flag takes a secret.

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

`koshi remote new` saves a server without attaching to one of its sessions. It
asks three questions in turn — the name, the address, and the secret — and
every answer is needed. It then dials the server once to check that it admits
the secret:

```text
$ koshi remote new
every answer is needed. Ctrl-C stops without saving.
name: work
address: laptop.local:7654
secret:
checking laptop.local:7654 …
saved work at laptop.local:7654.
```

A server that does not admit the secret is named, and the last question is
whether to save what was typed anyway. A server saved that way holds no
fingerprint, and its first connection pins the certificate it meets:

```text
checking laptop.local:7654 …
koshi: IPC unavailable: laptop.local:7654 refused the connection: nothing is listening on that port. if remote access is not enabled on that machine, run `koshi share grant` there and answer yes to the offer to open the port
save it anyway? [y/N]: y
saved work at laptop.local:7654; its certificate is pinned on the first connection.
```

Answering anything else prints `nothing was saved.` and writes nothing.

`koshi remote edit <SERVER>` asks the same three questions with the saved
values in brackets. An empty answer keeps the value in brackets, and an empty
secret keeps the saved secret, so only what changes is typed:

```text
$ koshi remote edit work
press Enter to keep the value in brackets. An empty secret keeps the saved one. Ctrl-C stops without saving.
name [work]:
address [laptop.local:7654]: laptop.local:7655
secret:
checking laptop.local:7655 …
updated work at laptop.local:7655.
```

An edit that keeps the address requires the pinned fingerprint on the check, so
a certificate that changed under that address does not pass. An edit that
changes the address requires none: a pinned fingerprint stands for the address
the record held when that certificate was met.

A check that passes pins the certificate the server presented, either way. A
check that does not pass keeps the pinned fingerprint while the address is
unchanged, and keeps none once the address changed; the next connection to the
new address pins the certificate it meets. When the check does not pass, that
question names it:
`save the change anyway? The certificate at that address is pinned on the
first connection to it. [y/N]`.

Nothing is written until every answer has settled. Ctrl-C at any question, and
input that ends before an answer arrives, leave the saved server unchanged.

The store is read again at the moment the record is written, and the read and
the write are held against every other `koshi` by a lock on
`remote/servers.lock` beside the store. A server another `koshi` saved while
the questions were open is still saved, and a name or an address that another
record took meanwhile is refused with nothing written. Every command that
changes a saved server takes that lock, `koshi attach` included, which stamps
the record it dialled. A lock another `koshi` still holds after five seconds
reads as `koshi: IPC unavailable: another koshi is changing the saved servers;
try again`. The operating system releases the lock if the `koshi` holding it
dies. The lock is never held while a question waits for an answer.

`koshi remote set-secret` reads the record again under that lock, after the
secret is typed. A server another `koshi` forgot meanwhile is refused, and the
record it forgot stays forgotten.

An edit reads the record it changes again at that same moment. A record whose
name, address, secret or fingerprint another `koshi` changed while the
questions were open is refused, and the older values are not written:

```text
koshi: invalid arguments: work changed while the questions were open, so nothing was saved; run `koshi remote edit work` again
```

The added time and the last-used time are not compared. Another `koshi` that
only dialled this server does not stop the edit, and the edit carries the
values the record on disk holds for both.

A saved server that pins no certificate is left out of the sweep that a bare
`koshi list-sessions` and a bare `koshi attach` make over every saved server,
and one stderr line names it. Naming it — `koshi list-sessions --remote work` —
connects, pins the certificate that server presents, and the sweep includes it
from then on.

A token is full access to every session it reaches. `koshi share grant alice`
reaches every session on the serving machine, including the sessions started
after that grant. `koshi share grant alice --session quiet-lake` reaches that
one session. Typing into a shell of one of those sessions acts as the user who
runs the session — the same as sitting at that machine and typing there. Hand a
token to somebody only when you would hand them that account.

The token store on the serving machine holds the sha256 of each token it
granted, never the token itself. A token nobody kept is replaced by a fresh
`koshi share grant`, and is never read back out of the store.

Bare `koshi attach` lists the sessions on every reachable saved server beside
this machine's own, each row naming the server it belongs to. The remote check
waits two seconds in total, not two seconds per server. A server not heard from
inside that wait is left out. A server that answers with a refusal is not
hidden. It prints the same sentence that `koshi attach --remote work` prints,
which names what to do:

```text
koshi: work: the server 192.0.2.10:7654 did not admit the connection. if that machine runs koshi 0.3.0 or 0.4.0, update koshi there. otherwise the token was rejected or revoked: re-grant it on that machine with `koshi share grant`, then store the new secret with `koshi remote set-secret` for a saved server, or give it when the next dial asks
```

A machine that runs koshi 0.3.0 or 0.4.0 prints its own sentence for a saved
server that runs this koshi. Its `koshi list-sessions` prints ``koshi: work:
the saved secret was refused; run `koshi remote set-secret work` ``, and its
bare `koshi attach` prints the same sentence without `koshi: `, also when the
cause is the version difference. There, `koshi attach --remote work` names
both protocol ranges, such as `the caller speaks remote protocol versions 1 to
1, this koshi speaks 2 to 2 (server 192.0.2.10:7654)`. A new secret does not
help. Update koshi on that machine.

`--remote` never creates a session. It takes `attach`, `list-sessions`, and the
action verbs — the verbs that open, close, resize, focus, and type into panes
and tabs, and the lock verbs. Bare `koshi --remote work` names nothing to run,
and every other verb, `koshi share --remote work` and `koshi doctor --remote
work` included, is refused:

```text
--remote works with `attach`, `list-sessions`, and the action verbs, such as `koshi attach --remote <server>`
```

`koshi share grant` prints the new token's secret, and `koshi share list`
prints every identity holding one. Inside a koshi pane, the session paints that
pane to every client viewing its tab, a client on another machine included.

`koshi share grant`, `koshi share revoke` and `koshi share list` run inside
a koshi pane are refused while any client is attached to that pane's session
from another machine:

```text
koshi: command not permitted
  someone is attached to this session from another machine, and they see this
  pane. Run `koshi share` from a terminal outside koshi.
```

A verb run in a pane asks that one session who is attached to it. It asks
before it resolves `--session` and before it asks the router anything. A
session that cannot answer is refused the same way, and the refusal carries
the failure it hit:

```text
koshi: command not permitted
  this session could not say who is attached to it, so whether anyone sees
  this pane from another machine is unknown: <reason>. Run `koshi share` from
  a terminal outside koshi.
```

A session server too old to say where a client connected from lists that
client with no origin. Such a row is refused the same as a client from another
machine.

Both refusals exit 1 and print nothing on standard output. No token is
granted, revoked or listed, and the token store is neither read nor written.

`KOSHI` in the environment is what marks a koshi pane. A `koshi share` verb run
outside every koshi pane always reaches the router, whoever is attached to
whatever session. To run a share verb while somebody is attached to your pane's
session from another machine, run it from a terminal outside koshi, or detach
them first.

Tokens are granted only from the machine holding the sessions.

## Versions

| Command | Result |
|---|---|
| `koshi version [--format table\|json]` | Print the build of the koshi program you just ran |
| `koshi server-version [--session <NAME_OR_ID>] [--format table\|json]` | Print the build each running koshi server runs |

`koshi version` prints the same line as `koshi --version`.

```text
koshi version
koshi 0.5.0
```

These two answers differ while an update rolls out. `koshi update` installs a
new binary, each session server replaces its own image one at a time, and then
the router restarts into it. Until every swap lands, the program your shell runs
is a newer build than the process answering it:

```text
koshi server-version
kind     session                                       version
router   -                                             0.5.0
session  session-3f2a1c94-8e7b-4d15-9a02-6c5138ef7b40  0.5.0
session  session-91c4de07-2b53-41a8-bf6e-70d9a2c81f35  0.3.0
```

The version column reads:

| Cell | Meaning |
|---|---|
| a build, like `0.4.0` | The server answered and named it |
| `unknown` | The server answered and is too old to name its build |
| `not running` | Nothing is listening there |
| `unreachable` | The server could not be asked, the session is restarting, or a session did not answer within 5 seconds after the router was checked; the reason prints on standard error |

A server that could not be asked does not stop the rest of the answer: the
other rows still print, and the command exits 4. Up to 16 sessions are asked
at the same time. Sessions of other users that the shared directory holds past its
listing limit earn no row: standard error names how many, and the command
exits 4. A directory of sessions that cannot be read earns no row either:
standard error names it, and the command exits 4. Everything answering exits 0,
including a machine running nothing at all.

`--session` reports that one session and leaves out the router. It takes the
session id or its exact generated name. A name must match exactly one running
session.

## Checking the installation

| Command | Result |
|---|---|
| `koshi doctor [--format table\|json]` | Check this machine's koshi installation |

```text
koshi doctor
check               verdict  reason                                                                              help
config              ok       3 config files validated                                                            -
shell               ok       a new pane runs /bin/zsh                                                            -
terminal            warn     TERM is not set                                                                     set TERM before running koshi, for example TERM=xterm-256color
runtime directory   ok       /tmp/koshi-1000 is ready; koshi names it after your user id                         -
log directory       ok       /home/you/.local/state/koshi/logs is writable and logging is off                    -
router              ok       no koshi is running                                                                 -
session directory   ok       sessions are advertised in /tmp/koshi-1000 (mode 700), which only you may reach     -
remote access       ok       koshi.kdl names no remote listen address, and this machine holds 0 standing grants  -
remote connections  ok       no koshi is running, so nothing from another machine is connected                   -
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
| `runtime directory` | The runtime directory: which directory it is, that it can be read, that it is private, and which rule produced its path |
| `log directory` | The log directory: that a file can be written there, and whether `koshi.kdl` turns logging on |
| `router` | Whether a router answers on its control socket |
| `session directory` | Where sessions are advertised, and who may reach that directory |
| `remote access` | `koshi.kdl`'s remote listen address, and how many access grants still stand |
| `remote connections` | How many open connections the running router holds from another machine |

The `session directory` and `remote connections` rows report facts and rate
nothing. The `remote access` row rates one thing: it reads `warn` when the
grants could not be read. A format `1` grant file is counted without changing
it; the router converts it when it starts. `koshi doctor` starts no koshi and
creates no directory. The `log directory` row writes one empty file in the log
directory and removes it again, which is how it reports whether that directory
can be written.

The `router` row is the only row that rates the running router. A router whose
build has no such question is `warn`, and its help reads `run: koshi
restart-servers`. A router that koshi 0.2.0 to 0.4.0 started is `fail`: `the
running router runs koshi 0.4.0 or older, which this koshi cannot talk to`,
and its help reads `run: koshi restart-servers`. Any other router that is
listening and does not answer is `fail`. In each of these cases the `remote
connections` row reads `the running router did not answer, so this is not
known`.

A row whose `reason` is shortened to fit the table carries the whole text in a
`detail` field, which `--format json` prints and the table leaves out. Every
other row has `"detail": null`.

### The runtime directory

koshi keeps its router socket and its session sockets in one directory per
user. On Linux and macOS that directory is `/tmp/koshi-<your user id>`, built
from your user id and nothing else. On Windows it is `run` under your
application data directory.

`KOSHI_RUNTIME_DIR` names the directory instead, and koshi reads it only when
it holds an absolute path. A relative value is ignored. Two koshi processes
holding different values use different directories and do not find each other.

`koshi doctor` prints the directory in use and the rule that produced it.

## Debugging

| Command | Result |
|---|---|
| `koshi debug dump-state [--format table\|json]` | Print every running session's sessions, tabs, panes, and clients |
| `koshi debug dump-layout [--tab <NAME_OR_ID>] [--format table\|json]` | Print each tab's split tree, solved rectangles, panes with no room, stacks, and per-client focus |
| `koshi debug events [--since <LENGTH>] [--filter <NAME>] [--format table\|json]` | Print the events each running session published most recently, oldest first |

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

`koshi debug events` prints the last 1000 events a session published. Each line
names when the record was stamped, which event it was, and the ids it named. No
line carries content: a text selection prints as `SelectionChanged` with its
client and pane ids, never as the selected text.

A session remembers events only for as long as its server process runs. A
restart starts the list empty.

Each row names the session by id and by name, so two sessions sharing a name
stay apart.

`--since` keeps the events recorded within a length of now — `30s`, `5m`, `2h`,
`7d`. `--filter` keeps the events whose name contains the text given, matched
ignoring case, so `--filter pane` keeps `PaneCreated` and `PaneFocused`. An
empty `--filter` is a usage error.

Example: `koshi debug events --since 30s --filter tab` prints the tab events of
the last thirty seconds and nothing else.

A session that started before you installed this Koshi keeps no such buffer.
`events` says so and names what to do: restart that session.

A shell that emits the OSC 133 prompt markers turns each command it runs into a
pair of events: `PaneCommandStarted` when the command starts, and
`PaneCommandFinished` when it ends. Both name the pane and nothing else — never
the command line, never its output. A shell that emits no markers publishes
neither event.

Example: running `cargo test` in pane `pane-7f3a…` under such a shell results
in two rows whose `event` cells read `PaneCommandStarted` and
`PaneCommandFinished`, each with `pane-7f3a…` as its only id.
`koshi debug events --filter panecommand` keeps that pair.
