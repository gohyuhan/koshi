//! Bounded iTerm2 OSC 1337 image commands and output.
//!
//! The parser receives one complete OSC 1337 body, without the `1337;` prefix
//! and without the OSC terminator. For example, the body
//! `File=inline=1;width=1;height=1:...` produces one decoded image record.
//! Framing the surrounding OSC string remains the terminal parser's job.

mod capabilities;
mod encoder;
mod parser;

pub use capabilities::{
    iterm_feature_string_supports_file, iterm_feature_string_supports_sixel,
    ITERM_CAPABILITIES_QUERY,
};
pub use encoder::{EncodeError, Encoder, OutputOptions, MAX_ITERM_PACKET_BYTES};
pub use parser::{
    iterm_command_can_be_graphics, iterm_command_is_graphics, iterm_payload_started,
    parse_iterm_command, ItermTransfer,
};
