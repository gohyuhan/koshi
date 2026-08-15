//! Every versioned surface koshi carries, in one table.
//!
//! A *surface* is anything two koshi builds must agree on to work together: a
//! wire protocol they speak, or a file one writes and another reads. Each one
//! carries its own number, and each number follows the same cadence rule.
//!
//! # The cadence rule
//!
//! A surface's high version moves when an existing field changes its type or
//! its meaning, in the same commit as that change. A field added or removed
//! with both sides still decoding cleanly does not move it. The first such
//! change after a release sets the number to the released value plus one, and
//! the number then holds until the next release — so one release cycle moves a
//! surface at most one step, however many changes it takes.
//!
//! Example — the control plane spoke 1 in `v0.2.0`. Two later changes each
//! bumped it, reaching 3, and neither was a meaning change. Folding those two
//! non-qualifying bumps put it back at 2: the released anchor of 1, plus the
//! one change that did qualify.
//!
//! [`Surface::version_problem`] checks that rule, and a test walks the whole
//! table through it. Before that check existed the rule lived only in prose,
//! and two surfaces carried a wrong number until somebody re-read the
//! paragraph.
//!
//! # What is here and what is not
//!
//! The table names each surface and the versions this build speaks. *Why* a
//! number last moved stays in the doc comment of the constant that exports it,
//! next to the code the number describes.

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
    /// when no release has carried it yet.
    ///
    /// `None` means the rule has nothing to anchor to: the surface may hold any
    /// value until it ships, because no released build can disagree with it.
    pub released: Option<u32>,
}

/// The session protocol: what an attached client and a session server speak
/// over that session's control socket.
///
/// `v0.1.0` spoke 1 and `v0.2.0` speaks 2. The floor is 2 because version 1 has
/// no attach and puts nothing user-visible on the socket, so no version-1 peer
/// has anything to ask a session server for.
pub const SESSION_PROTOCOL: Surface = Surface {
    name: "session protocol",
    min: 2,
    max: 2,
    released: Some(2),
};

/// The control plane: what a caller and the router speak over the router's
/// socket.
///
/// The router is born in `v0.2.0`, which speaks 1, so the floor is 1 and no
/// earlier build has a router to talk to.
pub const CONTROL_PROTOCOL: Surface = Surface {
    name: "control plane",
    min: 1,
    max: 2,
    released: Some(1),
};

/// The supervisor link: what a session server and the process holding its panes
/// speak.
///
/// Born after `v0.2.0` and not yet in a release, so it has no anchor. A
/// supervisor keeps running the image it started from, so an updated session
/// server can be newer than the supervisor it reconnects to.
pub const SUPERVISOR_PROTOCOL: Surface = Surface {
    name: "supervisor link",
    min: 1,
    max: 1,
    released: None,
};

/// The remote access token store: the file this machine keeps its grants in.
///
/// Born after `v0.2.0` and not yet in a release, so it has no anchor.
pub const TOKEN_STORE_FORMAT: Surface = Surface {
    name: "token store format",
    min: 1,
    max: 1,
    released: None,
};

/// The remote doorway: what a client on another machine and this machine's TLS
/// listener speak before any session is reached.
///
/// Born after `v0.2.0` and not yet in a release, so it has no anchor. It is
/// its own surface because the two ends are different machines, which upgrade
/// on their own schedules — the session protocol they settle after the door
/// opens is a separate agreement with a separate number.
pub const REMOTE_PROTOCOL: Surface = Surface {
    name: "remote doorway",
    min: 1,
    max: 1,
    released: None,
};

/// The saved server file: the servers a dialling machine has connected to,
/// with the secret and the pinned certificate fingerprint for each.
///
/// Born after `v0.2.0` and not yet in a release, so it has no anchor. It sits
/// on the dialling machine, so an upgraded koshi reads what an older one
/// saved.
pub const SAVED_SERVER_FORMAT: Surface = Surface {
    name: "saved server file format",
    min: 1,
    max: 1,
    released: None,
};

/// The remote certificate file: the certificate and private key this machine
/// generated for its remote listener.
///
/// Born after `v0.2.0` and not yet in a release, so it has no anchor. The
/// certificate outlives the build that made it, so a later build reads it.
pub const REMOTE_CERTIFICATE_FORMAT: Surface = Surface {
    name: "remote certificate file format",
    min: 1,
    max: 1,
    released: None,
};

/// The remote access record: the file saying the operator switched remote
/// access on for this machine.
///
/// Born after `v0.2.0` and not yet in a release, so it has no anchor. The
/// record outlives the build that wrote it, so a later build reads it and
/// keeps the port open.
pub const REMOTE_ACCESS_MARK_FORMAT: Surface = Surface {
    name: "remote access record format",
    min: 1,
    max: 1,
    released: None,
};

/// The resume file: the state a session server writes before it replaces its
/// own process image, and the next image reads back.
///
/// Born after `v0.2.0` and not yet in a release, so it has no anchor. The
/// build being installed states which formats it reads before the running
/// server commits to the swap.
pub const RESUME_FORMAT: Surface = Surface {
    name: "resume file format",
    min: 1,
    max: 2,
    released: None,
};

/// The config schema: the shape of the files under the config directory.
///
/// `v0.1.0` and `v0.2.0` both write 1. A file naming an older version is
/// migrated forward before it is read.
pub const CONFIG_SCHEMA: Surface = Surface {
    name: "config schema",
    min: 1,
    max: 1,
    released: Some(1),
};

/// Every versioned surface this build carries.
///
/// A new surface is added here in the same change that gives it a number, or
/// the check does not see it.
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
    /// Why this surface's numbers break the rule above, or `None` when they
    /// follow it.
    ///
    /// Two rules, both checked: the floor never rises above the ceiling, and a
    /// surface that has shipped sits either at its released value or one step
    /// above it. A surface no release has carried is checked on the first rule
    /// only.
    ///
    /// Example — a surface released at 2 that reads `max: 4` is reported as
    /// `"the control plane speaks 4, which is more than one step above the 2
    /// the last release spoke"`.
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
        if self.max > released + 1 {
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
