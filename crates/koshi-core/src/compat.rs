//! Every versioned surface koshi carries, in one table.
//!
//! A surface is anything two koshi builds must agree on to work together: a
//! wire protocol they speak, or a file one writes and another reads. Each
//! surface carries its own version number.
//!
//! # The cadence rule
//!
//! `maximum_version` moves in the same commit as the change that requires it.
//! Three changes require it:
//!
//! - An existing field changes its type.
//! - An existing field changes its meaning.
//! - A field is added that one side must not send until it knows the other
//!   reads it.
//!
//! Adding or removing a field that both sides still decode leaves
//! `maximum_version` where it is.
//!
//! The first such change after a release sets `maximum_version` to
//! `released_version + 1`. `maximum_version` then holds until the next
//! release, however many further changes land: one release cycle moves a
//! surface one step at most.
//!
//! [`Surface::find_version_problem`] checks this rule, and a test runs the whole
//! table through it.

/// One versioned surface: what two builds must agree on, and the versions this
/// build speaks of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Surface {
    /// What this surface is called in plain words, e.g. `"session protocol"`.
    /// Used in the message a failing check prints.
    pub surface_name: &'static str,
    /// The lowest version this build accepts. A peer whose highest is below it
    /// is refused.
    pub minimum_version: u32,
    /// The highest version this build speaks, and the one it uses when the peer
    /// speaks it too.
    pub maximum_version: u32,
    /// The version the last released koshi spoke of this surface, or `None`
    /// when no release has carried it.
    ///
    /// With `None`, [`Surface::find_version_problem`] checks only that
    /// `minimum_version` does not exceed `maximum_version`; `maximum_version`
    /// may hold any value.
    pub released_version: Option<u32>,
}

