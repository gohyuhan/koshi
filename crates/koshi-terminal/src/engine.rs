//! The per-pane terminal engine: a VTE parser paired with the
//! [`TerminalState`] it drives.
//!
//! One [`TerminalEngine`] backs one pane. PTY output arrives in read-sized
//! chunks that can split an escape sequence or a multi-byte UTF-8 code point
//! at any byte; the parser is the state machine that carries such a partial
//! decode from one chunk to the next, so it lives exactly as long as the
//! pane's screen state. Bundling the pair keeps a pane's decoder and screen
//! model one unit under one map entry in the runtime. Each
//! [`advance`](TerminalEngine::advance) call also hands back the reply bytes
//! the chunk's device queries produced, for the caller to write into the PTY.

use koshi_core::process::PtySize;

use crate::state::TerminalState;

/// One pane's emulation engine: the byte decoder and the screen model it
/// feeds.
pub struct TerminalEngine {
    /// The VTE state machine. Holds any partial escape sequence or split
    /// UTF-8 code point between [`advance`](TerminalEngine::advance) calls.
    parser: vte::Parser,
    /// The screen model the parser's decoded actions mutate.
    state: TerminalState,
}

impl TerminalEngine {
    /// An engine for a fresh pane of `size`: an idle parser and a blank
    /// [`TerminalState`].
    pub fn new(size: PtySize) -> Self {
        TerminalEngine {
            parser: vte::Parser::new(),
            state: TerminalState::new(size),
        }
    }

    /// Feed one chunk of PTY output through the parser into the state, and
    /// return the reply bytes any device queries in the chunk produced
    /// (DA/DSR/DECRQM answers — empty when the chunk held no query). The
    /// caller writes the replies back into the pane's PTY.
    ///
    /// Chunks may split an escape sequence or a UTF-8 code point at any byte;
    /// the parser resumes the partial decode on the next call.
    #[must_use = "undelivered replies hang the querying app"]
    pub fn advance(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.parser.advance(&mut self.state, bytes);
        self.state.take_replies()
    }

    /// The screen model, for reads (rendering, cursor and mode queries).
    pub fn state(&self) -> &TerminalState {
        &self.state
    }

    /// Resize the screen model to `size` (see [`TerminalState::resize`]).
    ///
    /// The parser keeps any partial decode: a sequence split across the
    /// resize still completes.
    pub fn resize(&mut self, size: PtySize) {
        self.state.resize(size);
    }
}

#[cfg(test)]
mod tests;
