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
| `koshi --headless --allow-other-users` | Open that session so the other users of this machine may reach it (macOS and Linux only) |
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

The SHA-256 checksum that `koshi update` checks comes from `checksums.txt` of
the same GitHub release. It finds an archive that changed on the way, such as
a download that stopped part way. It does not find a release whose archive and
`checksums.txt` were both replaced. `install.sh` and `install.ps1` check no
checksum. Every way to install or update koshi downloads from the GitHub
account `gohyuhan`: its releases, its Homebrew tap and its Scoop bucket. Each
download is as safe as that account and the permissions to change it.

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

On Linux, `install.sh` installs into `/usr/local/bin` the same way: it writes
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
seconds to come back on the new release. A session that takes the request has 5
seconds more to answer it. One that does not come back is reported as not
confirmed, and the update goes on. A server comes back when a new process
answers with the new release. An answer from the process that took the request
does not count, even when it names the new release. A session that does not
take the request in time does not restart. A session that took the request and
gave no answer may still restart, and its line says so. A router that does not
answer its first message in time is not asked to restart, and its line says so.
A router that took the request and gave no answer may still restart, and the
update waits 20 seconds for it to come back on the new release, as for a router
that answered. A session keeps its panes, the programs
running in them and their scrollback. A client from the installed build can
reattach to that session. The installed build migrates `version 1` KDL files to
`version 2` when a running session asks it which resume formats it reads, before
that session restarts. A session server or router migrates them again when it
starts. A file with an unknown key or a bad value migrates and keeps that
setting in its text. If a file cannot be read, holds KDL that does not parse, or
declares an unusable version, or the lock file `.migration.lock` in the config
directory cannot be opened or locked, migration writes no file and prints the
error on standard error. When `koshi.kdl` turns logging on, a session server
also writes the error to its session's log file, and a router to
`koshi-log-router.log`. The session still restarts and the router still
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
keeps its secret and certificate fingerprint, and an existing grant keeps its
scope and expiry. A saved server that holds no fingerprint, such as one an
older koshi saved before it ever connected, is not dialled until `koshi remote
edit` gives it one.

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
after the restart, one that answers nothing within 20 seconds, and one that took
the request and gave no answer are all reported the same way.

`koshi restart-servers` restarts each running session, then the router, into the
koshi program on disk, as `koshi update` does for a koshi built from source. Use
it after koshi is installed another way, such as by a package manager. Each
server must come back on the version of the koshi that runs the command. The
command prints one line per server, such as
`session-3f2a1c94-8e7b-4d15-9a02-6c5138ef7b40 restarted into koshi 0.5.0; its
panes keep running`. If any server does not come back on that version, it exits
1 with `not every running koshi server now runs koshi 0.5.0; see the lines
above`. A router that cannot start the new koshi keeps serving its own build.
When `koshi.kdl` turns logging on, it writes the cause to
`koshi-log-router.log` in the log directory at warn level.

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
It works on macOS and Linux only. On Windows koshi refuses it with exit code 2
and `koshi: invalid arguments: --allow-other-users works on macOS and Linux
only; on Windows only you reach your sessions`, and starts no session.

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
by `made-up-key "x"`, and `check` still rejects it. A session server and a
router run this migration before they read config; the command also lets you
run it directly. `koshi resume-support` leaves the files as they are.

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
panes tiled. A pane command typed inside a pane of the same session still acts
on that pane: `koshi pin-pane --client client-3c4d…` typed in floating pane
`htop` pins `htop` on `client-3c4d…`, whatever pane `client-3c4d…` focuses. A
session started by koshi 0.3.0 cannot carry the named viewer, so against one
the command refuses instead of zooming the wrong viewer.

Session and tab flags that say `NAME_OR_ID` accept either their generated name
or printed id. A value that reads as an id is always the id — it never falls
back to a name lookup. A name several targets share is refused, and the error
lists every matching id. Pane and client flags use printed ids.

An id that no running session holds is refused with `no running session has
tab tab-3f2a…`. With `--session`, and inside a pane, an id that the session
does not hold is refused with the id and the session, such as ``tab tab-3f2a…
is not in session `work` `` or ``client client-1a2b… is not attached to session
`work` ``. Both refusals exit 1.

A session that another user of this machine started also takes the form
`<user id>/<session id>`, as `koshi list-sessions` prints it in its `owner`
column. Example: `koshi attach 1001/session-3f2a…` looks for that session only
in the folders of the shared directory that user `1001` owns, and reads no file
of yours. Text with a `/` in any other form, such as `1001/quiet-lake`, is
refused with exit code 2 and ``expected `<user id>/session-<uuid>`, such as
`1001/session-<uuid>` ``. Your own user id is refused with exit code 2:
``1000/session-3f2a… names a session of yours; name it by its id alone:
`session-3f2a…` ``. A session that the folders of that user do not advertise
exits 3 and names the whole form, such as `1001/session-3f2a…`. On Windows no
folder of another user exists, and every such form exits 3.

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

Both formats print the same text for an id, a time, and a state:

- An id prints in the form a target flag takes: `pane-0192f0c1-…`, so a
  `--pane` flag takes the `pane_id` that `--format json` prints.
- A time prints as RFC 3339 UTC text, rounded down to the whole second:
  `2025-01-01T00:00:00Z`. A year before 0 or after 9999 prints with a sign
  and at least five digits: `-00001-12-31T23:59:59Z`, `+10000-01-01T00:00:00Z`.
- A state prints as the word the table prints: `running`, `host`,
  `not running`. A client's input mode prints as its keymap name, such as
  `normal` or `pane-placement`.

In a table, an absent value prints as `-` and a list of ids prints as its
count. In JSON, an absent value is `null`, a list of ids is the whole list,
and a size, a rectangle, or a cell is an object of numbers:
`"viewport_size": {"column_count": 120, "row_count": 40}`. `koshi inspect
client --format json` writes `pane_area` as `null` for no report,
`"starving"`, or a size object, and `origin` as `"local"`, `"remote"`, or
`null`.

