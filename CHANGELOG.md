# Changelog

Notable user-facing changes are recorded here.

## Unreleased

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
