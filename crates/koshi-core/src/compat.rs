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
/// - A frame's two revisions, an image placement's `is_available`, a close
///   command's `should_kill_process_tree`, and a terminal-too-small event's
///   cause are required. 4 read a message without them.
/// - The `MoveFloatingPane`, `SetPanePinned`, `SetPaneMinimized` and
///   `SetAllFloatingPanesMinimized` commands, the `NextFloatingPane` and
///   `PreviousFloatingPane` focus targets, and the `FloatingPaneMoved`,
///   `PanePinChanged` and `PaneMinimizedChanged` events in a command result,
///   exist in 5 only.
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
pub const SUPERVISOR_PROTOCOL: Surface = Surface {
    surface_name: "supervisor link",
    minimum_version: 2,
    maximum_version: 2,
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
pub const REMOTE_PROTOCOL: Surface = Surface {
    surface_name: "remote protocol",
    minimum_version: 2,
    maximum_version: 2,
    released_version: Some(2),
};

/// The saved server file: the servers a dialling machine has connected to,
/// with the secret and the pinned certificate fingerprint for each.
///
/// `v0.4.0` writes 1. `v0.5.0` writes 2. Format 1 is converted when the
/// dialling machine opens its saved-server store.
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
/// writes 5: format 5 adds each session's floating panes and each client's view
/// of them. Formats 4 and 5 use the declared field names in the saved records.
/// Formats 1 through 4 pass through the ordered migration steps before they are
/// restored.
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
/// `v0.4.0` writes 1. `v0.5.0` writes 2. `koshi resume-support`,
/// `koshi serve-session`, and `koshi serve-router` migrate version 1 files to
/// version 2 before they read config.
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