`koshi list-panes` lists the tiled panes of each session in tab order, then
its floating panes in the order they became floating. A floating pane prints
`float` in the `tab` column and `-` in the `tab_name` column, and `--format
json` writes `"tab_id": null` and `"tab_name": null` for it. `koshi inspect
pane` prints `float` as the `tab` of a floating pane. A `--pane` flag finds a
floating pane's session from outside koshi too: `koshi close-pane --pane
pane-…` closes that floating pane.

`koshi list-sessions` names each session's machine in its `server` column:
`local` for a session on this machine, else the saved server it runs on. Its
`owner` column names the user id of another user whose folder of the shared
directory advertises the session, such as `1001`, and `-` for every other
session. `--format json` carries the same value as `owner_user_id`, and `null`
for every other session. A bare
`koshi list-sessions` sweeps every saved server and appends what answered;
`koshi list-sessions --remote <server>` lists that one server's sessions alone.
A server whose connection failed, and a server that did not answer in time,
are named on standard error and their sessions are left out. On macOS, a local
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

`kill-session` takes the session id, `<user id>/<session id>`, or its exact
generated name; an id goes straight to that session with no lookup. With no
argument, it works only when exactly one session is running. An unknown name
or id exits 3.

`kill-session` ends a session of yours whatever koshi version it runs:

1. Koshi lists every process that runs under the session process. Then it
   sends the session a quit request. The session has 5 seconds to take the
   request, and 5 seconds more to answer it.
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
saved server that answered, numbers them, and reads your answer. A session that
another user of this machine started shows as `<user id>/<session id>`, and a
session on a saved server carries `(remote: <server>)`:

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
`<user id>/<session id>`, or a session name. `--all` detaches every terminal of
one session.

A session left with no terminal keeps running unless `auto-close-session` is on
in `koshi.kdl`, which ends it once the last terminal leaves.

## Panes

| Command | Main flags | Result |
|---|---|---|
| `koshi new-pane` | `--direction`, `--stacked`, `--floating`, `--pane`, `--tab`, `--session`, `--client` | Open a shell pane |
| `koshi run -- <COMMAND>...` | Same placement flags as `new-pane` | Open a pane running the command |
| `koshi close-pane` | `--pane`, `--force` | Close a pane |
| `koshi resize-pane` | `--direction`, `--size`, `--pane`, `--client` | Move one border up to a signed cell count |
| `koshi move-pane` | `--direction` and `--pane`, or `--pane`, `--x`, `--y` and `--client` | Swap a tiled pane with its visible neighbor in one step, or move a floating pane |
| `koshi place-pane` | `--pane`, `--tab` and `--direction` or `--stack-onto`, `--client` | Insert a pane at one side of a tab's tiled layout, or into a stack |
| `koshi undock-pane` | `--pane`, `--x`, `--y`, `--client` | Float a tiled pane up where it is drawn |
| `koshi dock-pane` | `--pane`, `--tab` and `--direction` or `--stack-onto`, `--client` | Put a floating pane into a tab's tiled layout |
| `koshi pin-pane`, `koshi unpin-pane` | `--pane`, `--client` | Pin or unpin a floating pane on one screen |
| `koshi minimize-pane`, `koshi restore-pane` | `--pane` or `--all`, `--client` | Minimize or restore floating panes on one screen |
| `koshi scroll-pane` | `--lines`, `--pane`, `--client` | Scroll one client's view of a pane by signed line count |
| `koshi focus-pane` | `--pane`, `--next-floating` or `--previous-floating`, `--client` | Focus a pane |
| `koshi toggle-pane-fullscreen` | `--client <CLIENT_ID>` | Toggle the focused pane's fullscreen view |
| `koshi input "<TEXT>"` | `--pane`, `--no-enter` | Type text; Enter follows unless held back |

Directions: `right`, `down`, `left`, `up`. A positive resize grows toward the
direction; a negative resize shrinks from that side. A pane at the tab edge on
that side moves its opposite border the same way instead. The border moves up
to `--size` cells, as far as minimum sizes allow. In a tab of two 40-column
panes, `resize-pane --direction left --size 100` on the right pane grows it by
36 columns, and the left pane keeps its 4-column minimum. A border that cannot
move one cell is refused, and nothing changes:

```text
koshi: below minimum size
  pane-0192f0c1-… cannot grow left here; the pane next to it has no room to give
