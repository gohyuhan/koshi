//! Terminal-owned Kitty command response handling.

use super::{GraphicsError, GraphicsOperation, ImageDisplay};

pub(crate) use koshi_kitty::{KittyCommand, KittyCommandKind, KittyDelete};

pub(super) fn attach_error_replies(
    events: &mut [Result<GraphicsOperation, GraphicsError>],
    display: &ImageDisplay,
) {
    if display.image_id.unwrap_or(0) == 0 && display.image_number.unwrap_or(0) == 0 {
        return;
    }
    for event in events {
        if let Err(error) = event {
            *event = Ok(GraphicsOperation::Failure {
                display: display.clone(),
                error: error.clone(),
            });
        }
    }
}
