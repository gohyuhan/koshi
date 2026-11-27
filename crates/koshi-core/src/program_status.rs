//! The program status that a pane's program reports through OSC 7501, the
//! Program Status Protocol: the state the program is in, and what it needs
//! from a person while it is blocked.
//!
//! Example: `ESC ] 7501 ; state=blocked:kind=permission ESC \` names the state
//! [`ProgramStatusState::Blocked`] and the kind [`BlockedKind::Permission`].

/// The state a program reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramStatusState {
    /// `idle`: at rest, waiting for the person's next instruction.
    Idle,
    /// `working`: running on its own.
    Working,
    /// `done`: finished a piece of work that is ready to look at.
    Done,
    /// `blocked`: cannot continue until a person acts.
    Blocked,
    /// `error`: failed and stopped.
    Error,
}

/// What a blocked program needs from a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedKind {
    /// `permission`: approval to do something.
    Permission,
    /// `question`: an answer the person types.
    Question,
    /// `auth`: a login, a token or another credential.
    Auth,
}