```

The pane that gives the cells has no room to give when it is at its minimum
size or hidden for lack of room. A shrink is refused as `<pane> cannot shrink
from the left here; it has no room to give`. A border that the layout holds in
place is refused as `invalid in the current state`, with `<pane> cannot grow
left here; the layout holds this border in place`. The layout holds a border in
three cases: a pane in the same split keeps the `preferred` size a profile
gives it; the pane that takes the cells holds its minimum size with cells a
third pane lends it; or the pane that takes the cells is hidden for lack of
room. In an 80-column tab whose left pane has `preferred 20`, the border
between the two panes stays at column 20.

On a floating pane, `--client` names the client whose view keeps the edge
opposite the moved border in place. With no `--client`, the issuing client
acts, else the session's only attached client. With several attached clients
and no `--client`, the resize is refused. A floating pane grows only up to that
client's pane area edge on the moved side: a 40-column pane at column 20 of 80
columns, grown right by 30, becomes 60 columns wide. A pane that client pinned
keeps its pinned cell. On a tiled pane, the resize ends the fullscreen view of
the client `--client` names. A positive `scroll-pane --lines` moves toward
history.

`focus-pane --pane` also takes a floating pane, minimized or not. That pane
then shows above the client's other floating panes and takes the client's keys.
A client that cannot draw floating panes refuses it with `<pane> cannot be
drawn on this client`. `scroll-pane --pane` on a floating pane that the client
minimized is refused with `<pane> is minimized; restore it first`.

Without `--pane`, `close-pane`, `resize-pane`, `scroll-pane` and `input` act on
one pane. Inside a koshi pane, the command acts on that pane. Outside koshi, the
command acts on the pane that takes the client's keys: its focused floating
pane, else its focused pane in its tab. `new-pane` without `--pane` is refused
with `<pane> is floating and has no split to divide; use new-pane --floating, or
dock-pane first`, and
`toggle-pane-fullscreen` with `<pane> is floating; fullscreen fills a tab`, in
two cases: inside a floating pane, and outside koshi while a floating pane takes
the client's keys.

Example: with a floating `htop` focused over a shell, `koshi close-pane` run
outside koshi closes `htop`, and the keys return to the shell.

Example: `koshi input --pane pane-… --no-enter "git status"` leaves
`git status` at that pane's prompt without running it.

Without `--pane` or `--tab`, `new-pane` and `run` split one pane. Inside a koshi
pane, they split that pane. Outside koshi, they split the pane that takes the
keys of the client `--client` names, else of the session's only attached
client. With several attached clients and no `--client`, the command fails
with `several clients are attached; name the target client`.

The client that `--client` names shows and focuses the new pane. If that client
views another tab, it moves onto the tab of the new pane first. Without
`--client`, the client of the pane the command runs in does this. Outside koshi
without `--client`, the new pane takes no client's focus on a tab that a client
already views. On a tab that no client views, the session's only attached
client moves onto the tab and focuses the new pane.

Example: terminal `client-1a2b…` views tab `logs`, and terminal `client-3c4d…`
views tab `build`. `koshi new-pane --direction right --client client-3c4d…`,
typed outside koshi, splits the pane that `client-3c4d…` focuses in `build`.
`client-3c4d…` then focuses the new pane, and `client-1a2b…` stays on `logs`.

### Floating panes

`koshi new-pane --floating` and `koshi run --floating -- <COMMAND>...` open a
pane that floats above the tiled panes and belongs to no tab. The client that
`--client` names, else the issuing client, else the session's only attached
client, shows it and gives it its input. Every other client holds it
minimized. A client that cannot draw floating panes also holds it minimized,
and its input stays where it was: its pane area is smaller than `5x6`, or its
tab shows `Terminal too small — enlarge window` in place of every pane.

- `--cols` and `--rows` set its size: a cell count such as `80`, or a percent
  of the pane area such as `60%`. Each needs the other. Without them the pane
  takes 60% of each axis. Each axis is cut to 90% of the smallest pane area of
  the attached clients, rounded down: `--cols 100% --rows 100%` on an `80x22`
  pane area gives `72x19`. A client with a pane area smaller than `5x6` does
  not count.
- `--x` and `--y` set its top-left cell, counted from the top-left corner of
  the pane area. Each needs the other.
- `--pinned` pins it on the client that shows it.
- `--floating` is refused with `--direction`, `--stacked`, `--pane` and
  `--tab`.

Example: `koshi run --floating --cols 80 --rows 60% --x 4 --y 2 -- htop` opens
`htop` in a floating pane 80 columns wide and 60% of the pane area high, with
its top-left cell at column 4, row 2.

`koshi undock-pane` floats a tiled or stacked pane up where the client draws
it: the pane keeps its top-left cell and its size, each axis cut to 90% as
above. A stack member that the client draws collapsed, on its header, keeps
the top-left cell and the size it opens into in its stack. `--x` and `--y`
move its top-left cell. The client shows it and gives
it its input, unless it cannot draw floating panes, as above. Every other
client whose input is on the pane does the same, at the top-left cell where
that client draws the pane. Every other client holds it minimized. The other
panes of its tab fill the space it leaves. The last tiled pane of the
session's only tab is refused with `the last pane of the last tab cannot be
undocked; a session needs one tiled pane`, and a 13th floating pane with `a
session holds at most 12 floating panes`.

Example: tab `#1` holds `A | B` in an `80x22` pane area. `koshi undock-pane
--pane <B>` leaves `A` alone in the tab, and `B` floats at `(40, 0)` with the
size `40x19`. In an `80x22` tab that holds only the stack `[vim, htop]` with
`vim` open, `koshi undock-pane --pane <htop>` floats `htop` at `(0, 1)` with
the size `72x19`.

`koshi dock-pane` puts a floating pane into a tab's tiled layout. `--tab`
names the tab, and `--direction` the side of the whole tab where the pane
lands, `right` when omitted. Without `--tab`, the pane docks into the first
tab of this list:

1. The tab the `--client` client views.
2. The tab of the tiled pane the command runs in.
3. The tab the issuing client views: the client of the pane the command runs
   in while it is attached, else the session's only attached client.
4. The session's only tab.

When no step gives a tab, the command refuses. For a session `work` with 3
tabs, the refusal is ``session `work` has 3 tabs; name one with --tab or
--stack-onto``. Example: tabs `one` and `two`, and one client that views
`two`. `koshi dock-pane` typed in a floating pane docks it into `two`.

`--stack-onto <PANE_ID>` puts the pane into the stack of that tiled pane as its
last member instead. A tiled pane in no stack forms a new stack with it. A
tiled pane moves to that place, as `place-pane` moves it. The client moves to
the destination tab, and the docked pane takes its input: every floating pane
the client shows is minimized. A tab with no room refuses with `tab <name> has
no room for another pane`. `koshi place-pane --stack-onto <PANE_ID>` puts any
pane, tiled or floating, into that stack.

A pane that `dock-pane` or `place-pane` moves takes along every other client
whose input is on it: that client moves to the pane's tab and keeps typing in
it, and every floating pane it shows is minimized. Every other client keeps
its tab, its input and its floating panes.

Each client shows open, in each stack, the member it focused last there. A
client that focused no member of a stack sees the stack's active member. A
pane that joins a stack opens for another client only when that client
focused it more recently than every other member of the stack. Example: two
clients view the stack `[vim, htop]`, one focused on `vim`, one on `htop`. The
first runs `koshi place-pane --pane <logs> --stack-onto <vim>`: it sees `logs`
open and focused, and the second still sees `htop`.

Example: tab `#1` holds `[S1, S2]` stacked, and `F` floats. `koshi dock-pane
--pane <F> --stack-onto <S2>` gives the stack `[S1, S2, F]`, with `F` open on
the screen of the client that docked it.

`koshi move-pane --pane <PANE_ID> --x <COLUMN> --y <ROW>` moves a floating
pane on one client's screen. A pinned pane is refused with `<pane> is pinned;
unpin it to move it`. `koshi move-pane --direction` on a floating pane is
refused with `<pane> is floating and has no tiled position; use move-pane --x
--y`.

`pin-pane`, `unpin-pane`, `minimize-pane` and `restore-pane` change one
client's screen. Without `--pane`, they act on the pane they run in, or from
outside koshi on the client's focused floating pane. `minimize-pane --all`
minimizes every floating pane the client shows. `restore-pane --all` restores
every floating pane the client minimized, and focuses the last one.

Example: terminals `client-1a2b…` and `client-3c4d…` both show `htop` floating.
`koshi minimize-pane --all --client client-3c4d…` minimizes it on
`client-3c4d…` alone.

`koshi focus-pane --next-floating` shows and focuses the floating pane after
the focused one, in the order the panes became floating, and wraps past the
last to the first. `--previous-floating` goes the other way. With no floating
pane in the session, both are refused with `the session holds no floating
pane`.

## Tabs

| Command | Main flags | Result |
|---|---|---|
| `koshi new-tab` | `--session <NAME_OR_ID>`, `--client <CLIENT_ID>` | Open a tab with one shell pane |
| `koshi close-tab` | `--tab <NAME_OR_ID>`, `--session <NAME_OR_ID>`, `--force` | Close a tab |
| `koshi next-tab` | `--client` | Focus the next tab |
| `koshi previous-tab` | `--client` | Focus the previous tab |
| `koshi focus-tab` | `--index` or `--tab <NAME_OR_ID>`, optional `--client` | Focus one tab |
| `koshi move-tab` | `--index`, optional `--tab <NAME_OR_ID>` | Move one tab to a zero-based index |

`--index` counts from 0. An index past the last tab is refused, and the command
exits 1. The error names the session and the indexes it takes. Example: session
`work` has 3 tabs. `koshi focus-tab --index 9` and `koshi move-tab --index 9`
both print `no target matched`, then ``session `work` has 3 tabs; --index takes
0 to 2``.

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
| `koshi keys list [--mode <MODE>] [--scope default\|user] [--format table\|json]` | List effective shortcuts |
| `koshi keys describe "<KEY_SEQUENCE>"` | Explain one shortcut, or each shortcut it starts |
| `koshi keys conflicts` | Report clashes, dead shortcuts, and warnings |
| `koshi keys validate <PATH>` | Check a shortcut file without applying it |

`keys list --mode` takes the name of one built-in mode, such as `normal` or
`locked`. Another name exits 2 and lists the mode names. `--scope user` lists
the shortcuts in your `keybinding.kdl`. `--scope default` lists the built-in
shortcuts. It also lists each built-in shortcut that your file rebinds or
removes, with the source `defaults (unbound)`.

`keys describe` explains each row that `keys list` prints for the key, and for
each longer key that starts with it. For example, `koshi keys describe "<C-t>"`
explains `<C-t> n` (`core:new-tab`) and `<C-t> x` (`core:close-tab`).

The `continuous` field (`is_continuous` in JSON) is `true` when the action
repeats from its prefix: after the shortcut fires, its prefix stays open, and
the last key alone fires the action again (`keybinding.md`, *Repeating keys*).
For example, `koshi keys describe "<C-p> <Left>"` explains
`core:focus-pane-left` with `continuous: true`, and `<C-p> <Left> <Left>`
focuses left twice.

If your `keybinding.kdl` rebinds or removes a built-in shortcut, `keys
describe` also explains that built-in shortcut, with the source
`defaults (unbound)`. Its `displaced_by` field names what replaced it:
`user bind` or `user remove`. For example, with `remove "<A-f>"` in the
`normal` block, `koshi keys describe "<A-f>"` explains
`core:toggle-pane-fullscreen` with `displaced_by: user remove`, and exits 0.
Each other shortcut shows `displaced_by: -` in a table and `null` in JSON. A
key that has no row and starts none exits 2.

`keys conflicts` checks the `keybinding.kdl` in the config directory, and
`keys validate` checks the file at `PATH`. Each one checks the file the way a
client checks its own `keybinding.kdl` when it attaches. Each one prints the
whole answer, then exits 0 when a client applies the file and 2 when a client
refuses it. A client refuses a file that it cannot read, a file that does not
parse, and a file with a `fatal` finding. It then uses the built-in shortcuts.

For example, a `keybinding.kdl` that holds `version 2` on line 1 and
`unlock_alternative "<A-u>"` on line 2 makes `koshi keys conflicts` print this
answer and exit 2:

```text
file: ignored (line 2, column 1: unknown key `unlock_alternative`; did you mean `unlock-alternative`?)
verdict: reject
```

With no `keybinding.kdl`, `koshi keys conflicts` prints `verdict: apply` and
exits 0.

## Remote access

| Command | Result |
|---|---|
| `koshi share grant <IDENTITY> [--session <SESSION>] [--expires <DURATION>]` | Grant an identity a remote access token |
| `koshi share open` | Open the port remote clients connect to, and print the commands that connect |
| `koshi share revoke <IDENTITY> [--session <SESSION>]` | Revoke the tokens an identity holds |
| `koshi share list [--session <SESSION>] [--format table\|json]` | List the tokens granted on this machine |
| `koshi attach --remote <SERVER> [--certificate-fingerprint <FINGERPRINT>] [--save-as <NAME>] [SESSION]` | Attach to a session on the machine `SERVER` names |
| `koshi list-sessions --remote <SERVER> [--certificate-fingerprint <FINGERPRINT>]` | List the sessions on the machine `SERVER` names |
| `koshi remote new` | Save a server, asking for its name, address, certificate fingerprint and secret |
| `koshi remote edit <SERVER>` | Change one saved server's name, address, certificate fingerprint or secret |
| `koshi remote list [--format table\|json]` | List the servers this machine has saved |
| `koshi remote forget <SERVER>` | Drop one saved server |
| `koshi remote set-secret <SERVER>` | Replace the secret of one saved server |

The four `share` verbs run on the machine holding the sessions. The rest run
on the machine connecting to it.

The `--session` of a `share` verb names one of your own sessions. A session
that another user of this machine started is not found and exits 3, such as
`koshi: session 1001/session-3f2a… is not running`.

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

A grant that was revoked or ran out 30 days ago or longer no longer shows in
`koshi share list`. The next `koshi share grant`, `koshi share revoke`, or
admitted remote connection removes it from the store. A grant revoked on
1 March shows until 31 March.

Two koshi programs never change the tokens at the same time. `koshi share
grant` and `koshi share revoke` wait up to 5 seconds while another koshi
changes them, then stop with:

```text
another koshi is changing the remote access tokens; try again
```

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

A token lasts at most `36500d`, 100 years of 365 days. A longer length is
refused, and no token is granted:

```text
koshi share grant alice --expires 36501d
```

is refused with `a token lasts at most 36500d; use never for a token that does
not expire`. The same refusal comes for every longer length, such as
`99999999999999d`, on Linux, macOS and Windows. Use `--expires never` for a
token that does not expire.

A grant whose expiry falls past the latest time this machine's clock counts is
refused by the router, and no token is granted.

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
other answer leaves it shut, still prints the token, and names `koshi share
open`, which opens the port at any time without a new token. With no address in
`koshi.kdl` there is nothing to offer, and the grant names the line to add.

The router reads `remote-listen` when it starts. When the address it read is
not the one `koshi.kdl` names now, the grant asks nothing. It prints the token,
names both addresses, and names `koshi restart-servers`, as `remote-listen` in
`koshi.md` states:

```text
/home/user/.config/koshi/koshi.kdl names the remote listen address 127.0.0.1:7654, and the running router read no remote listen address from it when it started. Run `koshi restart-servers` so the router reads it again, then run `koshi share open`. This token cannot be used to connect until the port opens.
```

A port the router cannot open prints the router's own reason, and whether
remote access is on:

```text
remote access stays off, and the port did not open: the remote listener could not open 192.0.2.20:7654: Address already in use (os error 48). Run `koshi share open` once that is fixed. This token cannot be used to connect until the port opens.
```

A token stands from the moment it is minted, whatever the port does. It
connects as soon as the port opens.

With the port open, the grant prints the command that connects to this
machine. Each command carries `--certificate-fingerprint` with the fingerprint
of the certificate this machine's listener presents: the sha256 of it, as
`sha256:` followed by 64 lowercase hex characters. It also carries `--save-as`
with this machine's host name, the name the other machine saves this one
under. A host name that does not start with a letter or digit, holds a
character other than ASCII letters, digits, `.`, `-` and `_`, has a
dot-separated part `localhost`, or is 64 hex characters gives no `--save-as`,
and the other machine saves this one with no name. The address in that
command depends on `remote-listen`:

- `0.0.0.0:7654` gives one command for each IPv4 address of this machine, and
  `[::]:7654` gives one for each IPv6 address. Koshi lists the addresses of each
  network interface that is up. It leaves out loopback and link-local
  addresses, and puts first the address this machine sends to the internet
  from. Each command ends with the name of its interface. If this machine has no
  such address, the command shows `<this machine's address>` in place of one.
