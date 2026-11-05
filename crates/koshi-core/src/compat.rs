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
//!
//! The release commit sets each `released_version` to its `maximum_version`.
//! On a `vX.Y.Z` tag, `release.yml` runs the ignored test
//! `every_surface_records_this_release_as_released`, which fails on a surface
//! whose `released_version` is not its `maximum_version`. A pre-release tag,
//! such as `v0.7.0-pr.1`, skips that test.

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
/// `v0.1.0` spoke 1, `v0.2.0` and `v0.3.0` spoke 2, `v0.4.0` spoke 3, `v0.5.0`
/// spoke 4, and `v0.6.0` speaks 5. This build speaks 6. The floor is 6: a peer
/// that speaks 5 is refused at the handshake.
///
/// The following shapes differ between 5 and 6. This build writes version 6 on
/// every session connection:
///
/// - The `SetFloatingPanePinned`, `ToggleFloatingPanePinned`,
///   `SetFloatingPaneMinimized` and `ToggleFloatingPaneMinimized` commands
///   carry the fields that 5's `SetPanePinned`, `TogglePanePinned`,
///   `SetPaneMinimized` and `TogglePaneMinimized` carried. Each acts on the
///   floating pane that holds the pane it names. 6 has none of the four names
///   of 5.
/// - The `FloatingPaneMoved`, `FloatingPanePinChanged` and
///   `FloatingPaneMinimizedChanged` events in a command result carry
///   `pane_ids`: every pane of the floating pane, in layout order. 5 carried
///   one `pane_id`, and named the last two `PanePinChanged` and
///   `PaneMinimizedChanged`.
/// - A floating pane of a `Layout` answer carries `pane_ids`: every pane of the
///   floating pane, in layout order. 5 carried one `pane_id`.
/// - A tab of a painted frame carries its solved tree in `layout_area`:
///   `solved_size`, `pane_slots`, `stack_headers`, `layout_mode`,
///   `is_every_pane_suppressed` and `gap_cell_count`. 5 carried these fields
///   on the tab, with `tab_size` in place of `solved_size`, and read a missing
///   `gap_cell_count` as `0`. 6 refuses a frame with no `gap_cell_count`.
/// - A floating pane slot of a painted frame carries `layout_area`: the
///   floating pane's tree as the viewing client draws it. A slot whose
///   `layout_area` holds no pane slot, or no slot of the slot's `pane_id`,
///   fails to decode.
/// - A minimized floating pane of a painted frame carries `pane_count`: the
///   panes the floating pane holds. A frame with `0` fails to decode.
/// - Each floating pane slot and each shown or minimized floating pane of a
///   painted frame carries `floating_pane_id`, the floating pane's id inside
///   its session. A frame without it fails to decode.
/// - The `CloseFloatingPane` command closes every pane of the floating pane
///   that holds the pane it names.
/// - The `FloatingPaneLayoutChanged` event in a command result carries
///   `pane_ids`: every pane of a floating pane whose tree, or one client's
///   zoom or shown stack member in it, changed, in layout order.
/// - The `DockFloatingPane` command docks every pane of the floating pane
///   that holds the pane it names into a tab, as one subtree with its split
///   weights. 5 has no such command.
/// - A `PlacePane` command with a `Floating` target moves a pane of a
///   floating pane of 2 or more panes into a new floating pane. 5 refused
///   every pane of a floating pane.
/// - The `PanePlacementCommitted` event, in a command result and on the
///   session's event stream, carries `source_pane_ids`: every pane the
///   placement takes from its source, in layout order. 5 carried one
///   `source_pane_id`.
///
/// A restart request the session reads and refuses is answered with the
/// `RequestFailed` code and a sentence naming what stopped the restart. A
/// `v0.5.0-pr.1` session, which also speaks 4, and a `v0.4.0` session answer
/// it with the `MalformedRequest` code and the same kind of sentence.
pub const SESSION_PROTOCOL: Surface = Surface {
    surface_name: "session protocol",
    minimum_version: 6,
    maximum_version: 6,
    released_version: Some(5),
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
/// `v0.4.0` speaks 1, `v0.5.0` speaks 2, and `v0.6.0` speaks 3. The floor is 2.
///
/// Version 3 gives each pane an output window: the supervisor sends at most
/// that many bytes of one pane's output ahead of the session's
/// `GrantPaneOutput` requests. Each pane's output travels as a binary frame.
pub const SUPERVISOR_PROTOCOL: Surface = Surface {
    surface_name: "supervisor link",
    minimum_version: 2,
    maximum_version: 3,
    released_version: Some(3),
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
/// `v0.3.0` writes 2, `v0.4.0` writes 3, `v0.5.0` writes 4, and `v0.6.0` writes
/// 5. This build writes 6: format 6 holds each floating pane as a floating pane
/// id, a layout tree, a desired size and a solved size, where format 5 held one
/// pane id with the two sizes. Each client's view of a floating pane, each
/// client's shown floating panes and floating focus, and each detached client's
/// saved view are keyed by floating pane id, and each view holds the client's
/// focus history inside the floating pane and its zoom there. Each tab holds
/// its layout tree and solved size in `layout_area`. A session of format 5
/// keeps its clients, and the body keeps its saved views and resume-token
/// hashes. Format 5 adds each session's floating panes, each client's view of
/// them, each session's starting tab, each tab's solved tab size, the views
/// detached clients left with the resume-token hashes, and the process IDs of
/// the children closed panes left running. It holds a focus history for each
/// client in each tab, where format 4 held one focused pane, and drops each
/// pane record's lifecycle and close policy. A session of format 4 or older
/// comes back with no client. Format 5 layout trees hold resize deltas that the
/// solve applies after the `preferred` targets, and formats 1 through 4 hold
/// resize deltas applied before them: a restored body of format 4 or older has
/// its resize deltas rewritten from the terminal sizes the header carries. A
/// tab whose sizes give back the rect it was last drawn at keeps every pane's
/// size. In every other tab, the resize delta of each child with a `preferred`
/// target moves onto its trailing-most flexible sibling. Format 5 also adds
/// each session's notification queue, and drops each session's
/// `is_recovery_notice_visible`. A pane's held shell-integration finish loses
/// the exit code it carried in format 4. A format 4 session gets a queue in a
/// new epoch, which holds one `Error` entry when its
/// `is_recovery_notice_visible` was `true`, and is empty otherwise. Formats 4
/// through 6 use the declared field names in the saved records. Formats 1
/// through 5 pass through the ordered migration steps before they are restored.
///
/// The build being installed states which format it writes. The running server
/// reads that answer before it commits to the swap.
pub const RESUME_FORMAT: Surface = Surface {
    surface_name: "resume file format",
    minimum_version: 1,
    maximum_version: 6,
    released_version: Some(5),
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
