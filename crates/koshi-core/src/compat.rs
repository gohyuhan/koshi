//! Every versioned surface koshi carries, in one table.
//!
//! A surface is anything two koshi builds must agree on to work together: a
//! wire protocol they speak, or a file one writes and another reads. Each
//! surface carries its own version number.
//!
//! # The cadence rule
//!
//! `maximum_version` moves in the same commit as the change that requires it. Three
//! changes require it:
//!
//! - An existing field changes its type.
//! - An existing field changes its meaning.
//! - A field is added that one side must not send until it knows the other
//!   reads it.
//!
//! Adding or removing a field that both sides still decode leaves `max` where
//! it is.
//!
//! The first such change after a release sets `maximum_version` to `released_version + 1`. `maximum_version`
//! then holds until the next release, however many further changes land: one
//! release cycle moves a surface one step at most.
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
/// `v0.1.0` spoke 1, `v0.2.0` and `v0.3.0` spoke 2, and `v0.4.0` spoke 3. This
/// build speaks 4. The floor is 4: a peer that speaks 3 is refused at the
/// handshake.
///
/// One shape differs between 3 and 4. This build writes version 4 on every
/// session connection:
///
/// - The keyboard request carries the whole event the client's terminal
///   reported: the key, whether it went down, repeated or came up, the shifted
///   and base-layout keys, the text, and all eight modifiers. 3 carried one
///   chord, which holds no event kind, no text, and neither lock modifier.
///
/// Four shapes differ between 2 and 3:
///
/// - A command naming a target client carries `target_client_id`. A peer
///   speaking 2 has no field for it.
/// - A `HostWrite` event carries its bytes as one base64 string. 2 wrote a
///   list of numbers.
/// - Each entry of a layout split's `children` is the child node itself. 2
///   wrapped it in a `{"node": …}` record. Layout trees travel in the attach
///   reply and in the layout report.
/// - Painted image placements name connection-local content identities. Their
///   RGBA records travel in bounded image-content events and remain cached
///   across unchanged frames. Version 2 had no terminal-image wire shape.
///
/// The session reader still accepts two shape-level encodings in stored data:
/// a `HostWrite` holding a list of numbers and a split child wrapped in a
/// `{"node": …}` record. Resume format 4 is the only resume format this build
/// reads.
pub const SESSION_PROTOCOL: Surface = Surface {
    surface_name: "session protocol",
    minimum_version: 4,
    maximum_version: 4,
    released_version: Some(3),
};

/// The control plane: what a caller and the router speak over the router's
/// socket.
///
/// `v0.4.0` speaks 2. `v0.5.0` speaks 3. The floor is 3.
pub const CONTROL_PROTOCOL: Surface = Surface {
    surface_name: "control plane",
    minimum_version: 3,
    maximum_version: 3,
    released_version: Some(2),
};

/// The supervisor link: what a session server and the process holding its panes
/// speak.
///
/// `v0.4.0` speaks 1. `v0.5.0` speaks 2. The floor is 2.
pub const SUPERVISOR_PROTOCOL: Surface = Surface {
    surface_name: "supervisor link",
    minimum_version: 2,
    maximum_version: 2,
    released_version: Some(1),
};

/// The remote access token store: the file this machine keeps its grants in.
///
/// `v0.4.0` writes 1. `v0.5.0` writes 2. The floor is 2.
pub const TOKEN_STORE_FORMAT: Surface = Surface {
    surface_name: "token store format",
    minimum_version: 2,
    maximum_version: 2,
    released_version: Some(1),
};

/// The remote doorway: what a client on another machine and this machine's TLS
/// listener speak before any session is reached.
///
/// `v0.4.0` speaks 1. `v0.5.0` speaks 2. The floor is 2. Its two ends are
/// different machines. The session protocol the two ends settle after the door
/// opens is a separate surface, [`SESSION_PROTOCOL`].
pub const REMOTE_PROTOCOL: Surface = Surface {
    surface_name: "remote doorway",
    minimum_version: 2,
    maximum_version: 2,
    released_version: Some(1),
};

/// The saved server file: the servers a dialling machine has connected to,
/// with the secret and the pinned certificate fingerprint for each.
///
/// `v0.4.0` writes 1. `v0.5.0` writes 2. The floor is 2. The file sits on
/// the dialling machine.
pub const SAVED_SERVER_FORMAT: Surface = Surface {
    surface_name: "saved server file format",
    minimum_version: 2,
    maximum_version: 2,
    released_version: Some(1),
};

/// The remote certificate file: the certificate and private key this machine
/// generated for its remote listener.
///
/// `v0.4.0` writes 1. `v0.5.0` writes 2. The floor is 2. A file with
/// format 1 is not read.
pub const REMOTE_CERTIFICATE_FORMAT: Surface = Surface {
    surface_name: "remote certificate file format",
    minimum_version: 2,
    maximum_version: 2,
    released_version: Some(1),
};

/// The remote access record: the file saying the operator switched remote
/// access on for this machine.
///
/// `v0.4.0` writes 1. `v0.5.0` writes 2. The floor is 2. A file with format 1
/// is not read.
pub const REMOTE_ACCESS_MARK_FORMAT: Surface = Surface {
    surface_name: "remote access record format",
    minimum_version: 2,
    maximum_version: 2,
    released_version: Some(1),
};

/// The resume file: the state a session server writes before it replaces its
/// own process image, and the next image reads back.
///
/// `v0.3.0` writes 2, `v0.4.0` writes 3, and `v0.5.0` writes 4. Format 4
/// uses the declared field names in the saved records. The floor is 4.
///
/// The build being installed states which format it writes. The running server
/// reads that answer before it commits to the swap.
pub const RESUME_FORMAT: Surface = Surface {
    surface_name: "resume file format",
    minimum_version: 4,
    maximum_version: 4,
    released_version: Some(3),
};

/// The config schema: the shape of the files under the config directory.
///
/// `v0.4.0` writes 1. `v0.5.0` writes 2. The floor is 2.
pub const CONFIG_SCHEMA: Surface = Surface {
    surface_name: "config schema",
    minimum_version: 2,
    maximum_version: 2,
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
    REMOTE_ACCESS_MARK_FORMAT,
    RESUME_FORMAT,
    CONFIG_SCHEMA,
];

impl Surface {
    /// Why this surface's numbers break the cadence rule, or `None` when they
    /// follow it.
    ///
    /// Three checks, in this order. Each names this surface's
    /// [`surface_name`](Self::surface_name).
    ///
    /// 1. `minimum_version` exceeds `maximum_version`: `"the control plane accepts 4 at the lowest and
    ///    3 at the highest, which is no version at all"`.
    /// 2. `maximum_version` is below `released_version`: `"the control plane speaks 1, which is
    ///    below the 2 the last release spoke"`.
    /// 3. `maximum_version` is more than one above `released_version`: `"the control plane speaks
    ///    4, which is more than one step above the 2 the last release spoke"`.
    ///
    /// The first failing check is the one reported. A surface whose `released`
    /// is `None` runs check 1 only.
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
