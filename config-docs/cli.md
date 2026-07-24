# koshi command line

This page lists commands that work now. Run `koshi <command> --help` for every
flag and accepted value.

## Starting koshi

| Command | Result |
|---|---|
| `koshi` | Open one session, tab, and shell pane |
| `koshi --profile <NAME>` | Open `profile/<NAME>.kdl` |
| `koshi update` | Check for and install the latest release |

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

`check` and `migrate` scan `koshi.kdl`, `keybinding.kdl`, every
`themes/*.kdl`, and every `profile/*.kdl`. Migration does not repair bad KDL or
bad fields. Example: valid version 1 + current version 3 results in 1 → 2 → 3;
invalid version 1 stops with no config file written.

Each matching config path must be a regular file. Both commands report all
read and schema errors found before migration writes anything. A symbolic link
to a regular file stays a link; migration updates its target.

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
or printed id. Pane and client flags use printed ids.

## Created ids

Create commands print ids on stdout in creation order:

```text
koshi new-pane
[PANE ID]: pane-<uuid>

koshi new-tab
[TAB ID]: tab-<uuid>
[PANE ID]: pane-<uuid>
```

`koshi run -- htop` prints one pane id. Commands that create nothing print no
id line.

## Sessions and discovery

| Command | Result |
|---|---|
| `koshi list-sessions` | List session ids and names |
| `koshi kill-session [NAME]` | End the named session, or the only running one |
| `koshi list-tabs [--session <SESSION_ID>]` | List tab ids, names, and owning sessions |
| `koshi list-panes [--session <SESSION_ID>]` | List pane, tab, and session ids and names |
| `koshi list-clients [--session <SESSION_ID>]` | List client ids and owning sessions |
| `koshi inspect session <SESSION_ID>` | Show one session's full record |
| `koshi inspect tab <TAB_ID>` | Show one tab's full record |
| `koshi inspect pane <PANE_ID>` | Show one pane's full record |
| `koshi inspect client <CLIENT_ID>` | Show one client's full record |

Every list and inspect command accepts `--format table` or `--format json`.
Table is the default.

`kill-session` matches the generated session name exactly. With no name, it
works only when exactly one session is running. An unknown name exits 3; an
unreachable control socket exits 4.

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
| `koshi focus-tab` | `--index` or `--tab`, optional `--client` | Focus one tab |
| `koshi move-tab` | `--index`, optional `--tab` | Move one tab to a zero-based index |

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
