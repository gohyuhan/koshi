# Findings — pre-0.3.0 quality sweep

Origin: the behavior-frozen simplify/optimize/docs/tests pass over the whole
workspace, 2026-08-20. The sweep changed no behavior; everything below is what
it found and deliberately left. Each item is self-contained: a fresh session
can pick one up without re-discovery.

State of the tree when this file was written: `cargo fmt --check`,
`cargo clippy --workspace --all-targets`, `cargo test --workspace`
(4985 passed / 0 failed / 1 ignored), `cargo doc --workspace --no-deps` — all
exit 0. All runs macOS; Linux/Windows ride on the CI matrix.
`crates/koshi/src/prompt.rs` is new and untracked — `git add` it before commit.

Status legend: **DONE** (fixed), **CLOSED** (checked and deliberately left,
with the reason), **DROPPED** (checked, not real). Every item was decided on
2026-08-20; the owner authorized tackling all of them in one pass.

---

## A — Bugs (fixing changes behavior; each needs an explicit go)

### A1. `<A-S-Tab>` drops the Alt modifier — DONE (ESC prefix; the kitty keyboard protocol planned for 0.5.0 replaces this encoder)
- Where: `crates/koshi-input/src/keyboard.rs:200`.
- What: the `NamedKey::Tab if mods.contains(SHIFT)` arm returns `ESC [ Z`
  with no Alt `ESC` prefix. `<A-Tab>` correctly produces `ESC \t`.
- Reference behavior: xterm emits a modified back-tab as `CSI 1;<param> Z`.
- Example: press `<A-S-Tab>` in a pane → the child receives plain `ESC [ Z`,
  identical to `<S-Tab>`; the Alt is lost.
- Fix choice: (a) prefix the existing `ESC [ Z` with `ESC`, or (b) emit the
  xterm form `CSI 1;<param> Z` for every modified back-tab. (b) is the
  spec-shaped answer; verify against xterm source before choosing.
- Owner lane: VTE/keyboard — assistant owns end-to-end once the form is picked.

### A2. `foreign_sessions` resolves permissive on failure — DONE (unreadable owner → no rows; test added)
- Where: `crates/koshi-link/src/ipc_client.rs:394`.
- What: when `std::fs::metadata` on the runtime dir fails, `own_subdir` is
  `None`, so the `Some(entry_name) != own_subdir` filter skips nothing; in the
  same failure `advertised_sessions` returns empty. Every one of the user's own
  shared sessions then comes back as a *foreign* row and is dialled through
  `foreign_endpoint` with an empty connection token.
- Trigger: runtime dir unreadable (permissions, race) with `allow-other-users`
  on.
- Why it matters: this is the exact "a check that cannot get its answer
  resolves to allow" defect shape the project rules ban.
- Fix: unanswerable metadata → skip the row / return the error, never
  reclassify own sessions as foreign.

### A3. Pane-remove early return skips zoom clear and focus repair — DONE (desync path now clears zoom and repairs focus; test added)
- Where: `crates/koshi-session/src/cascade.rs:100`.
- What: the `RemoveError::PaneNotFound` early return fires *after* the pane
  was removed from the registry and `PaneClosing`/`PaneRemoved` were emitted,
  but *before* zoom clearing and focus repair run.
- Consequence: clients stay focused/zoomed on a pane with no registry record;
  `Session::validate` then reports `FocusPaneNotInRegistry` and
  `ZoomTargetMissing`.
- Trigger: needs a registry/layout desync (layout names a pane the registry
  lost).
- Fix: run zoom clear + focus repair before (or regardless of) the early
  return.

### A4. `KeymapHintCatalog.reverted` is write-once-false — DONE (`with_reverted()`; loader sets it on the revert verdict. Note: with a single user layer the revert verdict is unreachable at startup — the wiring is ready for session/layout layers)
- Where: `crates/koshi-config/src/hints.rs:92` (also 153, 199).
- What: `from_parts` sets `reverted: false` and no API can ever write it
  again (field private, no `&mut` accessor). The doc says the conflict loader
  reports its verdict into it.
- Consumer: `crates/koshi-renderer/src/statusline_hints.rs:62` branches on it.
- Consequence: a `keybinding.kdl` whose conflict verdict is `RevertToDefaults`
  falls back to defaults with **no conflict marker** in the hint bar.
