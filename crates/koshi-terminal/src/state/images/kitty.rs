//! Retained Kitty uploads, placement commands, deletion, and replies.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use koshi_image::{
    decode_base64, decode_png, AnimationFrame, DecodedAnimation, DecodedImage, FrameDelay,
    LoopPolicy,
};

use crate::graphics::{
    GraphicsProtocol, ImageAction, ImageDisplay, ImageRecord, KittyCommand, KittyCommandKind,
    KittyDelete,
};
use crate::state::{Screen, TerminalState};

use super::{
    add_image_storage_byte_count, get_kitty_image_id, ImageContent, ImageContentId,
    ImagePlacementError, ImagePlacementId, KittyImage, MAX_IMAGE_PLACEMENT_COUNT,
    MAX_IMAGE_STORAGE_BYTE_COUNT,
};

impl TerminalState {
    pub(crate) fn apply_kitty_command(
        &mut self,
        kitty_command: &KittyCommand,
    ) -> Result<(), ImagePlacementError> {
        match kitty_command.get_command_kind() {
            KittyCommandKind::Place => {
                if kitty_command.get_image_display().is_unicode_placeholder {
                    let placement_result =
                        self.apply_virtual_kitty_image_placement(kitty_command.get_image_display());
                    let image_display_reply =
                        self.get_kitty_reply_display(kitty_command.get_image_display());
                    self.reply_to_kitty(
                        &image_display_reply,
                        placement_result.as_ref().err().copied(),
                        true,
                    );
                    return placement_result;
                }
                let kitty_upload = self.find_kitty_image_upload(kitty_command.get_image_display());
                let placement_result = kitty_upload
                    .ok_or(ImagePlacementError::ImageNotFound {
                        image_id: kitty_command.get_image_display().image_id,
                        image_number: kitty_command.get_image_display().image_number,
                    })
                    .and_then(|kitty_upload| {
                        let mut image_display = kitty_command.get_image_display().clone();
                        image_display.image_id = kitty_upload.display.image_id;
                        let image_record = ImageRecord {
                            protocol: GraphicsProtocol::Kitty,
                            image: Arc::clone(&kitty_upload.image),
                            animation: kitty_upload.animation.clone(),
                            action: ImageAction::Display,
                            display: image_display,
                            anchor: self.get_active_cursor_position(),
                        };
                        self.apply_image_record(&image_record)
                    });
                let image_display_reply =
                    self.get_kitty_reply_display(kitty_command.get_image_display());
                self.reply_to_kitty(
                    &image_display_reply,
                    placement_result.as_ref().err().copied(),
                    true,
                );
                placement_result
            }
            KittyCommandKind::Delete(selector) => {
                self.delete_kitty_placements(kitty_command, selector);
                Ok(())
            }
            KittyCommandKind::AnimationFrame | KittyCommandKind::AnimationCompose => {
                let animation_result = self.apply_kitty_animation_command(kitty_command);
                let image_display_reply =
                    self.get_kitty_reply_display(kitty_command.get_image_display());
                self.reply_to_kitty(
                    &image_display_reply,
                    animation_result.as_ref().err().copied(),
                    true,
                );
                animation_result
            }
            KittyCommandKind::AnimationControl | KittyCommandKind::AnimationDelete => {
                self.apply_kitty_animation_command(kitty_command)
            }
        }
    }

    pub(super) fn apply_virtual_kitty_image_placement(
        &mut self,
        image_display: &ImageDisplay,
    ) -> Result<(), ImagePlacementError> {
        if image_display.relative_image_id.is_some()
            || image_display.relative_placement_id.is_some()
            || image_display.relative_column_offset != 0
            || image_display.relative_row_offset != 0
        {
            return Err(ImagePlacementError::VirtualRelative);
        }
        if image_display.requested_column_count.is_none()
            || image_display.requested_row_count.is_none()
        {
            return Err(ImagePlacementError::MissingCellDimensions {
                requested_width: image_display.requested_width,
                requested_height: image_display.requested_height,
            });
        }
        let kitty_upload = self.find_kitty_image_upload(image_display).ok_or(
            ImagePlacementError::ImageNotFound {
                image_id: image_display.image_id,
                image_number: image_display.image_number,
            },
        )?;
        let mut virtual_image_display = image_display.clone();
        virtual_image_display.image_id = kitty_upload.display.image_id;
        virtual_image_display.placement_id = virtual_image_display
            .placement_id
            .filter(|candidate_placement_id| *candidate_placement_id != 0);
        let image_record = Arc::new(ImageRecord {
            protocol: GraphicsProtocol::Kitty,
            image: Arc::clone(&kitty_upload.image),
            animation: kitty_upload.animation.clone(),
            action: ImageAction::Display,
            display: virtual_image_display,
            anchor: self.get_active_cursor_position(),
        });
        super::raster::prepare_image_with_raster_plan(
            &image_record,
            self.cell_size,
            self.get_active_grid().get_grid_dimensions(),
        )?;
        let active_screen = self.active_screen;
        self.kitty_images.retain(|kitty_image| {
            !(kitty_image.is_virtual_placement
                && kitty_image.virtual_screen == Some(active_screen)
                && kitty_image.display.image_id == image_record.display.image_id
                && kitty_image.display.placement_id == image_record.display.placement_id)
        });
        if self.kitty_images.len() >= MAX_IMAGE_PLACEMENT_COUNT
            && !self.remove_unused_kitty_image_upload(None)
        {
            return Err(ImagePlacementError::TooManyPlacements {
                placement_count: self.kitty_images.len() + 1,
                placement_limit: MAX_IMAGE_PLACEMENT_COUNT,
            });
        }
        self.kitty_images.push(KittyImage {
            image_record,
            image_content: Arc::clone(&kitty_upload.image_content),
            is_virtual_placement: true,
            virtual_screen: Some(active_screen),
        });
        Ok(())
    }

    fn apply_kitty_animation_command(
        &mut self,
        kitty_command: &KittyCommand,
    ) -> Result<(), ImagePlacementError> {
        let animation_command = kitty_command
            .get_animation_command()
            .ok_or(ImagePlacementError::InvalidAnimationData)?;
        let kitty_image = self
            .find_kitty_image_upload(kitty_command.get_image_display())
            .ok_or(ImagePlacementError::ImageNotFound {
                image_id: kitty_command.get_image_display().image_id,
                image_number: kitty_command.get_image_display().image_number,
            })?;
        let image_content = Arc::clone(&kitty_image.image_content);
        if kitty_command.get_command_kind() == KittyCommandKind::AnimationDelete
            && image_content
                .animation
                .as_ref()
                .is_none_or(|animation| animation.get_frame_count() <= 1)
        {
            if kitty_command.should_free_image_data() {
                let image_id = kitty_image
                    .display
                    .image_id
                    .ok_or(ImagePlacementError::InvalidAnimationData)?;
                self.remove_kitty_image(image_id);
                self.kitty_images
                    .retain(|kitty_image| kitty_image.display.image_id != Some(image_id));
            }
            return Ok(());
        }
        let next_image_content = match kitty_command.get_command_kind() {
            KittyCommandKind::AnimationFrame => {
                self.apply_kitty_animation_frame(&image_content, animation_command)?
            }
            KittyCommandKind::AnimationControl => {
                self.apply_kitty_animation_control(&image_content, animation_command)?
            }
            KittyCommandKind::AnimationCompose => {
                self.apply_kitty_animation_compose(&image_content, animation_command)?
            }
            KittyCommandKind::AnimationDelete => {
                self.apply_kitty_animation_delete(&image_content, animation_command)?
            }
            KittyCommandKind::Place | KittyCommandKind::Delete(_) => {
                return Err(ImagePlacementError::InvalidAnimationData);
            }
        };
        self.install_kitty_image_content(image_content.image_content_id, next_image_content);
        Ok(())
    }

