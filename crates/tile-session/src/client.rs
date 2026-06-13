//! Attached clients: the per-client view state of one session.
//!
//! A session accepts several clients at once. Focus, viewport, and input
//! modes are per-client so two attached terminals never fight over one
//! global cursor; the session itself holds only this registry.

/// The clients currently attached to one session. Placeholder shell: the
/// client model fills in the client type and the attach/detach operations.
#[derive(Debug)]
pub struct ClientRegistry;