/// The session protocol: what an attached client and a session server speak
/// over that session's control socket.
///
/// `v0.1.0` spoke 1, `v0.2.0` and `v0.3.0` spoke 2, `v0.4.0` spoke 3, and
/// `v0.5.0` speaks 4. This build speaks 5. The floor is 5: a peer that speaks 4
/// is refused at the handshake.
///
/// The following shapes differ between 4 and 5. This build writes version 5 on
/// every session connection:
///
/// - A new-pane command carries `placement`: a split, a stack, or a floating
///   pane. 4 carried `source_pane_id`, `tab_id`, `direction` and
///   `should_stack` on the command itself, and had a separate run-command
///   command; `koshi run` sends a new-pane command with a spawn spec.
/// - `PaneCreated`, `PaneRemoved` and `PaneFocused` carry a `tab_id` that is
///   `null` for a floating pane, and `PanePlacementCommitted` carries a
///   `source_tab_id` and a `destination_tab_id` that are `null` for one.
/// - The `placement_target` of a `PlacePane` command and of a
///   `PanePlacementCommitted` event is `{"Tiled": {"Swap": …}}`,
///   `{"Tiled": {"Split": …}}`, `{"Tiled": {"Stack": …}}`, which puts the
///   source pane into a tiled pane's stack, or `{"Floating": {"top_left_cell":
///   …}}`, which makes a tiled or stacked pane a floating pane. 4 carried the
///   swap or split target with no `Tiled` wrapper, and had no `Stack` and no
///   `Floating` target. A `PlacePane` command's `source_pane_id` is `null` for
///   the pane the command source acts on; 4 required a pane id.
/// - A client names the placement target it previews with the
///   `SetPanePlacementPreview` request, and with the `placement_preview_target`
///   of its `Attach`, which 4 has not. The target's preview rides that
///   client's painted updates: a part whose `part_flags` set bit 2 carries the
///   preview's JSON after the frame's JSON, and a byte count of `0` drops the
///   preview. A preview carries `source`: `{"Tiled": …}` with the source tab's
///   tree and solved slots, or `"Floating"`. It carries no pane content: the
///   frame's own panes carry it. The `ConfirmPanePlacement` request runs a
///   tiled placement only when the destination tab's solve for its client
///   equals the request's `expected_placed_tab_layout`. 4 read a preview with
///   the `ReadPanePlacement` request and answered it with a
///   `PanePlacementSnapshot` or `PanePlacementRefused` event that carried
///   `source_tab_id`, `source_tab_snapshot` and each pane's rows; 5 has none
///   of them. A `PaintedUpdate` request carries only `update_sequence`.
/// - A frame carries no session or client revision; 4 carried both. An image
///   placement's `is_available`, a close command's
///   `should_kill_process_tree`, and a terminal-too-small event's cause are
///   required. 4 read a message without them.
/// - A frame carries `floating_pane_slots`: the floating panes its client
///   shows, back to front, each with its outer rect, content rect and pin
///   state. It also carries `floating_panes`: every floating pane of the
///   session, in the order the panes became floating, each as
///   `{"Shown": {"pane_id": …}}` while the client shows it, or as
///   `{"Minimized": {"pane_id": …, "pane_title": …}}`, with its title or
///   `null`, while the client minimized it. Both fields are required, and 4
///   has neither.
/// - A frame's client carries `last_applied_lock_command_id`: the last lock
///   command that the client's own connection sent and the session ran, or
///   `null`. It also carries `last_applied_floating_pane_move_command_id`: the
///   last floating pane move that the client's mouse or key binding sent and
///   the session ran, or `null`. It also carries
///   `last_applied_placement_command_id`: the last pane placement that the
///   client confirmed and the session ran, or `null`. It also carries
///   `active_tab_focused_pane_id`:
///   the client's focused pane in its active tab, also while a floating pane
///   takes its input, or `null`. 4 has none of these fields.
/// - The `MoveFloatingPane`, `SetPanePinned`, `TogglePanePinned`,
///   `SetPaneMinimized`, `TogglePaneMinimized`, `SetAllFloatingPanesMinimized`
///   and `ToggleAllFloatingPanesMinimized` commands, the `NextFloatingPane` and
///   `PreviousFloatingPane` focus targets, and the `FloatingPaneMoved`,
///   `PanePinChanged` and `PaneMinimizedChanged` events in a command result,
///   exist in 5 only.
/// - A `HostWrite` event names its bytes `host_write_bytes` and carries at
///   most 12582879 of them; 4 named them `host_output_bytes`. A longer host
///   write goes as `HostWritePart` events, then one `HostWrite`. The
///   `HostWritePart` event, the `ClipboardCopyPart` and `ClipboardCopy`
///   requests, and their `ClipboardCopyPartTaken` and `ClipboardCopyRouted`
///   answers exist in 5 only.
/// - An `Attach` request carries `link_timeout_ms`, which a client on another
///   machine sets and a client on this machine leaves out. A session refuses a
///   `link_timeout_ms` below `1000` with the `RequestFailed` code. `Leaving`
///   carries `is_returning`; 4 sent a bare `Leaving`. The `Ping` request and
///   the `Pong` event exist in 5 only.
/// - A `Keyboard` request carries `key_inputs`: the keys of one read of the
///   client's terminal that go to the pane, oldest first, which the session
///   writes to the pane in one write. 4 carried one key in `key_input`.
/// - A mouse round's `Forward` carries the event's `mouse_kind`, its
///   `modifier_flags` and `pane_local_cell`, the cell in the pane's content
///   that the client resolved; 4 carried the client's screen cell in
///   `mouse_input`. The `MoveFloatingPane` mouse action exists in 5 only.
/// - The `Prepare` and `Commit` requests and the `Prepared` answer exist in 5
///   only. A `SubmitCommand`, an `Attach`, a `ClipboardCopy` and a `Restart`
///   run only as the request a `Prepare` holds, once its `Commit` arrives, and
///   one sent alone is refused with the `RequestFailed` code. A
///   `SubmitCommand` on a connection that carries an attached client's event
///   stream runs as it arrives. 4 ran `SubmitCommand`, `Attach` and `Restart`
///   as they arrived.
/// - The `Hello` answer carries `process_image_id`: the id of the answering
///   session's process image, which a restart into a new image changes. The
///   field is required, and 4 has no such field.
/// - A frame travels as a painted update in binary frames, whose length
///   prefix has bit 31 set: the frame's JSON only when it changed, and each
///   pane's cursor, scroll position and rows in the row encoding, with only
///   the rows the client does not hold. A frame that changes nothing is not
///   sent. 4 sent every frame as one `Painted` JSON event that carried every
///   row as cell runs.
/// - The `PaintedUpdate` request exists in 5 only. An attached client sends it
///   after each paint, naming the last painted update it applied, and nothing
///   answers it. The session sends the next painted update once the bytes in
///   flight past the last reported update fit the connection's window, and
///   one that names an update not sent closes the connection. 4 sent every
///   frame as it was composed.
/// - The `Attach` request carries `max_frames_per_second`, from `30` to `120`:
///   the session sends the attached client at most that many frames a second,
///   and refuses an `Attach` with a value outside that range with the
///   `RequestFailed` code. The field is required, and 4 has no such field.
/// - Each chunk of an image record's RGBA bytes travels as a binary frame of
///   kind `2`: the transfer id and the byte offset as 8 big-endian bytes each,
///   the last-chunk flag as one byte, then the raw bytes. The
///   `ImageContentChunk` event exists in 4 only, and carried the bytes as
///   base64 text in JSON.
/// - A `SwitchSession` command and a `SwitchTo` event carry `owner_user_id`:
///   the user id whose folder of the shared directory advertises the session,
///   or `null`. The field is required, and 4 has no such field.
/// - A pane row of an `Overview` answer carries the lifecycle `Running` only.
///   4 also carried `Spawning`, `Exited` with the child's `exit_code`, and
///   `Closing`. The answer lists the floating panes after the tiled panes,
///   each with a `tab_id` of `null`; 4 has no floating pane.
/// - A `Layout` answer carries `floating_panes`: each floating pane's desired
///   size, solved size, and every attached client's stored position and what
///   that client draws for it. The field is required, and 4 has no such field.
/// - The attach answer's session structure and the `Resync` event carry
///   `notification_queue`: the session's whole notification queue. The
///   `NotificationPosted` event exists in 5 only: one entry the queue took,
///   with the queue's `revision` after it. 4 has no notification queue.
/// - The `CommandRejected` event names a command from the client's key
///   binding, its pane placement confirmation, or a floating pane move from
///   its mouse round, that the session refused. It carries `command_id`,
///   `fired_action`, the action the key fired or `null`, `reject_reason`, and
///   `help`, the session's hint or `null`. 4 sent `PlacementCommandRejected`
///   with `command_id` alone, for a pane placement only.
/// - A `SubmitCommand` request is an object with `command_envelope` and
///   `fired_action`, and a `ConfirmPanePlacement` request carries
///   `fired_action`: the action the client's key fired, or `null`. The field
///   is required in both. 4 carried the command envelope as the whole
///   `SubmitCommand` value, and has no `fired_action`.
/// - A frame carries no `is_recovery_notice_visible`. A session that cannot
///   restore its saved state after a restart posts the `Error` notification
///   `the session could not be restored after the restart; a new shell
///   started`. 4 carried the field, and left it out while it was `false`.
///
/// A restart request the session reads and refuses is answered with the
/// `RequestFailed` code and a sentence naming what stopped the restart. A
/// `v0.5.0-pr.1` session, which also speaks 4, and a `v0.4.0` session answer
/// it with the `MalformedRequest` code and the same kind of sentence.
pub const SESSION_PROTOCOL: Surface = Surface {
    surface_name: "session protocol",
    minimum_version: 5,
    maximum_version: 5,
    released_version: Some(4),
};