- One address of this machine, such as `192.0.2.20:7654`, gives one command
  with that address. A line then says that this machine accepts connections on
  that address only.
- A loopback address, such as `127.0.0.1:7654`, gives one command with that
  address, under `connect from this machine:`. A line then says that no other
  machine can connect to it, and names `0.0.0.0:7654` as a `remote-listen`
  value that lets another machine connect after `koshi restart-servers`.

The address is the one the router listens on. When `koshi.kdl` names another
address now, a last line names both, and `koshi restart-servers`.

An IPv4-mapped IPv6 address counts as its IPv4 form: `[::ffff:192.0.2.20]:7654`
gives the same command as `192.0.2.20:7654`.

Example, on a machine with the host name `desk` that listens on `0.0.0.0:7654`:

```text
connect from another machine, at the address of this one it can reach:
  koshi attach --remote 192.0.2.20:7654 --certificate-fingerprint sha256:3f6a0c2e9b8d4f17a5c3e1d0b9f8a7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0 --save-as desk [SESSION]   # en0
  koshi attach --remote 198.51.100.2:7654 --certificate-fingerprint sha256:3f6a0c2e9b8d4f17a5c3e1d0b9f8a7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0 --save-as desk [SESSION]   # utun3
set KOSHI_REMOTE_SECRET to the secret above, or paste it when asked.
```