    fn apply_kitty_animation_frame(
        &self,
        image_content: &Arc<ImageContent>,
        kitty_command: &koshi_kitty::KittyAnimationCommand,
    ) -> Result<Arc<ImageContent>, ImagePlacementError> {
        let existing_animation = image_content.animation.as_ref();
        let mut animation_frames = existing_animation.map_or_else(
            || {
                vec![AnimationFrame::from_image_and_delay(
                    Arc::clone(&image_content.decoded_image),
                    build_zero_animation_frame_delay(),
                )
                .expect("the retained image has valid pixels")]
            },
            |animation| animation.list_frames().to_vec(),
        );
        let requested_frame_index = kitty_command
            .frame_number
            .map(|frame_number| {
                frame_number
                    .checked_sub(1)
                    .and_then(|frame_number| usize::try_from(frame_number).ok())
                    .ok_or(ImagePlacementError::AnimationFrameNotFound {
                        frame_index: frame_number,
                    })
            })
            .transpose()?
            .unwrap_or(animation_frames.len());
        let target_frame_index = requested_frame_index.min(animation_frames.len());
        let is_editing_existing_frame = target_frame_index < animation_frames.len();
        let base_decoded_image = if is_editing_existing_frame {
            animation_frames[target_frame_index].clone_decoded_image()
        } else if let Some(base_frame_number) = kitty_command.base_frame_number {
            let base_frame_index = base_frame_number
                .checked_sub(1)
                .and_then(|candidate_frame_number| usize::try_from(candidate_frame_number).ok())
                .ok_or(ImagePlacementError::AnimationFrameNotFound {
                    frame_index: base_frame_number,
                })?;
            animation_frames
                .get(base_frame_index)
                .map(AnimationFrame::clone_decoded_image)
                .ok_or(ImagePlacementError::AnimationFrameNotFound {
                    frame_index: base_frame_number,
                })?
        } else {
            Arc::new(DecodedImage {
                pixel_width: image_content.decoded_image.pixel_width,
                pixel_height: image_content.decoded_image.pixel_height,
                rgba_bytes: vec![
                    0;
                    usize::try_from(
                        image_content
                            .decoded_image
                            .pixel_width
                            .checked_mul(image_content.decoded_image.pixel_height)
                            .ok_or(ImagePlacementError::InvalidAnimationData)?
                    )
                    .ok()
                    .and_then(|pixel_count| pixel_count.checked_mul(4))
                    .ok_or(ImagePlacementError::InvalidAnimationData)?
                ],
            })
        };
        let animation_patch = decode_kitty_animation_payload(
            kitty_command,
            base_decoded_image.pixel_width,
            base_decoded_image.pixel_height,
        )?;
        let (image_pixel_width, image_pixel_height) = (
            base_decoded_image.pixel_width,
            base_decoded_image.pixel_height,
        );
        let source_pixel_x = kitty_command.source_x_pixels;
        let source_pixel_y = kitty_command.source_y_pixels;
        let source_right_pixel = source_pixel_x
            .checked_add(animation_patch.pixel_width)
            .ok_or(ImagePlacementError::InvalidAnimationData)?;
        let source_bottom_pixel = source_pixel_y
            .checked_add(animation_patch.pixel_height)
            .ok_or(ImagePlacementError::InvalidAnimationData)?;
        if source_right_pixel > image_pixel_width || source_bottom_pixel > image_pixel_height {
            return Err(ImagePlacementError::InvalidAnimationData);
        }
        let mut decoded_rgba_bytes =
            if let Some(background_rgba_bytes) = kitty_command.background_rgba_bytes {
                background_rgba_bytes
                    .into_iter()
                    .cycle()
                    .take(
                        usize::try_from(
                            image_pixel_width
                                .checked_mul(image_pixel_height)
                                .ok_or(ImagePlacementError::InvalidAnimationData)?,
                        )
                        .ok()
                        .and_then(|pixel_count| pixel_count.checked_mul(4))
                        .ok_or(ImagePlacementError::InvalidAnimationData)?,
                    )
                    .collect::<Vec<_>>()
            } else {
                base_decoded_image.rgba_bytes.clone()
            };
        compose_animation_frame_pixels(
            &mut decoded_rgba_bytes,
            image_pixel_width,
            &animation_patch,
            (source_pixel_x, source_pixel_y),
            kitty_command.replaces_destination_pixels,
        )?;
        let decoded_animation_frame_image = Arc::new(DecodedImage {
            pixel_width: image_pixel_width,
            pixel_height: image_pixel_height,
            rgba_bytes: decoded_rgba_bytes,
        });
        let animation_frame = match kitty_command.gap_milliseconds {
            Some(gap_milliseconds) if gap_milliseconds < 0 => {
                AnimationFrame::from_gapless_image(decoded_animation_frame_image)
            }
            Some(gap_milliseconds) => AnimationFrame::from_image_and_delay(
                decoded_animation_frame_image,
                build_animation_frame_delay_from_gap(gap_milliseconds)?,
            ),
            None => AnimationFrame::from_image_and_delay(
                decoded_animation_frame_image,
                animation_frames.get(target_frame_index).map_or_else(
                    build_default_animation_frame_delay,
                    AnimationFrame::get_frame_delay,
                ),
            ),
        }
        .map_err(|_| ImagePlacementError::InvalidAnimationData)?;
        if target_frame_index == animation_frames.len() {
            animation_frames.push(animation_frame);
        } else {
            animation_frames[target_frame_index] = animation_frame;
        }
        let loop_policy = existing_animation.map_or(LoopPolicy::Infinite, |animation| {
            animation.get_loop_policy()
        });
        let animation = Arc::new(
            DecodedAnimation::from_frames_and_loop_policy(animation_frames, loop_policy)
                .map_err(|_| ImagePlacementError::InvalidAnimationData)?,
        );
        let animation_frame_index = (image_content.animation_frame_index as usize)
            .min(animation.get_frame_count().saturating_sub(1));
        let mut next_image_content = (**image_content).clone();
        next_image_content.animation = Some(animation.clone());
        next_image_content.animation_frame_index =
            u32::try_from(animation_frame_index).unwrap_or(0);
        next_image_content.decoded_image =
            animation.list_frames()[animation_frame_index].clone_decoded_image();
        next_image_content.is_animation_running =
            image_content.is_animation_running || image_content.is_animation_loading;
        next_image_content.is_animation_loading = image_content.is_animation_loading;
        Ok(Arc::new(next_image_content))
    }