/// The control plane: what a caller and the router speak over the router's
/// socket.
///
/// `v0.4.0` speaks 2. `v0.5.0` speaks 3. The floor is 3.
///
/// The `Hello` answer carries `process_image_id`: the id of the answering
/// router's process image, which a restart into a new image changes. A
/// `v0.5.0` router leaves it out, and a `v0.5.0` caller ignores it.
///
/// The `RemoteStatus` answer carries `config_file_path`: the `koshi.kdl` the
/// answering router read when it started. A `v0.5.0` router leaves it out,
/// which reads as `None`, and a `v0.5.0` caller ignores it.
///
/// A request the router reads and refuses, other than one naming a session it
/// does not have, is answered with the `RequestFailed` code. A `v0.5.0-pr.1`
/// router, which also speaks 3, and a `v0.4.0` router answer it with the
/// `MalformedRequest` code.
pub const CONTROL_PROTOCOL: Surface = Surface {
    surface_name: "control plane",
    minimum_version: 3,
    maximum_version: 3,
    released_version: Some(3),
};

/// The supervisor link: what a session server and the process holding its panes
/// speak.
///
/// `v0.4.0` speaks 1. `v0.5.0` speaks 2. The floor is 2.
///
/// Version 3 gives each pane an output window: the supervisor sends at most
/// that many bytes of one pane's output ahead of the session's
/// `GrantPaneOutput` requests. Each pane's output travels as a binary frame.
pub const SUPERVISOR_PROTOCOL: Surface = Surface {
    surface_name: "supervisor link",
    minimum_version: 2,
    maximum_version: 3,
    released_version: Some(2),
};

