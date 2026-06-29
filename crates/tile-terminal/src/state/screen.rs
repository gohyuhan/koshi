//! Which of the two screen buffers is active.

/// Which of the two screen buffers is currently active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    /// The normal, scrolling screen.
    #[default]
    Primary,
    /// The alternate screen used by full-screen apps (e.g. vim, htop).
    Alternate,
}