    fn apply_kitty_animation_control(
        &self,
        image_content: &Arc<ImageContent>,
        kitty_command: &koshi_kitty::KittyAnimationCommand,
    ) -> Result<Arc<ImageContent>, ImagePlacementError> {
        let current_animation = image_content
            .animation
            .as_ref()
            .ok_or(ImagePlacementError::InvalidAnimationData)?;
        let mut animation_frames = current_animation.list_frames().to_vec();
        if let Some(gap_milliseconds) = kitty_command.gap_milliseconds {
            let affected_frame_index = kitty_command.affected_frame_number.map_or(
                image_content.animation_frame_index as usize,
                |frame_number| frame_number.saturating_sub(1) as usize,
            );
            let animation_frame = animation_frames.get_mut(affected_frame_index).ok_or(
                ImagePlacementError::AnimationFrameNotFound {
                    frame_index: u32::try_from(affected_frame_index + 1).unwrap_or(u32::MAX),
                },
            )?;
            *animation_frame = if gap_milliseconds < 0 {
                AnimationFrame::from_gapless_image(animation_frame.clone_decoded_image())
            } else {
                AnimationFrame::from_image_and_delay(
                    animation_frame.clone_decoded_image(),
                    build_animation_frame_delay_from_gap(gap_milliseconds)?,
                )
            }
            .map_err(|_| ImagePlacementError::InvalidAnimationData)?;
        }
        let loop_policy = match kitty_command.loop_count {
            None | Some(0) => current_animation.get_loop_policy(),
            Some(1) => LoopPolicy::Infinite,
            Some(loop_count) => LoopPolicy::from_finite_playback_count(loop_count - 1)
                .map_err(|_| ImagePlacementError::InvalidAnimationData)?,
        };
        let animation = Arc::new(
            DecodedAnimation::from_frames_and_loop_policy(animation_frames, loop_policy)
                .map_err(|_| ImagePlacementError::InvalidAnimationData)?,
        );
        let animation_frame_index = kitty_command
            .frame_number
            .map(|frame_number| {
                frame_number
                    .checked_sub(1)
                    .and_then(|frame_number| usize::try_from(frame_number).ok())
                    .ok_or(ImagePlacementError::AnimationFrameNotFound {
                        frame_index: frame_number,
                    })
            })
            .transpose()?
            .unwrap_or(image_content.animation_frame_index as usize);
        if animation_frame_index >= animation.get_frame_count() {
            return Err(ImagePlacementError::AnimationFrameNotFound {
                frame_index: u32::try_from(animation_frame_index + 1).unwrap_or(u32::MAX),
            });
        }
        let mut next_image_content = (**image_content).clone();
        next_image_content.animation = Some(animation.clone());
        next_image_content.animation_frame_index =
            u32::try_from(animation_frame_index).unwrap_or(0);
        next_image_content.decoded_image =
            animation.list_frames()[animation_frame_index].clone_decoded_image();
        match kitty_command.playback_state {
            Some(1) => {
                next_image_content.is_animation_running = false;
                next_image_content.is_animation_loading = false;
                next_image_content.animation_loop_count = 0;
                next_image_content.animation_elapsed_nanos = 0;
            }
            Some(2) => {
                next_image_content.is_animation_running = true;
                next_image_content.is_animation_loading = true;
            }
            Some(3) => {
                next_image_content.is_animation_running = true;
                next_image_content.is_animation_loading = false;
            }
            None => {}
            Some(_) => return Err(ImagePlacementError::InvalidAnimationData),
        }
        Ok(Arc::new(next_image_content))
    }

    fn apply_kitty_animation_compose(
        &self,
        image_content: &Arc<ImageContent>,
        kitty_command: &koshi_kitty::KittyAnimationCommand,
    ) -> Result<Arc<ImageContent>, ImagePlacementError> {
        let current_animation = image_content
            .animation
            .as_ref()
            .ok_or(ImagePlacementError::InvalidAnimationData)?;
        let source_frame_index = kitty_command
            .source_frame_number
            .unwrap_or(1)
            .saturating_sub(1) as usize;
        let destination_frame_index = kitty_command
            .destination_frame_number
            .unwrap_or(1)
            .saturating_sub(1) as usize;
        let source_frame_image = current_animation
            .list_frames()
            .get(source_frame_index)
            .ok_or(ImagePlacementError::AnimationFrameNotFound {
                frame_index: u32::try_from(source_frame_index + 1).unwrap_or(u32::MAX),
            })?
            .get_decoded_image();
        let destination_frame_image = current_animation
            .list_frames()
            .get(destination_frame_index)
            .ok_or(ImagePlacementError::AnimationFrameNotFound {
                frame_index: u32::try_from(destination_frame_index + 1).unwrap_or(u32::MAX),
            })?
            .get_decoded_image();
        let composition_width_pixels = kitty_command.frame_width_pixels.unwrap_or(
            source_frame_image
                .pixel_width
                .saturating_sub(kitty_command.source_x_pixels),
        );
        let composition_height_pixels = kitty_command.frame_height_pixels.unwrap_or(
            source_frame_image
                .pixel_height
                .saturating_sub(kitty_command.source_y_pixels),
        );
        let source_right_pixel = kitty_command
            .source_x_pixels
            .checked_add(composition_width_pixels)
            .ok_or(ImagePlacementError::InvalidAnimationData)?;
        let source_bottom_pixel = kitty_command
            .source_y_pixels
            .checked_add(composition_height_pixels)
            .ok_or(ImagePlacementError::InvalidAnimationData)?;
        let destination_right_pixel = kitty_command
            .destination_x_pixels
            .checked_add(composition_width_pixels)
            .ok_or(ImagePlacementError::InvalidAnimationData)?;
        let destination_bottom_pixel = kitty_command
            .destination_y_pixels
            .checked_add(composition_height_pixels)
            .ok_or(ImagePlacementError::InvalidAnimationData)?;
        if composition_width_pixels == 0
            || composition_height_pixels == 0
            || source_right_pixel > source_frame_image.pixel_width
            || source_bottom_pixel > source_frame_image.pixel_height
            || destination_right_pixel > destination_frame_image.pixel_width
            || destination_bottom_pixel > destination_frame_image.pixel_height
        {
            return Err(ImagePlacementError::InvalidAnimationData);
        }
        if source_frame_index == destination_frame_index
            && kitty_command.source_x_pixels < destination_right_pixel
            && kitty_command.destination_x_pixels < source_right_pixel
            && kitty_command.source_y_pixels < destination_bottom_pixel
            && kitty_command.destination_y_pixels < source_bottom_pixel
        {
            return Err(ImagePlacementError::InvalidAnimationData);
        }
        let mut destination_rgba_bytes = destination_frame_image.rgba_bytes.clone();
        for row_index in 0..composition_height_pixels {
            for column_index in 0..composition_width_pixels {
                let source_byte_index = compute_rgba_byte_index(
                    source_frame_image.pixel_width,
                    kitty_command.source_x_pixels + column_index,
                    kitty_command.source_y_pixels + row_index,
                )?;
                let destination_byte_index = compute_rgba_byte_index(
                    destination_frame_image.pixel_width,
                    kitty_command.destination_x_pixels + column_index,
                    kitty_command.destination_y_pixels + row_index,
                )?;
                let source_pixel_rgba_bytes =
                    &source_frame_image.rgba_bytes[source_byte_index..source_byte_index + 4];
                if kitty_command.replaces_destination_pixels {
                    destination_rgba_bytes[destination_byte_index..destination_byte_index + 4]
                        .copy_from_slice(source_pixel_rgba_bytes);
                } else {
                    blend_animation_frame_pixel(
                        &mut destination_rgba_bytes
                            [destination_byte_index..destination_byte_index + 4],
                        source_pixel_rgba_bytes,
                    );
                }
            }
        }
        let composed_frame_image = Arc::new(DecodedImage {
            pixel_width: destination_frame_image.pixel_width,
            pixel_height: destination_frame_image.pixel_height,
            rgba_bytes: destination_rgba_bytes,
        });
        let destination_frame_delay =
            current_animation.list_frames()[destination_frame_index].get_frame_delay();
        let mut animation_frames = current_animation.list_frames().to_vec();
        animation_frames[destination_frame_index] =
            AnimationFrame::from_image_and_delay(composed_frame_image, destination_frame_delay)
                .map_err(|_| ImagePlacementError::InvalidAnimationData)?;
        let animation = Arc::new(
            DecodedAnimation::from_frames_and_loop_policy(
                animation_frames,
                current_animation.get_loop_policy(),
            )
            .map_err(|_| ImagePlacementError::InvalidAnimationData)?,
        );
        let mut next_image_content = (**image_content).clone();
        next_image_content.animation = Some(animation.clone());
        let animation_frame_index =
            (image_content.animation_frame_index as usize).min(animation.get_frame_count() - 1);
        next_image_content.decoded_image =
            animation.list_frames()[animation_frame_index].clone_decoded_image();
        Ok(Arc::new(next_image_content))
    }