Use the command with an address the other machine can reach. For example, a
laptop on the same Wi-Fi uses the `en0` address, and a laptop on a VPN uses the
`utun3` address.

`koshi share open` opens the port without a question and mints no token. It
prints the same connect commands as a grant, with the same fingerprint and the
same `--save-as`. Its secret line names the secret `koshi share grant` printed in
place of the secret above. On a port that is already open it
opens nothing and prints the commands again, so it is also how to read the
fingerprint again. A port that does not open prints the reason on standard
error, as the grant does, and the command exits 1. Example, after a grant whose
`y` was not typed:

```text
koshi share open
connect from another machine:
  koshi attach --remote 192.0.2.20:7654 --certificate-fingerprint sha256:3f6a0c2e9b8d4f17a5c3e1d0b9f8a7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0 --save-as desk [SESSION]
set KOSHI_REMOTE_SECRET to the secret `koshi share grant` printed, or paste it when asked.
this machine accepts connections on 192.0.2.20 only; the other machine must connect to that address.
```

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
| `last_used` | When a presented token last reached a session through this grant. The time is written to the store right after the connection opens |
| `revoked` | When an operator stopped the grant |

A time prints as RFC 3339 UTC text, such as `2025-01-01T00:00:00Z`, in the
table and in JSON. An absent value prints as `-` in a table and `null` in JSON.
`--format json` writes `scope` as `"host"` or the session id.

### Connecting to another machine

`--remote` names the machine an invocation talks to, by the `host:port` it
listens on or the name it was saved under. Everything after that — how a
session is named, how a missing name is resolved, what is refused — runs
against that machine unchanged.

The first connection to a server names its address and the fingerprint of its
certificate, and `--save-as` gives it a short name:

```text
koshi attach --remote laptop.local:7654 --certificate-fingerprint sha256:3f6a0c2e9b8d4f17a5c3e1d0b9f8a7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0 --save-as work web
```

After that the name stands in for the address and the fingerprint, and
nothing is retyped:

```text
koshi attach --remote work web
```

The secret never appears on a command line. koshi reads it from the
environment variable `KOSHI_REMOTE_SECRET`, and with that unset asks for it at
the terminal without printing what is typed. No flag takes a secret.

The fingerprint is the sha256 of the server's certificate, written as `sha256:`
followed by 64 lowercase hex characters. `koshi share grant` and `koshi share
open` on the serving machine print it in each connect command, `koshi remote
list` prints it in that form, and `--certificate-fingerprint` takes only that
form. A secret is 64 lowercase hex characters with nothing in front, so a
secret given where the fingerprint goes is refused, and the refusal does not
repeat it:

```text
koshi: invalid arguments: a certificate fingerprint is sha256: followed by 64 lowercase hex characters, as `koshi share grant` prints it on the machine that serves. 64 hex characters alone is the shape of a secret, and a secret is never given here
```

Both machines speak TLS 1.3 only, and a peer that offers
TLS 1.2 alone is refused during the handshake.

