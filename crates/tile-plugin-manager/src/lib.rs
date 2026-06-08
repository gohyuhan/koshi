//! `tile-plugin-manager` — plugin lifecycle manager: install, uninstall, enable,
//! disable, update, list, metadata index, lockfile, registry resolution, local
//! file sources, integrity checks, and plugin store layout. Owns plugin
//! inventory state. Must NOT depend on `tile-runtime` or `tile-ipc`.