    fn apply_kitty_animation_delete(
        &self,
        image_content: &Arc<ImageContent>,
        kitty_command: &koshi_kitty::KittyAnimationCommand,
    ) -> Result<Arc<ImageContent>, ImagePlacementError> {
        let current_animation = image_content
            .animation
            .as_ref()
            .ok_or(ImagePlacementError::InvalidAnimationData)?;
        let deleted_frame_index = kitty_command
            .frame_number
            .unwrap_or(1)
            .min(u32::try_from(current_animation.get_frame_count()).unwrap_or(u32::MAX))
            .saturating_sub(1) as usize;
        let mut animation_frames = current_animation.list_frames().to_vec();
        animation_frames.remove(deleted_frame_index);
        let mut next_image_content = (**image_content).clone();
        let selected_frame_index = image_content.animation_frame_index as usize;
        let selected_frame_index = if selected_frame_index > deleted_frame_index {
            selected_frame_index - 1
        } else if selected_frame_index == deleted_frame_index {
            deleted_frame_index.min(animation_frames.len().saturating_sub(1))
        } else {
            selected_frame_index
        };
        if animation_frames.len() == 1 {
            next_image_content.decoded_image = animation_frames[0].clone_decoded_image();
            next_image_content.animation = None;
            next_image_content.animation_frame_index = 0;
            next_image_content.animation_loop_count = 0;
            next_image_content.animation_elapsed_nanos = 0;
            next_image_content.is_animation_running = false;
            next_image_content.is_animation_loading = false;
            return Ok(Arc::new(next_image_content));
        }
        let animation = Arc::new(
            DecodedAnimation::from_frames_and_loop_policy(
                animation_frames,
                current_animation.get_loop_policy(),
            )
            .map_err(|_| ImagePlacementError::InvalidAnimationData)?,
        );
        let animation_frame_index =
            selected_frame_index.min(animation.get_frame_count().saturating_sub(1));
        next_image_content.animation = Some(animation.clone());
        next_image_content.animation_frame_index =
            u32::try_from(animation_frame_index).unwrap_or(0);
        next_image_content.decoded_image =
            animation.list_frames()[animation_frame_index].clone_decoded_image();
        Ok(Arc::new(next_image_content))
    }

    fn install_kitty_image_content(
        &mut self,
        image_content_id: ImageContentId,
        image_content: Arc<ImageContent>,
    ) {
        for image_placement in &mut self.primary_image_placements {
            if image_placement.image_content.image_content_id == image_content_id {
                super::apply_image_animation_content(image_placement, &image_content);
            }
        }
        for image_placement in &mut self.primary_image_history {
            if image_placement.image_content.image_content_id == image_content_id {
                super::apply_image_animation_history_content(image_placement, &image_content);
            }
        }
        for image_placement in &mut self.alternate_image_placements {
            if image_placement.image_content.image_content_id == image_content_id {
                super::apply_image_animation_content(image_placement, &image_content);
            }
        }
        for kitty_image in &mut self.kitty_images {
            if kitty_image.image_content.image_content_id != image_content_id {
                continue;
            }
            kitty_image.image_content = Arc::clone(&image_content);
            let mut image_record = kitty_image.image_record.as_ref().clone();
            image_record.image = Arc::clone(&image_content.decoded_image);
            image_record.animation = image_content.animation.clone();
            kitty_image.image_record = Arc::new(image_record);
        }
    }

    fn find_kitty_image_upload(&self, image_display: &ImageDisplay) -> Option<KittyImage> {
        self.kitty_images
            .iter()
            .rev()
            .find(|kitty_image| {
                !kitty_image.is_virtual_placement && {
                    if let Some(kitty_image_id) = image_display
                        .image_id
                        .filter(|candidate_image_id| *candidate_image_id != 0)
                    {
                        kitty_image.display.image_id == Some(kitty_image_id)
                    } else if let Some(image_number) = image_display
                        .image_number
                        .filter(|candidate_image_number| *candidate_image_number != 0)
                    {
                        kitty_image.display.image_number == Some(image_number)
                    } else {
                        false
                    }
                }
            })
            .cloned()
            .or_else(|| {
                self.primary_image_placements
                    .iter()
                    .chain(&self.alternate_image_placements)
                    .map(|image_placement| KittyImage {
                        image_record: Arc::clone(&image_placement.image_record),
                        image_content: Arc::clone(&image_placement.image_content),
                        is_virtual_placement: false,
                        virtual_screen: None,
                    })
                    .chain(
                        self.primary_image_history
                            .iter()
                            .map(|image_placement| KittyImage {
                                image_record: Arc::clone(&image_placement.image_record),
                                image_content: Arc::clone(&image_placement.image_content),
                                is_virtual_placement: false,
                                virtual_screen: None,
                            }),
                    )
                    .find(|kitty_image| {
                        kitty_image.protocol == GraphicsProtocol::Kitty
                            && image_display.image_id.is_some_and(|kitty_image_id| {
                                kitty_image_id != 0
                                    && kitty_image.display.image_id == Some(kitty_image_id)
                            })
                    })
            })
    }