koshi 0.5.0 and older print no fingerprint, so a first connection from this
koshi to a machine that runs one of them has no fingerprint to give. Update
koshi on that machine first; its `koshi share grant` then prints the
fingerprint. koshi 0.5.0 and older also refuse `--certificate-fingerprint` as
an unknown argument. On a machine that runs one of them, leave the flag out
of the connect command: that koshi saves the certificate the server presents
on its first connection.

koshi compares the fingerprint during the TLS handshake, before the secret is
sent. If the server presents another certificate, the connection is refused,
the secret is not sent, and the refusal names the address and both
fingerprints. A first connection, and the check that `koshi remote new` and
`koshi remote edit` run, end in both ways to give the new fingerprint:

```text
koshi: IPC unavailable: the certificate laptop.local:7654 presented is not the expected one: expected sha256:3f6a0c2e9b8d4f17a5c3e1d0b9f8a7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0, presented sha256:0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c. the secret was not sent. if that machine's certificate changed on purpose, `koshi share open` there prints the new fingerprint: give it with --certificate-fingerprint, or with `koshi remote edit` for a saved server
```

A connection to a saved server names that server, by its name or else its
address:

```text
koshi: IPC unavailable: the certificate laptop.local:7654 presented is not the expected one: expected sha256:3f6a0c2e9b8d4f17a5c3e1d0b9f8a7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0, presented sha256:0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c. the secret was not sent. if that machine's certificate changed on purpose, `koshi share open` there prints the new fingerprint: give it with `koshi remote edit work`
```

An address that is not saved needs `--certificate-fingerprint`. Without it,
the connection is refused and nothing is dialled:

```text
koshi: invalid arguments: laptop.local:7654 is not saved, so the fingerprint of its certificate is needed: add `--certificate-fingerprint <FINGERPRINT>` with the fingerprint `koshi share grant` printed on that machine
```

A saved server takes `--certificate-fingerprint` when it gives the
fingerprint the server was saved with, and the connection checks that
fingerprint. Any other fingerprint is refused before a connection opens:

```text
koshi: invalid arguments: work expects the certificate fingerprint sha256:3f6a0c2e9b8d4f17a5c3e1d0b9f8a7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0, and --certificate-fingerprint gives sha256:0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c; run `koshi remote edit work` to change the fingerprint it expects
```

`koshi remote edit <SERVER>` changes the fingerprint a saved server expects.
If the serving machine's certificate changes on purpose, `koshi share open`
there prints the new fingerprint, and `koshi remote edit` stores it.

A serving machine closes a connection before its TLS handshake finishes when
its koshi restarts or stops, when 64 other connections wait to present a
secret, and when the address the connection comes from opened 10 connections
in 60 seconds that were not admitted. A connection whose secret is
admitted does not count toward the 10. The connecting side cannot tell these
cases apart, nor tell them from another program on that port that closes the
connection. The refusal names all of them:

```text
koshi: IPC unavailable: the connection to laptop.local:7654 closed during the TLS handshake. a koshi server closes a connection this way when it restarts or stops, when 64 others wait to present a secret, and when the address it comes from opened 10 that were not admitted in 60 seconds. dial again in 60 seconds. if it keeps happening, run `koshi doctor` on that machine to check that koshi serves that port
```

A first connection from any command that takes `--remote`, with
`koshi list-sessions` included, saves the address, the secret and the
fingerprint. `koshi attach` also saves the name that `--save-as` gives. A
first connection with no `--save-as` saves the server with no name, and
`koshi remote list` prints `-` for that name. Then
`koshi attach --remote <address> --save-as <NAME>` gives that server the name
once the server admits the connection:

```text
koshi list-sessions --remote laptop.local:7654 --certificate-fingerprint sha256:3f6a0c2e9b8d4f17a5c3e1d0b9f8a7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0
koshi attach --remote laptop.local:7654 --save-as work
```

A saved server takes `--save-as` with the name it has, and refuses another
name; `koshi remote edit <SERVER>` changes its name. The connect command that
`koshi share grant` prints runs again with the same result.

The store lives on the connecting machine and is readable only by its owner.
`koshi remote list` prints the name, address, fingerprint and last-used time of
each saved server, and never a secret. Once the serving machine grants a fresh
secret, `koshi remote set-secret <SERVER>` replaces the saved one; it reads the
new secret the same way a connection does.

A saved server presents the secret it saved. If `KOSHI_REMOTE_SECRET` gives a
secret that no saved server holds, a `--remote` connection to a saved server
is refused before it opens, and the saved secret stays:

```text
koshi: invalid arguments: work holds a saved secret, and KOSHI_REMOTE_SECRET gives one that no saved server holds; run `koshi remote set-secret work` to replace the saved secret with it, or unset KOSHI_REMOTE_SECRET to dial with the saved one
```

`koshi remote set-secret work` reads that secret from `KOSHI_REMOTE_SECRET`
and saves it. A value that a saved server holds, an empty value, and an unset
variable connect with the secret the named server saved. A bare
`koshi list-sessions` and a bare `koshi attach` present each saved server's own
secret, and do not read `KOSHI_REMOTE_SECRET`.

`koshi remote new` saves a server without attaching to one of its sessions. It
asks four questions in turn — the name, the address, the fingerprint of the
server's certificate, and the secret — and every answer is needed. It then
dials the server once to check that it presents that certificate and admits
the secret:

```text
$ koshi remote new
every answer is needed. Ctrl-C stops without saving.
name: work
address: laptop.local:7654
fingerprint: sha256:3f6a0c2e9b8d4f17a5c3e1d0b9f8a7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0
secret:
checking laptop.local:7654 …
saved work at laptop.local:7654.
```

A fingerprint that is not `sha256:` followed by 64 lowercase hex characters is
refused, and the question is asked again:

```text
fingerprint: 3F6A
koshi: invalid arguments: a certificate fingerprint is sha256: followed by 64 lowercase hex characters, as `koshi share grant` prints it on the machine that serves
fingerprint:
```

A refused answer is never printed back. An address that is not `host:port`
reads `koshi: invalid arguments: an address is host:port, such as
laptop.local:7654`. A name or a fingerprint of 64 hex characters alone has the
shape of a secret and is refused, at the questions and at `--save-as`:

```text
name: f00df00df00df00df00df00df00df00df00df00df00df00df00df00df00df00d
koshi: invalid arguments: a saved name must not be 64 hex characters, the shape of a secret. Pick a plain name.
name:
```

A server that does not pass the check is named, and the last question is
whether to save what was typed anyway. A server saved that way holds the
fingerprint that was typed:

```text
checking laptop.local:7654 …
koshi: IPC unavailable: laptop.local:7654 refused the connection: nothing is listening on that port. if remote access is not enabled on that machine, run `koshi share open` there to open the port
save it anyway? [y/N]: y
saved work at laptop.local:7654.
```

Answering anything else prints `nothing was saved.` and writes nothing.

`koshi remote edit <SERVER>` asks the same four questions with the saved
values in brackets. An empty answer keeps the value in brackets, and an empty
secret keeps the saved secret, so only what changes is typed:

```text
$ koshi remote edit work
press Enter to keep the value in brackets. An empty secret keeps the saved one. Ctrl-C stops without saving.
name [work]:
address [laptop.local:7654]: laptop.local:7655
fingerprint [sha256:3f6a0c2e9b8d4f17a5c3e1d0b9f8a7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0]:
secret:
checking laptop.local:7655 …
updated work at laptop.local:7655.
```

A saved server that holds no fingerprint offers none in brackets, and the
question is asked until a fingerprint is typed.

The check requires the fingerprint the answers settled on. The saved server
holds that fingerprint when the check passes, and also when the change is
saved anyway. If the check does not pass, the last question is
`save the change anyway? [y/N]`.

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

A saved server that holds no certificate fingerprint is never dialled. The
sweep that a bare `koshi list-sessions` and a bare `koshi attach` make over
every saved server leaves it out, and names it on one stderr line together
with the command that gives it a fingerprint:

```text
koshi: desk holds no certificate fingerprint, so it is not dialled; run `koshi remote edit desk` and give the fingerprint `koshi share grant` printed on that machine
```

`--remote desk` is refused with the same sentence. Once
`koshi remote edit desk` stores the fingerprint, the sweep includes it.

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
inside that wait is left out, and one line names it:

```text
koshi: work did not answer; its sessions are not listed
```

A server whose connection fails inside that wait is left out too. Its line
carries the sentence that `koshi attach --remote work` prints for the same
failure, which names what to do. The failure can be a refusal, a certificate
that is not the expected one, a port where nothing listens, or a connection
closed during the TLS handshake:

```text
koshi: work: the server 192.0.2.10:7654 did not admit the connection: the token expired, was revoked, or was never granted on that machine, or that machine already serves as many remote connections as it admits. `koshi share list` on that machine shows when each grant expires or was revoked. to replace the token, run `koshi share grant` there, then store the new secret with `koshi remote set-secret work`; its sessions are not listed
```

The serving machine sends the same refusal for each of these causes, and the
sentence names every one. A serving machine that runs koshi 0.5.0 or older
sends a refusal that does not say whether it read the request. For that
machine the sentence also names koshi 0.3.0 and 0.4.0, which cannot read it,
and ends with `if that machine runs koshi 0.3.0 or 0.4.0, update koshi there`.

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

`--certificate-fingerprint` goes with `--remote` on each of those verbs, for a
server this machine has not saved. Without `--remote`, koshi stops with a usage
error that names `--remote <SERVER>`.

`--remote` takes a session id or name. A session named `<user id>/<session id>`
is a session in the shared directory of this machine, and `--remote` refuses it
with exit code 2 before it dials anything, such as `koshi: invalid arguments:
--remote takes a session id or name, not `1001/session-3f2a…`, which names a
folder of this machine's shared directory`.

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

### Copies made in a pane

With `pane-clipboard-writes { allow #true }` in the `koshi.kdl` of the machine
holding the session, a copy that a program in a pane makes reaches the
clipboard of a terminal attached from another machine:

- an `OSC 52` copy goes to the terminal of every client whose focused pane is
  that pane;
- on Linux and macOS, a copy made with `pbcopy`, `xclip`, `xsel` or `wl-copy`
  goes to the terminal of each client on another machine whose focused pane is
  that pane.

