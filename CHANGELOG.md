# Changelog

Notable user-facing changes are recorded here.

## v0.4.0

- Programs in a pane can draw images. Koshi decodes Sixel, Kitty graphics protocol, and iTerm2 inline images, keeps each image at its cell placement through scrolling, scrollback, resizing, and screen switches, and paints it in every attached terminal that can show images. A terminal that cannot show images sees `terminal image unavailable` in the cells the image covers.
- `image-support` in `koshi.kdl` is read by each terminal for itself. `#true`, the default, probes the terminal for Kitty, iTerm2, or Sixel output and paints images with the protocol it finds. `#false` keeps the text and the placeholders and sends no image bytes.
- A terminal that does not answer the cell-size query still places Sixel and iTerm2 images at the right scale, using its window pixel size instead.
- `pane { gap N }` in `koshi.kdl` leaves `N` blank cells between two panes that meet along a horizontal or vertical split. Every terminal attached to the same session sees the same gaps. Stacked panes stay contiguous. Dragging a pane border with the mouse still works across a gap.
- A shell that prints OSC 133 prompt markers gives koshi the start and end of every command it runs. Koshi keeps the prompt marks on their rows through scrolling, scrollback, resizing, and reflow, and publishes `PaneCommandStarted` and `PaneCommandFinished` events that name the pane and nothing else, never the command line or its output.
- `koshi debug events [--since <LENGTH>] [--filter <NAME>]` prints the last 1000 events each running session published, oldest first. A line names when the event was stamped, which event it was, and the ids it named. A keystroke prints as `PaneTyped` with its ids, never as the character typed. A session started by an older koshi keeps no such buffer, and the command says so.
- A bare `lock` node in a profile starts the session's first terminal with input locked, so keys go straight to the program in the focused pane until `koshi unlock`, `koshi toggle-lock`, or `<C-l>` releases it.
- `koshi new-tab` opens a tab with one shell pane, and `--client` names the terminal that switches onto it.
- `koshi toggle-pane-fullscreen [--client <CLIENT_ID>]` zooms or restores the focused pane of one terminal, and leaves every other terminal of the session tiled.
- Each terminal measures the area it draws panes into and reports it. The session sizes the shared programs of a tab from the smallest reported area, so a viewer whose tab bar or shortcut bar is a different height no longer gets a wrong grid. A terminal with no room stays connected, shows the too-small notice, and is left out of the calculation. `koshi inspect client` and `koshi debug dump-state` gain a `pane_area` column showing `-`, `starving`, or a size like `80x22`.
- Drawing, cursor placement, and mouse routing use one committed geometry per painted frame, so a click lands on the pane it appears to hit. A frame that fails to paint leaves the previous screen, mode, hints, title, cursor, and selection in place.
- `Shift`+drag selects text in a pane whose program owns the mouse, without entering mouse selection mode.
- Remote connections agree on a hybrid post-quantum key exchange, `X25519MLKEM768`, in place of classical `X25519` alone. An older koshi that offers only classical methods is still accepted. Certificates, pinning, and timeouts are unchanged.
- A pane title is cut to 512 bytes and stripped of control characters, right-to-left overrides, invisible tag characters, and other characters that cannot be safely drawn, before it reaches your screen, the other attached terminals, or the terminal title bar. Session and tab names arriving from another machine are filtered where they are printed and kept exactly as sent where they are matched.
- The saved-session file this build writes is format 3. A 0.3.0 build refuses to read it, so `koshi update` from 0.3.0 restarts each running session into 0.4.0, and a `koshi update` that would move a session back to 0.3.0 is refused and leaves that session on 0.4.0.

## v0.3.0