    pub(super) fn retain_kitty_image_upload(
        &mut self,
        image_record: &mut ImageRecord,
    ) -> Result<(), ImagePlacementError> {
        if image_record.protocol != GraphicsProtocol::Kitty
            || !matches!(
                image_record.action,
                ImageAction::Transmit | ImageAction::TransmitAndDisplay
            )
        {
            return Ok(());
        }
        if image_record
            .display
            .image_number
            .is_some_and(|candidate_image_number| candidate_image_number != 0)
            && image_record.display.image_id.is_none()
        {
            let kitty_image_ids: HashSet<u32> = self
                .kitty_images
                .iter()
                .filter_map(|kitty_image| kitty_image.display.image_id)
                .collect();
            let kitty_image_id = (1..=u32::MAX)
                .find(|kitty_image_id| !kitty_image_ids.contains(kitty_image_id))
                .ok_or(ImagePlacementError::IdentityExhausted)?;
            image_record.display.image_id = Some(kitty_image_id);
        }
        let Some(kitty_image_id) = get_kitty_image_id(image_record) else {
            return Ok(());
        };
        self.remove_kitty_image(kitty_image_id);
        self.kitty_images
            .retain(|kitty_image| kitty_image.display.image_id != Some(kitty_image_id));
        if self.kitty_images.len() >= MAX_IMAGE_PLACEMENT_COUNT
            && !self.remove_unused_kitty_image_upload(None)
        {
            return Err(ImagePlacementError::TooManyPlacements {
                placement_count: self.kitty_images.len() + 1,
                placement_limit: MAX_IMAGE_PLACEMENT_COUNT,
            });
        }
        let image_record = Arc::new(image_record.clone());
        let image_content = self.allocate_image_content(Arc::clone(&image_record))?;
        self.kitty_images.push(KittyImage {
            image_content,
            image_record,
            is_virtual_placement: false,
            virtual_screen: None,
        });
        Ok(())
    }

    pub(super) fn ensure_image_storage(
        &mut self,
        requested_byte_count: usize,
        protected_kitty_image_id: Option<u32>,
    ) -> Result<(), ImagePlacementError> {
        loop {
            let used_byte_count = self.get_image_storage_byte_count();
            if used_byte_count <= MAX_IMAGE_STORAGE_BYTE_COUNT {
                return Ok(());
            }
            if !self.remove_unused_kitty_image_upload(protected_kitty_image_id) {
                return Err(ImagePlacementError::StorageLimit {
                    used_byte_count: used_byte_count.saturating_sub(requested_byte_count),
                    requested_byte_count,
                    byte_limit: MAX_IMAGE_STORAGE_BYTE_COUNT,
                });
            }
        }
    }

    fn list_referenced_kitty_image_ids(&self) -> HashSet<u32> {
        self.primary_image_placements
            .iter()
            .chain(&self.alternate_image_placements)
            .map(|image_placement| &image_placement.image_record)
            .chain(
                self.primary_image_history
                    .iter()
                    .map(|image_placement| &image_placement.image_record),
            )
            .chain(
                self.kitty_images
                    .iter()
                    .filter(|kitty_image| kitty_image.is_virtual_placement)
                    .map(|kitty_image| &kitty_image.image_record),
            )
            .filter_map(|image_record| get_kitty_image_id(image_record))
            .collect()
    }

    fn remove_unused_kitty_image_upload(&mut self, protected_kitty_image_id: Option<u32>) -> bool {
        let referenced_kitty_image_ids = self.list_referenced_kitty_image_ids();
        let candidate_kitty_image_index = self.kitty_images.iter().position(|kitty_image| {
            kitty_image.display.image_id.is_some_and(|kitty_image_id| {
                Some(kitty_image_id) != protected_kitty_image_id
                    && !referenced_kitty_image_ids.contains(&kitty_image_id)
            })
        });
        if let Some(kitty_image_index) = candidate_kitty_image_index {
            self.kitty_images.remove(kitty_image_index);
            true
        } else {
            false
        }
    }

    pub(super) fn get_image_storage_byte_count(&self) -> usize {
        let mut retained_image_content_ids = HashSet::<ImageContentId>::new();
        let mut retained_decoded_image_pointers = HashSet::new();
        let mut storage_byte_count = self.get_native_fragment_storage_byte_count();
        let mut add_image_content_storage = |image_content: &Arc<super::ImageContent>| {
            if retained_image_content_ids.insert(image_content.image_content_id) {
                add_image_storage_byte_count(
                    &mut storage_byte_count,
                    &mut retained_decoded_image_pointers,
                    image_content.decoded_image.as_ref(),
                );
                if let Some(animation) = &image_content.animation {
                    for animation_frame in animation.list_frames() {
                        add_image_storage_byte_count(
                            &mut storage_byte_count,
                            &mut retained_decoded_image_pointers,
                            animation_frame.get_decoded_image(),
                        );
                    }
                }
                if let Some(sixel_source) = &image_content.sixel {
                    storage_byte_count = storage_byte_count.saturating_add(
                        sixel_source
                            .get_sixel_storage_byte_count()
                            .unwrap_or(usize::MAX),
                    );
                }
            }
        };
        for kitty_image in &self.kitty_images {
            add_image_content_storage(&kitty_image.image_content);
        }
        for image_placement in self
            .primary_image_placements
            .iter()
            .chain(&self.alternate_image_placements)
            .chain(
                self.native_images
                    .iter()
                    .map(|native_image_source| &native_image_source.placement),
            )
        {
            add_image_content_storage(&image_placement.image_content);
        }
        for image_placement in &self.primary_image_history {
            add_image_content_storage(&image_placement.image_content);
        }

        for raster_image in self
            .primary_image_placements
            .iter()
            .chain(&self.alternate_image_placements)
            .chain(
                self.native_images
                    .iter()
                    .map(|native_image_source| &native_image_source.placement),
            )
            .filter_map(|image_placement| image_placement.raster.as_ref())
            .chain(
                self.primary_image_history
                    .iter()
                    .filter_map(|image_placement| image_placement.raster.as_ref()),
            )
        {
            add_image_storage_byte_count(
                &mut storage_byte_count,
                &mut retained_decoded_image_pointers,
                raster_image.as_ref(),
            );
        }
        storage_byte_count
    }