Example: `printf hi | xclip -selection clipboard` in a pane that you focus
from your laptop with `koshi attach --remote` puts `hi` on your laptop's
clipboard. The setting is off by default. [`koshi.kdl`](koshi.md#pane-clipboard-writes)
lists every copy it covers and every copy it does not.

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
new binary, up to 16 session servers replace their own images at the same
time, and then the router restarts into it. Until every swap lands, the program
your shell runs is a newer build than the process answering it:

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
| `unreachable` | The server could not be asked, the session is restarting, the router did not answer within 5 seconds, or a session did not answer within 5 seconds after the router was checked; the reason prints on standard error |

`--format json` writes each row's `state`: `"running"` with the build in
`version`, `"not running"`, or `"unreachable"` with the reason in `detail`. A
server too old to name its build is `"running"` with `"version": null`.

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
check               verdict  reason                                                                                                                              help
config              ok       3 config files validated                                                                                                            -
shell               ok       a new pane runs /bin/zsh                                                                                                            -
terminal            warn     TERM is not set                                                                                                                     set TERM before running koshi, for example TERM=xterm-256color
runtime directory   ok       /tmp/koshi-1000 is ready; koshi names it after your user id                                                                         -
log directory       ok       /home/you/.local/state/koshi/logs is writable and logging is off                                                                    -
session log file    ok       not inside a koshi pane; each session writes koshi-log-<session-id>.log in the log directory, and koshi list-sessions names the id  -
router log file     ok       the router writes /home/you/.local/state/koshi/logs/koshi-log-router.log                                                            -
router              ok       no koshi is running                                                                                                                 -
session directory   ok       sessions are advertised in /tmp/koshi-1000 (mode 700), which only you may reach                                                     -
remote access       ok       koshi.kdl names no remote listen address, and this machine holds 0 standing grants                                                  -
remote listener     ok       no koshi is running, so nothing is listening                                                                                        -
remote connections  ok       no koshi is running, so nothing from another machine is connected                                                                   -
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
| `session log file` | Inside a pane, the log file of that pane's session, read from `KOSHI_SESSION_ID`; outside one, the `koshi-log-<session-id>.log` name |
| `router log file` | The log file of the router, `koshi-log-router.log` in the log directory |
| `router` | Whether a router answers on its control socket within 5 seconds |
| `session directory` | Where sessions are advertised, and who may reach that directory |
| `remote access` | `koshi.kdl`'s remote listen address, and how many access grants still stand |
| `remote listener` | The address the running router read when it started, whether it serves on it, the fingerprint of the certificate it presents, and whether `koshi.kdl` names that address now |
| `remote connections` | How many open connections the running router holds from another machine |

The `router log file`, `session directory` and `remote connections` rows
report facts and rate nothing. The `session log file` row reads `warn` when
`KOSHI` is set and the rest of the pane identity is missing or malformed. The `remote access` row rates one thing: it reads `warn` when the
grants could not be read. A format `1` grant file is counted without changing
it; the router converts it when it starts. `koshi doctor` starts no koshi and
creates no directory. The `log directory` row writes one empty file in the log
directory and removes it again, which is how it reports whether that directory
can be written.

The `router` row rates whether the running router answers. A router whose
build has no such question is `warn`, and its help reads `run: koshi
restart-servers`. A router that koshi 0.2.0 to 0.4.0 started is `fail`: `the
running router runs koshi 0.4.0 or older, which this koshi cannot talk to`,
and its help reads `run: koshi restart-servers`. Any other router that is
listening and does not answer is `fail`. In each of these cases the `remote
listener` and `remote connections` rows read `the running router did not
answer, so this is not known`.

The `remote listener` row rates the remote listener of the router that
answered. It never reads `fail`. Its reasons:

| Reason | Verdict | Help |
|---|---|---|
| `the router serves 0.0.0.0:7654 with the certificate fingerprint sha256:3f6a…` | `ok` | - |
| ``remote access is off, so nothing is listening; `koshi share open` opens 0.0.0.0:7654`` | `ok` | - |
| `the router read no remote listen address when it started, so nothing is listening` | `ok` | - |
| `remote access is on, and nothing is listening on 0.0.0.0:7654: the router could not open it when it started` | `warn` | `run: koshi share open` |
| `the router serves 0.0.0.0:7654 and names no certificate fingerprint`, from a koshi 0.5.0 router | `warn` | `run: koshi restart-servers` |

The fingerprint is the one `koshi share grant` prints in its connect commands,
so the row reads it again without a new grant. When `koshi.kdl` names another
address than the one the router read when it started, the row is `warn`. Its
reason names the file and both addresses, after the fingerprint while the
router serves. Example: `/home/user/.config/koshi/koshi.kdl names the remote
listen address 127.0.0.1:7654, and the running router read no remote listen
address from it when it started`, with the help `run: koshi restart-servers`.
When the router reads another `koshi.kdl` than `koshi doctor`, the reason
names both files, and the help reads `set remote-listen in <the router's
file>, then run: koshi restart-servers`.

A row whose `reason` is shortened to fit the table carries the whole text in a
`detail` field, which `--format json` prints and the table leaves out. Every
other row has `"detail": null`.

### The runtime directory

koshi keeps its router socket and its session sockets in one directory per
user. On Linux and macOS that directory is `/tmp/koshi-<your user id>`, built
from your user id and nothing else. On Windows it is `run` under your
application data directory.

On Linux and macOS, a session whose `pane-clipboard-writes` allows writes also
keeps its folder of stand-in clipboard tools there:
`session-<uuid>.clipboard-tools`. The folder goes when the session ends.

On Linux and macOS, while the router and each session server run, each one
holds a shared `flock` on the directory, and every hour sets the access time of
each file it keeps there to 30 days after the clock. Modification times do not
change. A session that other users of the machine may reach does the same for
its socket and its folder under the shared sessions directory. Example: on
1 March, `ls -lu /tmp/koshi-1000` lists `router.json` with an access date of
31 March.

`KOSHI_RUNTIME_DIR` names the directory instead, and koshi reads it only when
it holds an absolute path. A relative value is ignored. Two koshi processes
holding different values use different directories and do not find each other.

`koshi doctor` prints the directory in use and the rule that produced it.

## Debugging

| Command | Result |
|---|---|
| `koshi debug dump-state [--format table\|json]` | Print every running session's sessions, tabs, panes, and clients |
| `koshi debug dump-layout [--tab <NAME_OR_ID>] [--format table\|json]` | Print each tab's split tree, solved rectangles, panes with no room, stacks, each floating pane with what each client draws for it, and per-client focus |
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

`dump-layout` prints the floating panes after the tabs, in the order the panes
became floating. Each pane line names the size the pane asks for and the size
it holds. Under it, one line per attached client says what that client draws:

- `rect <x>,<y> <cols>x<rows> layer <n>`: the client draws the pane there.
  Layer `0` draws first, and a higher layer draws over a lower one.
- `minimized`: the client holds the pane minimized.
- `no floating viewport`: the client shows the pane and draws nothing. Its
  pane area is smaller than `5x6`, or it has no room to draw a pane.

Each client line ends with the position that client stored: `default`,
`moved <x>,<y>`, or `pinned <x>,<y>`. A session with no floating pane prints
no floating section. `--tab` narrows the tabs only, and every floating pane
still prints: a floating pane draws over every tab.

Example: one 80x24 terminal shows a floating pane that `koshi new-pane
--floating` opened. The pane prints as `pane pane-… asks 60%x60% size 48x13`,
and the client line under it prints as `client client-… rect 16,4 48x13 layer 0
position default`.

Each line under a split ends with how that child is sized, in the words a
layout file uses: `weight 2`, `size 30` for 30 cells, or `size 60%`, then
`min <cells>` and `preferred <cells>` when set, then `resize <cells>` after a
user resize. Example: `pane pane-…, weight 2 min 10 resize +5`. A split whose
weights do not number one per child says so: `horizontal split, 3 weights for
2 children`.

`dump-layout --format json` writes the same words as fields. Each tree node
has `"node": "pane"` or `"node": "split"`, and a split has `split_direction`
(`horizontal`, `vertical` or `stacked`) and `weights`, one sizing per child in
the words above. Each client solve has `layout_mode`
(`tiled`, or `fullscreen` with `fullscreen_pane_id`). Each client view of a
floating pane has `position` (`default`, or `moved` or `pinned` with
`stored_cell`) and `visibility` (`drawn` with `top_left_cell` and
`layer_index`, `minimized`, or `no floating viewport`). `desired_size` writes
each axis as `--cols` and `--rows` take it: `"60%"` or `"80"`.

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
`7d`. A length over `18446744073709551615s` is refused with `a length is at
most 18446744073709551615s`. `--filter` keeps the events whose name contains
the text given, matched ignoring case, so `--filter pane` keeps `PaneCreated`
and `PaneFocused`. An empty `--filter` is a usage error.

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