- Fix: new small pub API so the loader can record the verdict; renderer side
  already works.

### A5. Seven config keys silently accept child blocks — DONE (one guard in `single_value`, covers every scalar key including field-partials)
- Where: `crates/koshi-config/src/app_config.rs:126-195`.
- What: `theme`, `remote-reconnect`, `allow-beta-features`,
  `allow-other-users`, `remote-listen`, `shared-sessions-dir`,
  `auto-close-session` drop child blocks without an error; only `version`
  rejects children.
- Example: `theme "midnight" { foo }` parses clean; the `{ foo }` vanishes.
- Fix: reject children on every scalar key, same as `version`.

### A6. Updater pre-release picks highest semver, not newest release — DONE (ruled: highest semver is the wanted rule; doc states it; `highest_version` extracted and pinned by test)
- Where: `crates/koshi/src/updater.rs`, `latest_release`'s pre-release branch.
- What: among pre-releases it selects the maximum semver, not the most recent
  by publish date. A re-published older-versioned pre-release is never chosen;
  a higher-versioned yanked-in-spirit one always is.
- Untestable as written: welded to the `get_json` network call; needs the
  release list injectable (see E38).
- Decision needed: is highest-semver actually the wanted rule? If yes, this is
  a doc line, not a bug.

### A7. Detached-started profile session loses its focus target — DONE (focus history recorded unconditionally; test added)
- Where: `crates/koshi-session/src/tab_ops.rs:203-210`.
- What: `commit_profile_tab` records the tab's focus history only when a
  client is given; `commit_new_tab` records unconditionally.
- Example: profile with leaves `[editor, shell]`, `focus_leaf = 1`, committed
  with `focus_client: None` (session started with `koshi --headless`); first
  attaching client lands on `editor` (layout order), not the profile's `shell`.
- Fix: record focus history unconditionally, like `commit_new_tab`.

### A8. `commit_new_pane` with an unknown tab id half-commits — DONE (no-op guard, same as `remove_pane_cascade`; test added)
- Where: `crates/koshi-session/src/pane_ops.rs:116`.
- What: an unknown `tab_id` still registers the pane and emits
  `PaneCreated`/`LayoutChanged`/`PaneFocused` naming a nonexistent tab; the
  orphaned record is later reported by `validate`.
- Contrast: `remove_pane_cascade` (`cascade.rs:72`) no-op guards the same
  case.
- Fix: the same guard — unknown tab → no registration, no events.

### A9. Endpoint-write failure leaves a stale endpoint file — DONE (both failure paths clean up; no deterministic test — injecting the write failure needs a seam neither path has)
- Where: `crates/koshi-runtime/src/ipc_server.rs:336-342`.
- What: when the endpoint-file write fails, the unwind drops the listener and
  removes the socket file but leaves any *pre-existing* stale endpoint file in
  place; the advert-failure path right below (344-349) does remove it.
  `write_atomic` guarantees no partial file — the residue is the old file the
  failed write did not replace.
- Fix: same cleanup on both failure paths.

### A10. `tabline_first_visible` missing its siblings' guards — DONE (returns `Option<usize>`, `None` when no tabline is drawn; test added)
- Where: `crates/koshi-renderer/src/hit_test.rs:246`.
- What: lacks the `viewport == 0` and `all_suppressed` guards its two sibling
  functions have. Cannot panic, but on an all-suppressed frame it answers `0`
  for a tabline `render_frame` never draws — against its own rustdoc.
- Fix: add the two guards; return matches the siblings' convention.

---

## B — Model / design questions (need a ruling, not just a patch)

### B11. `PaneRecord.exit_code` / `exited_at` can lie forever — DONE (deleted; zero production consumers, the lifecycle variant is the single source)
- Where: `crates/koshi-pane/src/pane/state.rs:68-70`.
- What: both are public fields that `update_lifecycle` never writes, so they
  can disagree with `PaneLifecycle::Exited { code, at }` indefinitely.
  `pane/tests.rs:36` documents the split rather than preventing it.