    pub(crate) fn reply_to_kitty(
        &mut self,
        image_display: &ImageDisplay,
        image_placement_error: Option<ImagePlacementError>,
        is_kitty_protocol: bool,
    ) {
        if !is_kitty_protocol {
            return;
        }
        let kitty_reply_message = match image_placement_error {
            None => "OK",
            Some(ImagePlacementError::ImageNotFound { .. }) => "ENOENT:image not found",
            Some(ImagePlacementError::ParentNotFound) => {
                "ENOPARENT:relative image parent not found"
            }
            Some(ImagePlacementError::RelativeCycle) => "ECYCLE:relative image cycle",
            Some(ImagePlacementError::RelativeDepth) => "ETOODEEP:relative image chain too deep",
            Some(ImagePlacementError::AnimationFrameNotFound { .. }) => {
                "ENOENT:animation frame not found"
            }
            Some(ImagePlacementError::UnsupportedPlacement) => {
                "ENOTSUP:unsupported image placement"
            }
            Some(
                ImagePlacementError::StorageLimit { .. }
                | ImagePlacementError::TooManyPlacements { .. },
            ) => "ENOSPC:image storage limit",
            Some(_) => "EINVAL:invalid image placement",
        };
        self.write_kitty_reply(
            image_display,
            kitty_reply_message,
            image_placement_error.is_some(),
        );
    }

    pub(crate) fn reply_to_kitty_failure(
        &mut self,
        image_display: &ImageDisplay,
        graphics_error: &crate::graphics::GraphicsError,
    ) {
        use crate::graphics::GraphicsError;
        let kitty_reply_message = match graphics_error {
            GraphicsError::UnsupportedAction { .. } | GraphicsError::UnsupportedMedia { .. } => {
                "ENOTSUP:unsupported image command"
            }
            GraphicsError::TransferTooLarge { .. } | GraphicsError::ImageTooLarge { .. } => {
                "ENOSPC:image storage limit"
            }
            _ => "EINVAL:invalid image data",
        };
        self.write_kitty_reply(image_display, kitty_reply_message, true);
    }

    fn write_kitty_reply(
        &mut self,
        image_display: &ImageDisplay,
        kitty_reply_message: &str,
        is_reply_error: bool,
    ) {
        if image_display.response_suppression_level == 2
            || (image_display.response_suppression_level == 1 && !is_reply_error)
        {
            return;
        }
        if image_display.image_id.unwrap_or(0) == 0 && image_display.image_number.unwrap_or(0) == 0
        {
            return;
        }
        let mut kitty_reply = format!("\x1b_Gi={}", image_display.image_id.unwrap_or(0));
        if let Some(image_number) = image_display.image_number {
            kitty_reply.push_str(&format!(",I={image_number}"));
        }
        if let Some(placement_id) = image_display
            .placement_id
            .filter(|candidate_placement_id| *candidate_placement_id != 0)
        {
            kitty_reply.push_str(&format!(",p={placement_id}"));
        }
        kitty_reply.push(';');
        kitty_reply.push_str(kitty_reply_message);
        kitty_reply.push_str("\x1b\\");
        self.device_query_replies
            .extend_from_slice(kitty_reply.as_bytes());
    }

