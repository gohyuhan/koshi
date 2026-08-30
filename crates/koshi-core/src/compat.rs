//! Every versioned surface koshi carries, in one table.
//!
//! A surface is anything two koshi builds must agree on to work together: a
//! wire protocol they speak, or a file one writes and another reads. Each
//! surface carries its own version number.
//!
//! # The cadence rule
//!
//! `max` moves in the same commit as the change that requires it. Three
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
//! The first such change after a release sets `max` to `released + 1`. `max`
//! then holds until the next release, however many further changes land: one
//! release cycle moves a surface one step at most.
//!
//! [`Surface::version_problem`] checks this rule, and a test runs the whole
//! table through it.

/// One versioned surface: what two builds must agree on, and the versions this
/// build speaks of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Surface {
    /// What this surface is called in plain words, e.g. `"session protocol"`.
    /// Used in the message a failing check prints.
    pub name: &'static str,
    /// The lowest version this build accepts. A peer whose highest is below it
    /// is refused.
    pub min: u32,
    /// The highest version this build speaks, and the one it uses when the peer
    /// speaks it too.
    pub max: u32,
    /// The version the last released koshi spoke of this surface, or `None`
    /// when no release has carried it.
    ///
    /// With `None`, [`Surface::version_problem`] checks only that `min` does
    /// not exceed `max`; `max` may hold any value.
    pub released: Option<u32>,
}

/// The session protocol: what an attached client and a session server speak
/// over that session's control socket.
///
/// `v0.1.0` spoke 1, `v0.2.0` and `v0.3.0` speak 2, and this build speaks 3.
/// The floor is 3: a peer that speaks 2 is refused at the handshake.
///
/// Three shapes differ between 2 and 3. This build writes 3's shape for each
/// of them on every connection, whatever version the peer settled on:
///
/// - A command naming a target client carries `target_client`. A peer speaking
///   2 has no field for it.
/// - A `HostWrite` event carries its bytes as one base64 string. 2 wrote a
///   list of numbers.
/// - Each entry of a layout split's `children` is the child node itself. 2
///   wrapped it in a `{"node": …}` record. Layout trees travel in the attach
///   reply and in the layout report.
///
/// Both readers still take either shape: a `HostWrite` holding a list of
/// numbers decodes, and so does a split child wrapped in a `{"node": …}`
/// record. [`RESUME_FORMAT`] reads back to 1, and a resume file written by an
/// earlier build carries the wrapped shape.
pub const SESSION_PROTOCOL: Surface = Surface {
    name: "session protocol",
    min: 3,
    max: 3,
    released: Some(2),
};

/// The control plane: what a caller and the router speak over the router's
/// socket.
///
/// `v0.2.0` speaks 1. `v0.3.0` and this build speak 2. The floor is 1.
pub const CONTROL_PROTOCOL: Surface = Surface {
    name: "control plane",
    min: 1,
    max: 2,
    released: Some(2),
};

/// The supervisor link: what a session server and the process holding its panes
/// speak.
///
/// `v0.3.0` and this build both speak 1. The supervisor end can be older than
/// the session server that reconnects to it.
pub const SUPERVISOR_PROTOCOL: Surface = Surface {
    name: "supervisor link",
    min: 1,
    max: 1,
    released: Some(1),
};

/// The remote access token store: the file this machine keeps its grants in.
///
/// `v0.3.0` and this build both write 1.
pub const TOKEN_STORE_FORMAT: Surface = Surface {
    name: "token store format",
    min: 1,
    max: 1,
    released: Some(1),
};

/// The remote doorway: what a client on another machine and this machine's TLS
/// listener speak before any session is reached.
///
/// `v0.3.0` and this build both speak 1. Its two ends are different machines.
/// The session protocol the two ends settle after the door opens is a separate
/// surface, [`SESSION_PROTOCOL`].
pub const REMOTE_PROTOCOL: Surface = Surface {
    name: "remote doorway",
    min: 1,
    max: 1,
    released: Some(1),
};

/// The saved server file: the servers a dialling machine has connected to,
/// with the secret and the pinned certificate fingerprint for each.
///
/// `v0.3.0` and this build both write 1. The file sits on the dialling
/// machine. This build reads what an older koshi saved.
pub const SAVED_SERVER_FORMAT: Surface = Surface {
    name: "saved server file format",
    min: 1,
    max: 1,
    released: Some(1),
};

/// The remote certificate file: the certificate and private key this machine
/// generated for its remote listener.
///
/// `v0.3.0` and this build both write 1. This build reads a certificate an
/// older build generated.
pub const REMOTE_CERTIFICATE_FORMAT: Surface = Surface {
    name: "remote certificate file format",
    min: 1,
    max: 1,
    released: Some(1),
};

/// The remote access record: the file saying the operator switched remote
/// access on for this machine.
///
/// `v0.3.0` and this build both write 1. This build reads a record an older
/// build wrote, and keeps the port open.
pub const REMOTE_ACCESS_MARK_FORMAT: Surface = Surface {
    name: "remote access record format",
    min: 1,
    max: 1,
    released: Some(1),
};

/// The resume file: the state a session server writes before it replaces its
/// own process image, and the next image reads back.
///
/// `v0.3.0` writes 2 and this build writes 3. Format 3 adds prompt metadata to
/// every terminal row, and writes each entry of a layout split's `children` as
/// the child node itself. 1 and 2 wrapped it in a `{"node": …}` record. This
/// build reads either shape, so a session server upgraded from `v0.3.0` keeps
/// its layout trees across the swap.
///
/// The build being installed states which formats it reads. The running server
/// reads that answer before it commits to the swap.
pub const RESUME_FORMAT: Surface = Surface {
    name: "resume file format",
    min: 1,
    max: 3,
    released: Some(2),
};

/// The config schema: the shape of the files under the config directory.
///
/// `v0.1.0`, `v0.2.0` and `v0.3.0` all write 1. A file naming an older version
/// is migrated forward before it is read.
pub const CONFIG_SCHEMA: Surface = Surface {
    name: "config schema",
    min: 1,
    max: 1,
    released: Some(1),
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
    /// Three checks, in this order. Each names this surface's [`name`](Self::name).
    ///
    /// 1. `min` exceeds `max`: `"the control plane accepts 4 at the lowest and
    ///    3 at the highest, which is no version at all"`.
    /// 2. `max` is below `released`: `"the control plane speaks 1, which is
    ///    below the 2 the last release spoke"`.
    /// 3. `max` is more than one above `released`: `"the control plane speaks
    ///    4, which is more than one step above the 2 the last release spoke"`.
    ///
    /// The first failing check is the one reported. A surface whose `released`
    /// is `None` runs check 1 only.
    #[must_use]
    pub fn version_problem(&self) -> Option<String> {
        if self.min > self.max {
            return Some(format!(
                "the {} accepts {} at the lowest and {} at the highest, which is no version at all",
                self.name, self.min, self.max
            ));
        }
        let released = self.released?;
        if self.max < released {
            return Some(format!(
                "the {} speaks {}, which is below the {} the last release spoke",
                self.name, self.max, released
            ));
        }
        if self.max - released > 1 {
            return Some(format!(
                "the {} speaks {}, which is more than one step above the {} the last release spoke",
                self.name, self.max, released
            ));
        }
        None
    }
}

#[cfg(test)]
mod tests;
