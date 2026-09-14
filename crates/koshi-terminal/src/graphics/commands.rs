//! Terminal-owned Kitty command response handling.

use super::{GraphicsError, GraphicsOperation, ImageDisplay};

pub(crate) use koshi_kitty::{KittyCommand, KittyCommandKind, KittyDelete};

pub(super) fn attach_error_replies(
    graphics_events: &mut [Result<GraphicsOperation, GraphicsError>],
    image_display: &ImageDisplay,
) {
    if image_display.image_id.unwrap_or(0) == 0 && image_display.image_number.unwrap_or(0) == 0 {
        return;
    }
    for graphics_event in graphics_events {
        if let Err(graphics_error) = graphics_event {
            *graphics_event = Ok(GraphicsOperation::Failure {
                image_display: image_display.clone(),
                graphics_error: graphics_error.clone(),
            });
        }
    }
}