    fn delete_kitty_placements(&mut self, kitty_command: &KittyCommand, selector: KittyDelete) {
        let resolved_kitty_image_id = self
            .find_kitty_image_upload(kitty_command.get_image_display())
            .and_then(|kitty_image| kitty_image.display.image_id);
        let cursor_position = self.get_active_cursor_position();
        let live_top_row_index = self.scrollback.get_total_pushed_line_count();
        let (grid_row_count, grid_column_count) = self.get_active_grid().get_grid_dimensions();
        let mut removed_image_ids = HashSet::new();
        let mut removed_kitty_placement_identities = HashSet::new();
        let mut image_ids_with_released_storage = HashSet::new();
        let mut relative_anchor_by_placement_id = HashMap::new();
        match self.active_screen {
            Screen::Primary => {
                for image_placement in &self.primary_image_placements {
                    if image_placement
                        .image_record
                        .display
                        .relative_image_id
                        .is_some()
                        || image_placement
                            .image_record
                            .display
                            .relative_placement_id
                            .is_some()
                    {
                        if let Ok(Some((anchor_row, anchor_column))) = self.resolve_relative_anchor(
                            &image_placement.image_record,
                            Some(image_placement.image_placement_id),
                        ) {
                            relative_anchor_by_placement_id.insert(
                                image_placement.image_placement_id,
                                (i128::from(anchor_row), anchor_column),
                            );
                        }
                    }
                }
                for image_placement in &self.primary_image_history {
                    if image_placement
                        .image_record
                        .display
                        .relative_image_id
                        .is_some()
                        || image_placement
                            .image_record
                            .display
                            .relative_placement_id
                            .is_some()
                    {
                        if let Ok(Some((anchor_row, anchor_column))) = self.resolve_relative_anchor(
                            &image_placement.image_record,
                            Some(image_placement.image_placement_id),
                        ) {
                            relative_anchor_by_placement_id.insert(
                                image_placement.image_placement_id,
                                (i128::from(anchor_row), anchor_column),
                            );
                        }
                    }
                }
            }
            Screen::Alternate => {
                for image_placement in &self.alternate_image_placements {
                    if image_placement
                        .image_record
                        .display
                        .relative_image_id
                        .is_some()
                        || image_placement
                            .image_record
                            .display
                            .relative_placement_id
                            .is_some()
                    {
                        if let Ok(Some((anchor_row, anchor_column))) = self.resolve_relative_anchor(
                            &image_placement.image_record,
                            Some(image_placement.image_placement_id),
                        ) {
                            relative_anchor_by_placement_id.insert(
                                image_placement.image_placement_id,
                                (i128::from(anchor_row), anchor_column),
                            );
                        }
                    }
                }
            }
        }
        let should_remove_all_image_placements =
            matches!(selector, KittyDelete::ImageId | KittyDelete::ImageNumber)
                && kitty_command
                    .get_image_display()
                    .placement_id
                    .is_none_or(|placement_id| placement_id == 0);
        let mut should_keep_kitty_placement =
            |image_record: &Arc<ImageRecord>,
             image_placement_id: ImagePlacementId,
             stored_anchor_row: i128,
             stored_anchor_column: u16,
             row_count: u16,
             column_count: u16| {
                if image_record.protocol != GraphicsProtocol::Kitty {
                    return true;
                }
                let delete_image_display = kitty_command.get_image_display();
                let is_relative = image_record.display.relative_image_id.is_some()
                    || image_record.display.relative_placement_id.is_some();
                let resolved_anchor_geometry = if is_relative {
                    relative_anchor_by_placement_id
                        .get(&image_placement_id)
                        .copied()
                } else {
                    Some((stored_anchor_row, i64::from(stored_anchor_column)))
                };
                let Some((anchor_row, anchor_column)) = resolved_anchor_geometry else {
                    return true;
                };
                let is_row_in_placement = |candidate_row: i128| {
                    anchor_row <= candidate_row
                        && candidate_row < anchor_row + i128::from(row_count)
                };
                let is_column_in_placement = |candidate_column: u32| {
                    let candidate_column = i64::from(candidate_column);
                    anchor_column <= candidate_column
                        && candidate_column < anchor_column + i64::from(column_count)
                };
                let source_pixel_offset_x = delete_image_display
                    .source_pixel_offset_x
                    .unwrap_or(1)
                    .saturating_sub(1);
                let source_pixel_offset_y =
                    i128::from(delete_image_display.source_pixel_offset_y.unwrap_or(1)) - 1;
                let is_matching_image_id = image_record.display.image_id == resolved_kitty_image_id
                    && resolved_kitty_image_id.is_some();
                let is_matching_placement_id =
                    delete_image_display
                        .placement_id
                        .is_none_or(|placement_id| {
                            placement_id == 0
                                || image_record.display.placement_id == Some(placement_id)
                        });
                let should_remove_placement = match selector {
                    KittyDelete::Visible => {
                        anchor_row < i128::from(grid_row_count)
                            && anchor_row + i128::from(row_count) > 0
                            && anchor_column < i64::from(grid_column_count)
                            && anchor_column + i64::from(column_count) > 0
                    }
                    KittyDelete::ImageId | KittyDelete::ImageNumber => {
                        is_matching_image_id && is_matching_placement_id
                    }
                    KittyDelete::Cursor => {
                        is_row_in_placement(i128::from(cursor_position.0))
                            && is_column_in_placement(u32::from(cursor_position.1))
                    }
                    KittyDelete::Cell => {
                        is_row_in_placement(source_pixel_offset_y)
                            && is_column_in_placement(source_pixel_offset_x)
                    }
                    KittyDelete::CellAtZ => {
                        is_row_in_placement(source_pixel_offset_y)
                            && is_column_in_placement(source_pixel_offset_x)
                            && image_record.display.z_index == delete_image_display.z_index
                    }
                    KittyDelete::ImageIdRange => {
                        image_record.display.image_id.is_some_and(|kitty_image_id| {
                            kitty_image_id
                                >= delete_image_display.source_pixel_offset_x.unwrap_or(0)
                                && kitty_image_id
                                    <= delete_image_display.source_pixel_offset_y.unwrap_or(0)
                        })
                    }
                    KittyDelete::Column => is_column_in_placement(source_pixel_offset_x),
                    KittyDelete::Row => is_row_in_placement(source_pixel_offset_y),
                    KittyDelete::ZIndex => {
                        image_record.display.z_index == delete_image_display.z_index
                    }
                };
                if should_remove_placement {
                    if let Some(kitty_image_id) = image_record.display.image_id {
                        image_ids_with_released_storage.insert(kitty_image_id);
                        if should_remove_all_image_placements
                            || image_record.display.placement_id.is_none()
                        {
                            removed_image_ids.insert(kitty_image_id);
                        } else if let Some(placement_id) = image_record
                            .display
                            .placement_id
                            .filter(|candidate_placement_id| *candidate_placement_id != 0)
                        {
                            removed_kitty_placement_identities
                                .insert((kitty_image_id, placement_id));
                        }
                    }
                }
                !should_remove_placement
            };
        match self.active_screen {
            Screen::Primary => {
                self.primary_image_placements.retain(|image_placement| {
                    should_keep_kitty_placement(
                        &image_placement.image_record,
                        image_placement.image_placement_id,
                        i128::from(image_placement.anchor.0),
                        image_placement.anchor.1,
                        image_placement.row_count,
                        image_placement.column_count,
                    )
                });
                self.primary_image_history.retain(|image_placement| {
                    should_keep_kitty_placement(
                        &image_placement.image_record,
                        image_placement.image_placement_id,
                        i128::from(image_placement.anchor.0) - i128::from(live_top_row_index),
                        image_placement.anchor.1,
                        image_placement.row_count,
                        image_placement.column_count,
                    )
                });
            }
            Screen::Alternate => self.alternate_image_placements.retain(|image_placement| {
                should_keep_kitty_placement(
                    &image_placement.image_record,
                    image_placement.image_placement_id,
                    i128::from(image_placement.anchor.0),
                    image_placement.anchor.1,
                    image_placement.row_count,
                    image_placement.column_count,
                )
            }),
        }
        self.remove_relative_dependents(
            &mut removed_image_ids,
            &mut removed_kitty_placement_identities,
        );
        self.kitty_images.retain(|kitty_image| {
            if !kitty_image.is_virtual_placement
                || kitty_image.virtual_screen != Some(self.active_screen)
            {
                return true;
            }
            let delete_image_display = kitty_command.get_image_display();
            let kitty_image_id = kitty_image.display.image_id;
            let is_matching_placement_id =
                delete_image_display
                    .placement_id
                    .is_none_or(|placement_id| {
                        placement_id == 0 || kitty_image.display.placement_id == Some(placement_id)
                    });
            let should_remove_virtual_placement = match selector {
                KittyDelete::ImageId | KittyDelete::ImageNumber => resolved_kitty_image_id
                    .is_some_and(|resolved_kitty_image_id| {
                        kitty_image_id == Some(resolved_kitty_image_id) && is_matching_placement_id
                    }),
                KittyDelete::ImageIdRange => kitty_image_id.is_some_and(|candidate_image_id| {
                    candidate_image_id >= delete_image_display.source_pixel_offset_x.unwrap_or(0)
                        && candidate_image_id
                            <= delete_image_display.source_pixel_offset_y.unwrap_or(0)
                }),
                _ => false,
            };
            if should_remove_virtual_placement {
                if let Some(kitty_image_id) = kitty_image_id {
                    image_ids_with_released_storage.insert(kitty_image_id);
                }
                if let Some(kitty_placement_identity) =
                    super::get_kitty_placement_identity(&kitty_image.image_record)
                {
                    removed_kitty_placement_identities.insert(kitty_placement_identity);
                } else if let Some(kitty_image_id) = kitty_image_id {
                    removed_image_ids.insert(kitty_image_id);
                }
            }
            !should_remove_virtual_placement
        });
        self.remove_relative_dependents(
            &mut removed_image_ids,
            &mut removed_kitty_placement_identities,
        );
        if kitty_command.should_free_image_data() {
            removed_image_ids.extend(image_ids_with_released_storage);
            if let Some(kitty_image_id) = resolved_kitty_image_id
                .filter(|_| matches!(selector, KittyDelete::ImageId | KittyDelete::ImageNumber))
            {
                removed_image_ids.insert(kitty_image_id);
            }
            if selector == KittyDelete::ImageIdRange {
                removed_image_ids.extend(
                    self.kitty_images
                        .iter()
                        .filter_map(|image_record| image_record.display.image_id)
                        .filter(|kitty_image_id| {
                            *kitty_image_id
                                >= kitty_command
                                    .get_image_display()
                                    .source_pixel_offset_x
                                    .unwrap_or(0)
                                && *kitty_image_id
                                    <= kitty_command
                                        .get_image_display()
                                        .source_pixel_offset_y
                                        .unwrap_or(0)
                        }),
                );
            }
            let referenced_kitty_image_ids = self.list_referenced_kitty_image_ids();
            self.kitty_images.retain(|kitty_image| {
                kitty_image.display.image_id.is_none_or(|kitty_image_id| {
                    !removed_image_ids.contains(&kitty_image_id)
                        || referenced_kitty_image_ids.contains(&kitty_image_id)
                })
            });
        }
    }