- Fix shape: derive the two fields from the lifecycle transition (write them
  inside `update_lifecycle`), or delete them in favor of reading the lifecycle.

### B12. `server_version_rows_in` never de-duplicates — DONE (proven no overlap: `foreign_sessions` drops every advertised id; doc line added)
- Where: `crates/koshi/src/version.rs` (rows builder, ~line 207 region).
- What: merges `advertised_sessions` with `foreign_sessions` and only sorts.
  A session listed by both sources earns two rows.
- Unproven: whether the two sources can overlap at all. Answer that first;
  if they can, dedupe by session id; if they cannot, add the one-line doc fact.

### B13. `rect_at` Stacked branch skips whole-stack suppression — DONE (suppression check added; a resize in a suppressed stack now refuses with zero spare; test added)
- Where: `crates/koshi-layout/src/resize.rs:212` vs `solver.rs:452`.
- What: the Stacked branch lacks the whole-stack suppression check the solver
  applies, so `resize()` returns `Ok` and stores `resize_delta` −1/+1 for a
  subtree that solves to zero area. Probe-confirmed during the sweep.
- Consequence: the stored delta is harmless once the tab regrows, but the doc
  contract of `rect_at` does not match the code in that state.
- Fix: add the suppression check, or state the actual contract in the doc.

### B14. `continuous` flag list is a silent trap — DONE (test pins the exact continuous set; the `core_seed` parameter rework was not taken)
- Where: `crates/koshi-core/src/action.rs`.
- What: `continuous` is set by a second pass repeating ten seed names
  (`resize-pane*`, `focus-pane*`) in a `matches!`. An eleventh family member
  added without editing that list ships `continuous: false` silently; no test
  pins the family-to-flag pairing.
- Proper fix: a `continuous` parameter on `core_seed` — touches ~40 call
  sites. Cheap alternative: a test pinning the exact continuous set.

### B15. `EmptyTabPolicy::RespawnShell` leaves a dead tab — CLOSED (verified: no production caller selects it — the runtime passes the `CloseTab` default on every removal path, so the variant is unreachable; comment corrected. Wiring it up is a feature decision, not a hole)
- Where: `crates/koshi-session` (policy handling; see session docs).
- What: the policy leaves an empty tab in the session and emits no events; the
  tab keeps a layout naming a removed pane's ex-slot. Session docs call the
  respawn "the runtime's job", but no session-layer path exists.
- Action: verify the runtime actually performs the respawn on this path; if
  nothing does, this is a hole to task.

### B16. `register_running_pane` swallows `DuplicateId` — CLOSED (keep-original is deliberate: pinned by `committing_a_pane_id_already_registered_keeps_the_original_record` and stated in the fn doc)
- Where: `crates/koshi-session` (the shared registration helper the sweep
  consolidated).
- What: discards `PaneRegistry::insert`'s `Err(DuplicateId)`: a duplicate id
  keeps the old record while `PaneCreated` is still emitted and the id
  installed in the layout. Practically unreachable — ids are freshly minted
  UUIDs. Pre-existing in all three former copies.
- Options: debug_assert, propagate, or leave with a doc line.

### B17. `TextView::first_row` unreachable arithmetic — CLOSED (left: `Scrollback` maintains `lines.len() <= total_pushed` on every path, scrollback.rs `push`/reflow; the saturating form is total and stays)
- Where: `crates/koshi-terminal/src/selection.rs:139`.
- What: the `saturating_sub` is unreachable given `Scrollback`'s
  `lines.len() <= total_pushed` invariant; either the doc premise is stale or
  the spot deserves a `debug_assert`.

---

## C — Cross-crate simplifications (pub-API changes, banned during the sweep)