/// The remote access token store: the file this machine keeps its grants in.
///
/// `v0.4.0` writes 1. `v0.5.0` writes 2. Format 1 is converted before the
/// token store is opened.
pub const TOKEN_STORE_FORMAT: Surface = Surface {
    surface_name: "token store format",
    minimum_version: 1,
    maximum_version: 2,
    released_version: Some(2),
};

/// The remote protocol: what a client on another machine and this machine's
/// TLS listener speak before any session is reached.
///
/// `v0.4.0` speaks 1. `v0.5.0` speaks 2. The floor is 2. Its two ends are
/// different machines. The session protocol the two ends settle after the
/// Welcome is a separate surface, [`SESSION_PROTOCOL`].
///
/// An `Attach` may carry `link_timeout_ms`, and a value below `1000` is
/// refused. A listener that does not know the field ignores it, and an
/// `Attach` without it sets no time limit on the bridge.
///
/// The `Refused` that answers a Hello whose secret the listener does not
/// admit carries `remote_protocol_version`, the version settled from that
/// Hello. A `v0.5.0` dialer ignores the field, and a `v0.5.0` listener leaves
/// it out, which reads as `None`.
///
/// An Attach to a session the secret reaches that cannot be opened is refused
/// with `the session did not answer`. A `v0.5.0` listener refuses it with
/// `this server did not admit the connection`, and a `v0.5.0` dialer reads
/// both sentences the same way.
pub const REMOTE_PROTOCOL: Surface = Surface {
    surface_name: "remote protocol",
    minimum_version: 2,
    maximum_version: 2,
    released_version: Some(2),
};

/// The saved server file: the servers a dialling machine has saved, with the
/// secret and the certificate fingerprint for each.
///
/// `v0.4.0` writes 1. `v0.5.0` writes 2. Format 1 is converted when the
/// dialling machine opens its saved-server store. At the same point, a saved
/// name with the shape of a secret, 64 lowercase hex characters, which
/// `v0.5.0` writes, is dropped, and the store stays at 2.
pub const SAVED_SERVER_FORMAT: Surface = Surface {
    surface_name: "saved server file format",
    minimum_version: 1,
    maximum_version: 2,
    released_version: Some(2),
};

/// The remote certificate file: the certificate and private key this machine
/// generated for its remote listener.
///
/// `v0.4.0` writes 1. `v0.5.0` writes 2. Format 1 is converted when the
/// replacement router starts.
pub const REMOTE_CERTIFICATE_FORMAT: Surface = Surface {
    surface_name: "remote certificate file format",
    minimum_version: 1,
    maximum_version: 2,
    released_version: Some(2),
};

/// The remote access record: the file saying the operator switched remote
/// access on for this machine.
///
/// `v0.4.0` writes 1. `v0.5.0` writes 2. Format 1 is converted when the
/// replacement router starts.
pub const REMOTE_ACCESS_RECORD_FORMAT: Surface = Surface {
    surface_name: "remote access record format",
    minimum_version: 1,
    maximum_version: 2,
    released_version: Some(2),
};

/// The resume file: the state a session server writes before it replaces its
/// own process image, and the next image reads back.
///
/// `v0.3.0` writes 2, `v0.4.0` writes 3, and `v0.5.0` writes 4. This build
/// writes 5: format 5 adds each session's floating panes, each client's view of
/// them, each session's starting tab, each tab's solved tab size, the views
/// detached clients left with the resume-token hashes, and the process IDs of
/// the children closed panes left running. It turns each client's focused pane
/// in each tab into a one-pane focus history, and drops each pane record's
/// lifecycle and close policy. Format 5 layout trees hold resize deltas that
/// the solve applies after the `preferred` targets, and formats 1 through 4
/// hold resize deltas applied before them: a restored body of format 4 or
/// older has its resize deltas rewritten from the terminal sizes the header
/// carries. A tab whose sizes give back the rect it was last drawn at keeps
/// every pane's size. In every other tab, the resize delta of each child with
/// a `preferred` target moves onto its trailing-most flexible sibling.
/// Format 5 also adds each session's notification queue, and drops each
/// session's `is_recovery_notice_visible`. A format 4 session gets a queue in a
/// new epoch, which holds one `Error` entry when its
/// `is_recovery_notice_visible` was `true`, and is empty otherwise.
/// Formats 4 and 5 use the declared field names in the saved records. Formats 1
/// through 4 pass through the ordered migration steps before they are restored.
///
/// The build being installed states which format it writes. The running server
/// reads that answer before it commits to the swap.
pub const RESUME_FORMAT: Surface = Surface {
    surface_name: "resume file format",
    minimum_version: 1,
    maximum_version: 5,
    released_version: Some(4),
};