- `koshi attach --remote <server>` attaches this terminal to a session on another machine, over TLS.
- The first connection to a machine records the certificate it presents and pins it; a later connection presenting a different certificate is refused and names both fingerprints.
- `remote-listen` in `koshi.kdl` names the address the remote listener binds. Writing that line opens no port: the port opens the first time `koshi share grant` runs and you answer yes to the offer, and on every start after that.
- `koshi share grant`, `koshi share revoke`, and `koshi share list` hand out, stop, and list the access tokens that reach the sessions on this machine. A grant reaches every session or one named session, prints its token once, and stops working after `24h` unless `--expires` says otherwise.
- `koshi remote new`, `edit`, `list`, `forget`, and `set-secret` keep the servers this machine dials. `new` and `edit` ask for the name, the address, and the secret in turn, then dial the server to check the secret before saving.
- A secret never appears on a command line: koshi reads `KOSHI_REMOTE_SECRET`, and with that unset asks for it at the terminal without printing what is typed.
- `koshi list-sessions` also lists the sessions of every saved server that answers, each row naming its server in a new `server` column; `--remote <server>` lists that one server's sessions.
- Bare `koshi attach` offers the sessions on the saved servers beside this machine's own, and asks which one to attach to whenever a session on a server is listed.
- `remote-reconnect` dials a dropped remote link again — after 1, 2, 4, 8 seconds, and 8 before every dial after that, for up to 120 seconds — while the tab strip counts down to the next dial. Joining again restores the active tab, each tab's focused and fullscreened pane, and each pane's scroll position.
- A remote refusal no dial can change stops the dialing at once and names itself: a certificate that is not the pinned one, a token the server does not admit, a token that does not reach the session, or two builds sharing no protocol version.
- `koshi share` verbs run inside a koshi pane are refused while anyone is attached to that pane's session from another machine, so a token never prints onto a screen someone else reads.
- `allow-other-users` lets the other users of this machine list, attach to, and kill your sessions, and `shared-sessions-dir` names where those session sockets live. `koshi --headless --allow-other-users` opens one session that way for its whole life.
- `koshi update` restarts the running koshi servers into the new release: each session replaces its own program while keeping its panes, the programs in them, and their scrollback, and the process that tracks sessions restarts after them. A session that refuses the restart is named on standard error and keeps the old build.
- `koshi server-version` prints the build every running koshi server runs, one row per server, so a session still on the old build shows beside the ones that moved.
- `koshi doctor` checks this machine's installation — config, shell, terminal, runtime directory, log directory, plugins directory, router, session directory, remote access, and remote connections — and rates each row `ok`, `warn`, or `fail`.
- Sessions are advertised in `/tmp/koshi-<your user id>` on Linux and macOS, named after your user id alone, so every shell finds the same sessions. `KOSHI_RUNTIME_DIR` names another directory when it holds an absolute path.

## v0.2.0

- Sessions run in their own process and keep running after the terminal leaves.
- `koshi attach` joins a running session; without a name it lists the running ones to pick from.
- `koshi detach` leaves a session without ending it; `--all` detaches every terminal of one session.
- `koshi --headless` starts a session with no terminal attached and prints its id.
- `koshi attach <session>` typed inside a pane moves that terminal to another session.
- Several terminals can view one session at once, each with its own focus and view.
- A terminal that missed an event resyncs instead of drawing a stale view.
- The quit shortcut leaves the session running instead of ending it.
- `auto-close-session` ends a session once its last terminal leaves.
- Each terminal reads its own theme, keybindings, and view settings from its own files.
- Sessions and tabs accept their printed id anywhere their name is accepted, and an id is never re-read as a name.
- `koshi debug dump-state` and `koshi debug dump-layout` print running state and solved layouts.
- A panic writes `crash-<timestamp>.txt` to the data directory after restoring the terminal.
- Terminal reset and custom tab stops.
- A koshi that cannot speak another koshi's protocol version refuses and names both version ranges.
- The session server starts with no console window on Windows.
- Dragging a pane border moves it the whole drag distance in one step instead of one cell at a time.
- The shortcut hint bar updates without waiting for the next frame.
- A stop request that fails skips the wait and ends the process at once.
- `allow-beta-features` runs features still marked beta; this release marks none.
- Smaller cell storage, cheaper mouse movement, and lower scrollback memory.

## v0.1.0

- Split, stacked, fullscreen, focus, resize, and close operations for panes.
- Tab creation, closing, movement, and switching.
- Per-pane processes, terminal screens, scrollback, and selections.
- True color, text styles, alternate screens, wide characters, emoji, and box drawing.
- Mouse focus, border resizing, scrolling, text selection, and OSC 52 clipboard copy.
- Multi-key shortcuts, configurable leader keys, shortcut hints, and conflict checks.
- Locked input mode and mouse selection mode.
- KDL files for app settings, themes, keybindings, and saved layouts.
- Twenty-five ready-made themes.
- Saved profiles with tabs, pane layouts, commands, working directories, and environment values.
- Local config path, explanation, validation, and migration commands.
- Session, tab, pane, and client discovery commands.
- External pane, tab, input, focus, lock, and session control commands.
- Per-session text or JSON logging.
- Startup update checks, explicit self-update, and release installers.
- Linux, macOS, and Windows support on x86-64 and ARM64.
- Input aimed at an open shortcut stays in Koshi until that shortcut completes or is cancelled.
- Pane input reaches only panes visible to the issuing client.
- New panes inherit the issuing terminal's working directory when no directory is given.
- Config files require an explicit schema version.
- Config migration validates the source and every version step before accepting output.