### C18. Delete empty pub placeholder modules — DONE (13 files deleted)
- `koshi-core/src/types.rs` (one line, defines nothing),
  `koshi_terminal::types` (lib.rs:16 + doc-only types.rs),
  `koshi_ipc::types` (same shape), `koshi-pty` `types.rs` +
  `backend/tests.rs` placeholders,
  `koshi-layout/src/layout.rs` + `layout/{command,state,tests}.rs` (4 files,
  12 lines, zero items — layout.rs's doc describes code that does not exist),
  `koshi-storage` `pub mod store` (+ `store::state`) and `pub mod types`.
- Zero workspace consumers for all of them (grep-confirmed in the sweep).
- Pure deletion, ~25 lines + the `pub mod` lines.

### C19. `Rect::at_origin(size)` + `Size::min_axes` in koshi-core — DONE (57 call sites; runtime's `min_size` helper deleted)
- `Rect::new(Point { x: 0, y: 0 }, size)` appears 11 times in koshi-runtime
  alone; per-axis min is hand-rolled where needed. Adding the two constructors
  deletes runtime's private `min_size` helper.

### C20. Kill the last hot-path clones in session/pane — DONE (`PaneKind` derives `Copy`, 7 clones removed. The `repair_focus` item was stale: no clone exists — single by-value pass — so the signature stays)
- `focus::repair_focus` takes `FocusCandidates` by value → change to
  `&FocusCandidates` (removes the clone the sweep already hoisted).
- `PaneLifecycle::transition` takes `kind: PaneKind` by value, read only in
  the `Err` arm → `&PaneKind`, or derive `Copy` on `PaneKind` (its whole
  payload is already `Copy` — `PluginId` derives Copy, koshi-core/src/ids.rs:165).

### C21. `FiringRules` struct in koshi-config — DONE (both `too_many_arguments` expects removed)
- `scan_collisions` / `effective_bindings` / `merge_keymaps` repeat the same
  layer walk with a 7-parameter tail forcing two
  `#[expect(clippy::too_many_arguments)]`. A
  `FiringRules { registry, reserved, locked, max_chord_depth }` plus one
  shared iterator removes both expects.

### C22. Small dedup batch — DONE (7 of 8: SessionRef Display, `waited_out` in `transport`, `store_deadline`, link's `read_store`/`store_failed` made pub, `Discovered::sort_sessions`, `SpawnSpec::shell`, `transport::accept_until_shutdown`. Dropped: `Direction::axis()` — claimed repetition is 2 sites, both clearer as-is)
- `Direction::axis()` on koshi-core geometry (layout currently maps
  direction→axis in a private helper; other crates repeat the shape).
- A `SessionRef` label helper — SessionNotFound-from-SessionRef built at
  `main.rs:443`, `share.rs:404`, `version.rs:207`.
- `waited_out(&io::Error)` byte-identical at
  `koshi-test-support/src/throttle.rs:79` and `koshi-ipc/src/tls.rs:302`;
  natural home `koshi_ipc::transport` needs test-support→ipc dep, the
  no-manifest home is koshi-core — pick one.
- `TlsReader`/`TlsWriter` `set_deadline` bodies byte-identical
  (tls.rs:123,191).
- `read_store` + `store_failed` duplicated (`koshi/src/remote_cmd.rs:74/:93`
  ↔ `koshi-link/src/remote_client.rs:172/:182`) — make link's pair pub.
- Session census sort order hand-written twice (`koshi/src/targeting.rs:190`,
  `koshi-link/src/discovery.rs:198`) while the invariant is documented on
  `Discovered::sessions` — `pub fn sort_sessions` on `Discovered`.
- `SpawnSpec` shell_kind-derived-from-program pairing enforced in
  `koshi-core/src/process.rs:129` and `koshi-runtime/src/runtime/command.rs:445`
  — one pub constructor in core.
- accept_loop/serve_connection skeleton duplicated
  (`koshi-daemon/src/router.rs:580/:615`,
  `koshi-runtime/src/ipc_server.rs:545/:614`) — new pub fn in
  `koshi_ipc::transport`, ~24 lines. (remote_listener's accept loop is TCP +
  rate limiting — different semantics, excluded.)

### C23. `LockMode::passes_to_pane()` — DONE (one method on `LockMode`; both process-side copies deleted)
- The pass-through rule `matches!(mode, Normal | Locked)` is duplicated across
  **processes**: `koshi-client/src/input.rs:231` and
  `koshi-runtime/src/runtime/input.rs:226`. Adding a `LockMode` variant and
  updating one copy silently splits which keys reach the pane. One pub method
  on `LockMode` in koshi-core; both crates already depend on core.

### C24. Pane lookup by id in koshi-pty — DONE (`PortablePtyBackend::child_pid`; note: `router::child_pid` no longer existed, only the supervisor copy)
- `router::child_pid` and `pty_supervisor::child_pid` each call
  `PortablePtyBackend::carried_panes()`, allocating a Vec of every pane to
  find one entry. A koshi-pty lookup by `PaneId` drops that allocation from
  the spawn path and `close_panes_that_ended`.

### C25. `DomainError::severity()` default body — CLOSED (left per-impl: a future impl silently inheriting `Recoverable` is the exact fail-open shape the project bans)
- `fn severity() { Severity::Recoverable }` appears identically 17 times
  across 17 files; one impl (koshi-session) returns `SessionFatal`. A default
  trait-method body at `koshi-core/src/error.rs:79` saves ~50 lines — but a
  future impl that forgets to override silently gets `Recoverable`.
  `category()` stays per-impl (8 distinct values).

### C26. The seven identical id types — CLOSED (left, as recommended)
- `koshi-core/src/ids.rs`: SessionId/ClientId/TabId/PaneId/PluginId/CommandId/
  SubscriberId are byte-identical apart from the Display prefix (~35 lines
  each). Collapse needs `macro_rules!` (banned) or a generic `Id<Kind>` (pub
  API change across the whole workspace). Cost exceeds the win.

### C27. Wire/render type mirror — CLOSED for 0.3.0 (deferred past the release, as recommended)
- ~250 lines of mirrored type declarations plus ~450 lines of two inverse
  conversion modules: `koshi-ipc/frame.rs` ↔ `koshi-renderer/snapshot.rs`,
  out-direction `koshi-runtime/src/runtime/frame.rs` (228 lines),
  in-direction `koshi-client/src/attach/paint.rs` (223 lines).
- Byte-identical pairs: FrameSlot/PaneSlot, FrameTabMeta/TabMeta,
  FrameClient/ClientSnapshot, FrameCursor/CursorSnapshot,
  FrameSelection/SelectionSpans, FrameScrollback/ScrollbackMeta,
  FrameColor/Color, FrameUnderline/UnderlineStyle, FrameRowEnd/RowEnd,
  FrameCursorShape/CursorShape.
- Collapse needs a new koshi-ipc → koshi-terminal dep edge (acyclic) and
  pub-type moves. **Caveat:** FrameSession/FrameTab/FramePane are NOT pure
  mirrors — field names differ; collapsing without serde renames changes the
  wire format. FrameWindow/FrameRow/FrameCell/FrameStyle are run-length wire
  forms, genuinely not duplication.

### C28. `MouseAction` ↔ `WireMouseAction` mirror — CLOSED (left, as recommended)
- `koshi-client/src/mouse.rs:99` ↔ `koshi-ipc/src/protocol.rs:317`,
  variant-for-variant, converter at `attach.rs:2181`. Collapse deletes the pub
  client type; saves ~90 lines.

### C29. `FrameRow::cells()` has no production caller — CLOSED (kept as the tested inverse of `from_cells`)
- `koshi-ipc/src/frame.rs:349` — the sweep's paint.rs change removed the last
  production use; remaining callers are koshi-ipc's own tests. Keep as the
  tested inverse of `from_cells`, or drop it and assert over runs.

---

## D — Performance (need shape changes)

### D30. `FrameRow::from_cells` forces a throwaway Vec per row per frame — DONE (takes `impl IntoIterator<Item = FrameCell>`; the per-run clone went with it)
- `koshi-ipc/src/frame.rs:331` takes `&[FrameCell]`, so runtime's `wire_row`
  builds a temporary `Vec<FrameCell>` for every row of every painted frame.
  An iterator-taking form removes one allocation per row per frame on the
  render hot path.

### D31. `Screen::refresh` rebuilds everything for viewer-only changes — DONE (`Screen` caches the `RenderSnapshot` — grids behind `Arc`s, so the cache clone moves no cell data; `last_painted` frame no longer stored)
- `koshi-client/src/attach.rs:334` — a hover, tab-strip peek, lock-mode flip,
  or open key sequence re-expands every run of every pane via
  `to_snapshot(last_painted)` and rebuilds every Grid behind a fresh `Arc`, to
  draw the same picture with different chrome. Caching the `RenderSnapshot`
  beside `last_painted` makes refresh nearly free; changes `Screen`'s shape.

### D32. `TlsReader::read` zeroes 16 KiB per call — DONE (buffer boxed on the struct; hand-written `Debug` omits it)
- `koshi-ipc/src/tls.rs:145` — a fresh zeroed 16 KiB stack buffer per read on
  the remote PTY-output path. Move the buffer into the struct; needs a
  hand-written `Debug` (derived Debug would print 16384 bytes).

### D33. Renderer double-format + linear span scan — CLOSED (left: redraw is event-driven, the strings are small, and the `row_span` merged walk would lean a pub contract on an invariant for a micro win)
- Tabline texts (`render/tabline.rs:106-160` + `draw_tabline`): tab texts,
  right_block_text and session_texts are each formatted twice per frame
  (solve + paint). Fix = `TablineLayout` carrying measured texts (pub(crate)
  struct change).
- `snapshot.rs:452` `SelectionSpans::row_span` is a linear scan per visible
  row, O(rows × spans) per pane per frame; a merged walk would depend on the
  documented ascending-row invariant, and `row_span` is pub.

### D34. `SessionLogWriter::write` runs `create_dir_all` per line — DONE (ruled: append first, recreate-and-retry on failure — keeps the deleted-dir resilience, drops the per-line syscalls; test added)
- `koshi-observability/src/logging.rs:~230` — two path-walking syscalls per
  log event on the runtime dispatch thread. Removing it changes behavior: it
  currently re-creates a `logs/` dir deleted mid-session. Rule which behavior
  is wanted, then fix accordingly.

### D35. Leave-unless-you-care — CLOSED (left, as recommended)
- `koshi-pty` `wait_for_exit` polls the exited flag every 25 ms for the whole
  graceful-kill window; the watcher channel could carry the exit but the swap
  changes thread wake ordering.
- `koshi-layout`/renderer `reveal_active` probes with `pack_tabs` in a loop —
  up to N packs of O(N) with a fresh Vec each, per frame.
- Micro items recorded and dismissed: `send_switch` per-switch Vec,
  `restore_cursor` 4× dispatch on cold DECRC, `evict_to_caps` byte re-walk
  (caching changes serde shape), `main.rs::run` 30-branch if-let chain,
  per-call header-row copy in `output::table`.

---

## E — Test seams needing product change

### E36. `grant_token`'s replacing filter has zero tests — DONE (five-record test: only the live matching token's connection is cut)
- `koshi-daemon` — the filter (identity AND scope AND not-revoked AND
  not-expired) that decides which existing token a grant replaces is untested.
  Testable today with effort, or cheaply after a seam.

### E37. TLS-concrete signatures block bridge tests — DONE with residual (`admitted_frames`/`refuse` now generic over `Read`/`Write + Deadlined`; list-then-attach and second-Hello-refusal tests added. Residual: the version-range substitution pin through a real TLS handshake still needs a hand-built remote client)
- `admitted_frames` (koshi-daemon/src/remote_listener.rs) takes concrete
  `TlsReader`/`TlsWriter`; the documented "one connection may list and then
  attach" and the second-Hello refusal have no test — needs generic
  `Read`/`Write` params (product change) or a real TLS rig.
- Session-protocol range relayed through the bridge: only `bridged_hello` is
  tested; a substitution at `serve_remote` → `serve_admitted` →
  `bridge_to_session` passes every test. Pinning needs a hand-built Hello with
  a distinctive range through a real handshake.

### E38. Untestable-by-construction spots — DONE with residual (`is_discovery`/`discovery_session` moved onto `CliCommand` + tests; `highest_version` extracted + test; `read_hidden_line` takes `impl Read` + test. Residual: the updater's orchestration fns — `maybe_prompt_startup_update`, `run_update_command`, `swap_exe`, `replace_with_sudo` — need an injection design; `run_discovery`/`finish_command` print and talk live IPC, a move alone buys no test)
- koshi bin helpers (`is_discovery`, `discovery_session`, `run_discovery`,
  `finish_command`) live in `main.rs` with no test module — moving them into
  lib.rs makes them testable.
- updater (`maybe_prompt_startup_update`, `run_update_command`, `swap_exe`,
  `replace_with_sudo`, `latest_release`) reads real clock/config/network/
  current_exe and exits — needs injectable seams.
- `read_hidden_line` opens stdin itself (needs an `impl Read` param — the one
  place a typed secret is assembled).

### E39. Hand-maintained name pairs with no cross-check — DONE (test drives all 16 action verbs through `to_action` and checks each name against `core_action_seeds`)
- `CliCommand::to_action` builds `ActionRef`s from hardcoded strings
  (`"new-pane"`, …) and every production call site discards the result; a name
  drifting from `core_action_seeds()` is invisible at runtime and untestable
  today.

---

## F — Docs / structural doc gaps

### F40. Comment and test nits — DONE (this pass)
- `core/action/tests.rs` — coming-soon doc named "search family and quit";
  now names the real set (`core:copy-selection` + six plugin actions).
- `core/command/tests.rs` — variant-name snapshot extended 16 → all 20
  variants (added ToggleMouseSelect, Detach, DetachAll, SwitchSession);
  "22-variant" doc corrected to 20.
- `runtime/mouse.rs:244` — highlight-drop comment now includes motion reports.
- `runtime/shutdown.rs` — "SEAM"/"when it lands" status language rewritten as
  behavior (stages 3 and 5 are no-ops; storage is the NullStorage stand-in).
- `koshi/src/cli.rs` — `parse_session_ref`/`parse_tab_ref` rustdoc now states
  the exact empty-value error.
- ipc rustdoc links: reported as 4 unresolved-link warnings — did NOT
  reproduce under `cargo doc -p koshi-ipc --no-deps` on a fresh build; treated
  as not real. **DROPPED.**
- Gates after: fmt ✓, clippy ✓, core+runtime+koshi suites 1688 passed.

### F41. README CLI reference is incomplete — DONE (sharing/remote/debug sections and the `--allow-other-users` row added; `koshi plugin` deliberately absent — see F42)
- README omits `koshi share`, `koshi remote`, `koshi debug`, `koshi plugin`,
  the global `--remote`, and `--headless --allow-other-users` (all real, per
  `crates/koshi/src/cli.rs:57-62, 265-284, 424-443`). `config-docs/cli.md`
  documents all but `koshi plugin`. Adding is a structural doc task, not a
  correction.

### F42. `koshi plugin` parses but is unreachable and undocumented — DONE (ruled: hidden from help with `#[command(hide = true)]` until the plugin host exists; still parses, still reports the runtime unavailable)
- Parses, then falls through to `CliError::IpcUnavailable`
  (`main.rs:328-332`); appears in no doc. Decide: document it, hide it until
  the plugin host exists, or leave.

---

## G — Workspace hygiene

### G43. Dev-dep drift + duplicated test scaffolding — DONE with residual (tempfile/uuid/serde_json/proptest promoted to `[workspace.dependencies]`; `test_runtime_dir` consolidated into `koshi_test_support::fixtures` — 10 copies deleted. The two `snap` fixtures turned out to be different fixtures, not duplicates — left. The remaining builder dedups — Hello ×5, Server::new ×5, drive-loop ×4, and the rest — stay recorded, per this finding's own "clean wins" call)
- Version drift: tempfile 3.27.0 vs 3.20.0 (koshi-paths), uuid 1.24.0 vs
  1.23.4 (koshi-daemon), serde_json 1.0.150 vs 1.0.151. Fix: promote
  tempfile, uuid, serde_json, proptest to `[workspace.dependencies]` — saves
  16 version strings and removes the drift.
- ~300 lines of duplicated test scaffolding koshi-test-support should export:
  `test_runtime_dir` ×6, Attach IpcRequest builder ×7, Hello builder ×5,
  `Server::new` builder ×5, server drive-loop ×4, RecordingSink ×2,
  supervisor socket-name ×2, `non_utf8_path` ×2, snapshot fixtures ~90 lines
  ×2, RenderSnapshot fixture ×2, `reply()` ×3, stand-in router ×2.
  `test_runtime_dir` and the snapshot fixtures are the clean wins;
  `test_runtime_dir` needs tempfile added as a real dep of koshi-test-support
  (currently zero deps).