    fn remove_relative_dependents(
        &mut self,
        removed_image_ids: &mut HashSet<u32>,
        removed_kitty_placement_identities: &mut HashSet<(u32, u32)>,
    ) {
        loop {
            let mut has_removed_dependent = false;
            let mut should_retain_image_record = |image_record: &Arc<ImageRecord>| {
                let Some(parent_image_id) = image_record.display.relative_image_id else {
                    return true;
                };
                let parent_placement_id = image_record
                    .display
                    .relative_placement_id
                    .filter(|placement_id| *placement_id != 0);
                let is_parent_removed = removed_image_ids.contains(&parent_image_id)
                    || parent_placement_id.is_some_and(|placement_id| {
                        removed_kitty_placement_identities
                            .contains(&(parent_image_id, placement_id))
                    });
                if !is_parent_removed {
                    return true;
                }
                has_removed_dependent = true;
                if let Some(kitty_placement_identity) =
                    super::get_kitty_placement_identity(image_record)
                {
                    removed_kitty_placement_identities.insert(kitty_placement_identity);
                } else if let Some(kitty_image_id) = image_record.display.image_id {
                    removed_image_ids.insert(kitty_image_id);
                }
                false
            };
            self.primary_image_placements.retain(|image_placement| {
                should_retain_image_record(&image_placement.image_record)
            });
            self.primary_image_history.retain(|image_placement| {
                should_retain_image_record(&image_placement.image_record)
            });
            self.alternate_image_placements.retain(|image_placement| {
                should_retain_image_record(&image_placement.image_record)
            });
            if !has_removed_dependent {
                break;
            }
        }
    }

    pub(crate) fn get_kitty_reply_display(&self, image_display: &ImageDisplay) -> ImageDisplay {
        let mut image_display_reply = image_display.clone();
        if image_display_reply.image_id.is_none() {
            image_display_reply.image_id = self
                .find_kitty_image_upload(image_display)
                .and_then(|kitty_image| kitty_image.display.image_id);
        }
        image_display_reply
    }
}

fn build_zero_animation_frame_delay() -> FrameDelay {
    FrameDelay::from_millisecond_ratio(0, 1).expect("a one-millisecond denominator is valid")
}

fn build_default_animation_frame_delay() -> FrameDelay {
    FrameDelay::from_millisecond_ratio(40, 1).expect("a one-millisecond denominator is valid")
}

fn build_animation_frame_delay_from_gap(
    gap_milliseconds: i32,
) -> Result<FrameDelay, ImagePlacementError> {
    FrameDelay::from_millisecond_ratio(gap_milliseconds.max(0) as u32, 1)
        .map_err(|_| ImagePlacementError::InvalidAnimationData)
}

fn decode_kitty_animation_payload(
    kitty_command: &koshi_kitty::KittyAnimationCommand,
    fallback_width_pixels: u32,
    fallback_height_pixels: u32,
) -> Result<DecodedImage, ImagePlacementError> {
    let decoded_payload_bytes = decode_base64(
        GraphicsProtocol::Kitty,
        &kitty_command.encoded_payload_bytes,
    )
    .map_err(|_| ImagePlacementError::InvalidAnimationData)?;
    match kitty_command.media_format.unwrap_or(32) {
        24 => koshi_image::decode_raw_rgb(
            GraphicsProtocol::Kitty,
            kitty_command
                .frame_width_pixels
                .unwrap_or(fallback_width_pixels),
            kitty_command
                .frame_height_pixels
                .unwrap_or(fallback_height_pixels),
            &decoded_payload_bytes,
        )
        .map_err(|_| ImagePlacementError::InvalidAnimationData),
        32 => koshi_image::decode_raw_rgba(
            GraphicsProtocol::Kitty,
            kitty_command
                .frame_width_pixels
                .unwrap_or(fallback_width_pixels),
            kitty_command
                .frame_height_pixels
                .unwrap_or(fallback_height_pixels),
            &decoded_payload_bytes,
        )
        .map_err(|_| ImagePlacementError::InvalidAnimationData),
        100 => decode_png(GraphicsProtocol::Kitty, &decoded_payload_bytes)
            .map_err(|_| ImagePlacementError::InvalidAnimationData),
        _ => Err(ImagePlacementError::InvalidAnimationData),
    }
}

fn compute_rgba_byte_index(
    image_pixel_width: u32,
    pixel_x: u32,
    pixel_y: u32,
) -> Result<usize, ImagePlacementError> {
    let pixel_byte_index = pixel_y
        .checked_mul(image_pixel_width)
        .and_then(|pixel_count| pixel_count.checked_add(pixel_x))
        .and_then(|pixel_count| pixel_count.checked_mul(4))
        .and_then(|pixel_count| usize::try_from(pixel_count).ok())
        .ok_or(ImagePlacementError::InvalidAnimationData)?;
    Ok(pixel_byte_index)
}

fn compose_animation_frame_pixels(
    destination_rgba_bytes: &mut [u8],
    destination_pixel_width: u32,
    source_frame_image: &DecodedImage,
    destination_pixel_origin: (u32, u32),
    should_replace_destination_pixels: bool,
) -> Result<(), ImagePlacementError> {
    for row_index in 0..source_frame_image.pixel_height {
        for column_index in 0..source_frame_image.pixel_width {
            let source_byte_index =
                compute_rgba_byte_index(source_frame_image.pixel_width, column_index, row_index)?;
            let destination_byte_index = compute_rgba_byte_index(
                destination_pixel_width,
                destination_pixel_origin.0 + column_index,
                destination_pixel_origin.1 + row_index,
            )?;
            if should_replace_destination_pixels {
                destination_rgba_bytes[destination_byte_index..destination_byte_index + 4]
                    .copy_from_slice(
                        &source_frame_image.rgba_bytes[source_byte_index..source_byte_index + 4],
                    );
            } else {
                blend_animation_frame_pixel(
                    &mut destination_rgba_bytes[destination_byte_index..destination_byte_index + 4],
                    &source_frame_image.rgba_bytes[source_byte_index..source_byte_index + 4],
                );
            }
        }
    }
    Ok(())
}

fn blend_animation_frame_pixel(destination_rgba_bytes: &mut [u8], source_rgba_bytes: &[u8]) {
    let source_alpha = u32::from(source_rgba_bytes[3]);
    let destination_alpha = u32::from(destination_rgba_bytes[3]);
    let inverse_source_alpha = 255 - source_alpha;
    let output_alpha = source_alpha + (destination_alpha * inverse_source_alpha + 127) / 255;
    if output_alpha == 0 {
        destination_rgba_bytes.fill(0);
        return;
    }
    for channel_index in 0..3 {
        let source_color_contribution = u32::from(source_rgba_bytes[channel_index]) * source_alpha;
        let destination_color_contribution = u32::from(destination_rgba_bytes[channel_index])
            * destination_alpha
            * inverse_source_alpha
            / 255;
        destination_rgba_bytes[channel_index] = u8::try_from(
            (source_color_contribution + destination_color_contribution + output_alpha / 2)
                / output_alpha,
        )
        .unwrap_or(u8::MAX);
    }
    destination_rgba_bytes[3] = u8::try_from(output_alpha).unwrap_or(u8::MAX);
}

#[cfg(test)]
mod tests;
