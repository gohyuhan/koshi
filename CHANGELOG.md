# Changelog

Notable user-facing changes are recorded here.

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