/// The config schema: the shape of the files under the config directory.
///
/// `v0.4.0` writes 1. `v0.5.0` writes 2. `koshi serve-session` and
/// `koshi serve-router` migrate version 1 files to version 2 before they read
/// config.
pub const CONFIG_SCHEMA: Surface = Surface {
    surface_name: "config schema",
    minimum_version: 1,
    maximum_version: 2,
    released_version: Some(2),
};

/// The endpoint file: the file each running server writes in the runtime
/// directory to name its control socket, its connection token and its process.
///
/// Format 1 is every endpoint file with no `file_format` field: `v0.4.0` writes
/// `{socket, token, pid}`, and `v0.5.0-pr.1` writes
/// `{socket_address, connection_token, process_id}`. `v0.5.0` writes 2, the
/// second shape with `"file_format": 2`. A reader converts a format 1 file in
/// memory and leaves it on disk: the server that wrote it rewrites it in
/// format 2 when it restarts into this build.
pub const ENDPOINT_FILE_FORMAT: Surface = Surface {
    surface_name: "endpoint file format",
    minimum_version: 1,
    maximum_version: 2,
    released_version: Some(2),
};

/// The program file: the file each running server writes beside its endpoint
/// file to name the koshi version it runs and the program file it restarts
/// into.
///
/// `v0.5.0` writes 1.
pub const PROGRAM_FILE_FORMAT: Surface = Surface {
    surface_name: "program file format",
    minimum_version: 1,
    maximum_version: 1,
    released_version: Some(1),
};

/// Every versioned surface this build carries. A surface absent from this list
/// is not checked against the cadence rule.
pub const SURFACES: &[Surface] = &[
    SESSION_PROTOCOL,
    CONTROL_PROTOCOL,
    SUPERVISOR_PROTOCOL,
    TOKEN_STORE_FORMAT,
    REMOTE_PROTOCOL,
    SAVED_SERVER_FORMAT,
    REMOTE_CERTIFICATE_FORMAT,
    REMOTE_ACCESS_RECORD_FORMAT,
    RESUME_FORMAT,
    CONFIG_SCHEMA,
    ENDPOINT_FILE_FORMAT,
    PROGRAM_FILE_FORMAT,
];

impl Surface {
    /// Why this surface's numbers break the cadence rule, or `None` when they
    /// follow it.
    ///
    /// Three checks, in this order. Each names this surface's
    /// [`surface_name`](Self::surface_name).
    ///
    /// 1. `minimum_version` exceeds `maximum_version`: `"the control plane
    ///    accepts 4 at the lowest and 3 at the highest, which is no version at
    ///    all"`.
    /// 2. `maximum_version` is below `released_version`: `"the control plane
    ///    speaks 1, which is below the 2 the last release spoke"`.
    /// 3. `maximum_version` is more than one above `released_version`: `"the
    ///    control plane speaks 4, which is more than one step above the 2 the
    ///    last release spoke"`.
    ///
    /// The first failing check is the one reported. A surface whose
    /// `released_version` is `None` runs check 1 only.
    #[must_use]
    pub fn find_version_problem(&self) -> Option<String> {
        if self.minimum_version > self.maximum_version {
            return Some(format!(
                "the {} accepts {} at the lowest and {} at the highest, which is no version at all",
                self.surface_name, self.minimum_version, self.maximum_version
            ));
        }
        let released_version = self.released_version?;
        if self.maximum_version < released_version {
            return Some(format!(
                "the {} speaks {}, which is below the {} the last release spoke",
                self.surface_name, self.maximum_version, released_version
            ));
        }
        if self.maximum_version - released_version > 1 {
            return Some(format!(
                "the {} speaks {}, which is more than one step above the {} the last release spoke",
                self.surface_name, self.maximum_version, released_version
            ));
        }
        None
    }
}

#[cfg(test)]
mod tests;
