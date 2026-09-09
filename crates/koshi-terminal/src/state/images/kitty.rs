//! Retained Kitty uploads, placement commands, deletion, and replies.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use koshi_image::{
    decode_base64, decode_png, raw_rgb, raw_rgba, AnimationFrame, DecodedAnimation, DecodedImage,
    FrameDelay, LoopPolicy,
};

use crate::graphics::{
    GraphicsProtocol, ImageAction, ImageDisplay, ImageRecord, KittyCommand, KittyCommandKind,
    KittyDelete,
};
use crate::state::{Screen, TerminalState};

use super::{
    add_image_storage, kitty_image_id, ImageContent, ImageContentId, ImagePlacementError,
    ImagePlacementId, KittyImage, MAX_IMAGE_PLACEMENTS, MAX_IMAGE_STORAGE_BYTES,
};

impl TerminalState {
    pub(crate) fn apply_kitty_command(
        &mut self,
        command: &KittyCommand,
    ) -> Result<(), ImagePlacementError> {
        match command.kind() {
            KittyCommandKind::Place => {
                if command.display().unicode_placeholder {
                    let result = self.apply_virtual_kitty_placement(command.display());
                    let display = self.kitty_reply_display(command.display());
                    self.reply_kitty(&display, result.as_ref().err().copied(), true);
                    return result;
                }
                let found = self.find_kitty_upload(command.display());
                let result = found
                    .ok_or(ImagePlacementError::ImageNotFound {
                        id: command.display().image_id,
                        number: command.display().image_number,
                    })
                    .and_then(|upload| {
                        let mut display = command.display().clone();
                        display.image_id = upload.display.image_id;
                        let record = ImageRecord {
                            protocol: GraphicsProtocol::Kitty,
                            image: Arc::clone(&upload.image),
                            animation: upload.animation.clone(),
                            action: ImageAction::Display,
                            display,
                            anchor: self.active_cursor_position(),
                        };
                        self.apply_image_record(&record)
                    });
                let display = self.kitty_reply_display(command.display());
                self.reply_kitty(&display, result.as_ref().err().copied(), true);
                result
            }
            KittyCommandKind::Delete(selector) => {
                self.delete_kitty_placements(command, selector);
                Ok(())
            }
            KittyCommandKind::AnimationFrame | KittyCommandKind::AnimationCompose => {
                let result = self.apply_kitty_animation_command(command);
                let display = self.kitty_reply_display(command.display());
                self.reply_kitty(&display, result.as_ref().err().copied(), true);
                result
            }
            KittyCommandKind::AnimationControl | KittyCommandKind::AnimationDelete => {
                self.apply_kitty_animation_command(command)
            }
        }
    }

    pub(super) fn apply_virtual_kitty_placement(
        &mut self,
        display: &ImageDisplay,
    ) -> Result<(), ImagePlacementError> {
        if display.relative_image_id.is_some()
            || display.relative_placement_id.is_some()
            || display.relative_offset_x != 0
            || display.relative_offset_y != 0
        {
            return Err(ImagePlacementError::VirtualRelative);
        }
        if display.cell_columns.is_none() || display.cell_rows.is_none() {
            return Err(ImagePlacementError::MissingCellDimensions {
                width: display.width,
                height: display.height,
            });
        }
        let upload = self
            .find_kitty_upload(display)
            .ok_or(ImagePlacementError::ImageNotFound {
                id: display.image_id,
                number: display.image_number,
            })?;
        let mut virtual_display = display.clone();
        virtual_display.image_id = upload.display.image_id;
        virtual_display.placement_id = virtual_display.placement_id.filter(|id| *id != 0);
        let record = Arc::new(ImageRecord {
            protocol: GraphicsProtocol::Kitty,
            image: Arc::clone(&upload.image),
            animation: upload.animation.clone(),
            action: ImageAction::Display,
            display: virtual_display,
            anchor: self.active_cursor_position(),
        });
        super::raster::prepare_with_plan(&record, self.cell_size, self.active_grid().dimensions())?;
        let screen = self.active;
        self.kitty_images.retain(|image| {
            !(image.virtual_placement
                && image.virtual_screen == Some(screen)
                && image.display.image_id == record.display.image_id
                && image.display.placement_id == record.display.placement_id)
        });
        if self.kitty_images.len() >= MAX_IMAGE_PLACEMENTS && !self.evict_unused_kitty_upload(None)
        {
            return Err(ImagePlacementError::TooManyPlacements {
                count: self.kitty_images.len() + 1,
                limit: MAX_IMAGE_PLACEMENTS,
            });
        }
        self.kitty_images.push(KittyImage {
            record,
            content: Arc::clone(&upload.content),
            virtual_placement: true,
            virtual_screen: Some(screen),
        });
        Ok(())
    }

    fn apply_kitty_animation_command(
        &mut self,
        command: &KittyCommand,
    ) -> Result<(), ImagePlacementError> {
        let animation_command = command
            .animation()
            .ok_or(ImagePlacementError::AnimationDataInvalid)?;
        let image = self.find_kitty_upload(command.display()).ok_or(
            ImagePlacementError::ImageNotFound {
                id: command.display().image_id,
                number: command.display().image_number,
            },
        )?;
        let content = Arc::clone(&image.content);
        if command.kind() == KittyCommandKind::AnimationDelete
            && content
                .animation
                .as_ref()
                .is_none_or(|animation| animation.frame_count() <= 1)
        {
            if command.free_data() {
                let image_id = image
                    .display
                    .image_id
                    .ok_or(ImagePlacementError::AnimationDataInvalid)?;
                self.remove_kitty_image(image_id);
                self.kitty_images
                    .retain(|image| image.display.image_id != Some(image_id));
            }
            return Ok(());
        }
        let next = match command.kind() {
            KittyCommandKind::AnimationFrame => {
                self.apply_kitty_animation_frame(&content, animation_command)?
            }
            KittyCommandKind::AnimationControl => {
                self.apply_kitty_animation_control(&content, animation_command)?
            }
            KittyCommandKind::AnimationCompose => {
                self.apply_kitty_animation_compose(&content, animation_command)?
            }
            KittyCommandKind::AnimationDelete => {
                self.apply_kitty_animation_delete(&content, animation_command)?
            }
            KittyCommandKind::Place | KittyCommandKind::Delete(_) => {
                return Err(ImagePlacementError::AnimationDataInvalid);
            }
        };
        self.install_kitty_content(content.id, next);
        Ok(())
    }

    fn apply_kitty_animation_frame(
        &self,
        content: &Arc<ImageContent>,
        command: &koshi_kitty::KittyAnimationCommand,
    ) -> Result<Arc<ImageContent>, ImagePlacementError> {
        let existing = content.animation.as_ref();
        let mut frames = existing.map_or_else(
            || {
                vec![
                    AnimationFrame::new(Arc::clone(&content.image), zero_frame_delay())
                        .expect("the retained image has valid pixels"),
                ]
            },
            |animation| animation.frames().to_vec(),
        );
        let requested_index = command
            .frame
            .map(|frame| {
                frame
                    .checked_sub(1)
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or(ImagePlacementError::AnimationFrameNotFound { frame })
            })
            .transpose()?
            .unwrap_or(frames.len());
        let target_index = requested_index.min(frames.len());
        let editing = target_index < frames.len();
        let base = if editing {
            frames[target_index].image_shared()
        } else if let Some(base_frame) = command.base_frame {
            let base_index = base_frame
                .checked_sub(1)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or(ImagePlacementError::AnimationFrameNotFound { frame: base_frame })?;
            frames
                .get(base_index)
                .map(AnimationFrame::image_shared)
                .ok_or(ImagePlacementError::AnimationFrameNotFound { frame: base_frame })?
        } else {
            Arc::new(DecodedImage {
                width: content.image.width,
                height: content.image.height,
                rgba: vec![
                    0;
                    usize::try_from(
                        content
                            .image
                            .width
                            .checked_mul(content.image.height)
                            .ok_or(ImagePlacementError::AnimationDataInvalid)?
                    )
                    .ok()
                    .and_then(|pixels| pixels.checked_mul(4))
                    .ok_or(ImagePlacementError::AnimationDataInvalid)?
                ],
            })
        };
        let patch = decode_animation_payload(command, base.width, base.height)?;
        let (width, height) = (base.width, base.height);
        let x = command.source_x;
        let y = command.source_y;
        let right = x
            .checked_add(patch.width)
            .ok_or(ImagePlacementError::AnimationDataInvalid)?;
        let bottom = y
            .checked_add(patch.height)
            .ok_or(ImagePlacementError::AnimationDataInvalid)?;
        if right > width || bottom > height {
            return Err(ImagePlacementError::AnimationDataInvalid);
        }
        let mut rgba = if let Some(background) = command.background {
            background
                .into_iter()
                .cycle()
                .take(
                    usize::try_from(
                        width
                            .checked_mul(height)
                            .ok_or(ImagePlacementError::AnimationDataInvalid)?,
                    )
                    .ok()
                    .and_then(|pixels| pixels.checked_mul(4))
                    .ok_or(ImagePlacementError::AnimationDataInvalid)?,
                )
                .collect::<Vec<_>>()
        } else {
            base.rgba.clone()
        };
        compose_pixels(&mut rgba, width, &patch, (x, y), command.replace)?;
        let image = Arc::new(DecodedImage {
            width,
            height,
            rgba,
        });
        let frame = match command.gap_ms {
            Some(gap) if gap < 0 => AnimationFrame::new_gapless(image),
            Some(gap) => AnimationFrame::new(image, frame_delay_from_gap(gap)?),
            None => AnimationFrame::new(
                image,
                frames
                    .get(target_index)
                    .map_or_else(default_frame_delay, AnimationFrame::delay),
            ),
        }
        .map_err(|_| ImagePlacementError::AnimationDataInvalid)?;
        if target_index == frames.len() {
            frames.push(frame);
        } else {
            frames[target_index] = frame;
        }
        let loop_policy =
            existing.map_or(LoopPolicy::Infinite, |animation| animation.loop_policy());
        let animation = Arc::new(
            DecodedAnimation::new(frames, loop_policy)
                .map_err(|_| ImagePlacementError::AnimationDataInvalid)?,
        );
        let frame_index =
            (content.animation_frame as usize).min(animation.frame_count().saturating_sub(1));
        let mut next = (**content).clone();
        next.animation = Some(animation.clone());
        next.animation_frame = u32::try_from(frame_index).unwrap_or(0);
        next.image = animation.frames()[frame_index].image_shared();
        next.animation_running = content.animation_running || content.animation_loading;
        next.animation_loading = content.animation_loading;
        Ok(Arc::new(next))
    }

    fn apply_kitty_animation_control(
        &self,
        content: &Arc<ImageContent>,
        command: &koshi_kitty::KittyAnimationCommand,
    ) -> Result<Arc<ImageContent>, ImagePlacementError> {
        let current = content
            .animation
            .as_ref()
            .ok_or(ImagePlacementError::AnimationDataInvalid)?;
        let mut frames = current.frames().to_vec();
        if let Some(gap) = command.gap_ms {
            let index = command
                .affected_frame
                .map_or(content.animation_frame as usize, |frame| {
                    frame.saturating_sub(1) as usize
                });
            let frame =
                frames
                    .get_mut(index)
                    .ok_or(ImagePlacementError::AnimationFrameNotFound {
                        frame: u32::try_from(index + 1).unwrap_or(u32::MAX),
                    })?;
            *frame = if gap < 0 {
                AnimationFrame::new_gapless(frame.image_shared())
            } else {
                AnimationFrame::new(frame.image_shared(), frame_delay_from_gap(gap)?)
            }
            .map_err(|_| ImagePlacementError::AnimationDataInvalid)?;
        }
        let loop_policy = match command.loops {
            None | Some(0) => current.loop_policy(),
            Some(1) => LoopPolicy::Infinite,
            Some(value) => LoopPolicy::finite(value - 1)
                .map_err(|_| ImagePlacementError::AnimationDataInvalid)?,
        };
        let animation = Arc::new(
            DecodedAnimation::new(frames, loop_policy)
                .map_err(|_| ImagePlacementError::AnimationDataInvalid)?,
        );
        let frame_index = command
            .frame
            .map(|frame| {
                frame
                    .checked_sub(1)
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or(ImagePlacementError::AnimationFrameNotFound { frame })
            })
            .transpose()?
            .unwrap_or(content.animation_frame as usize);
        if frame_index >= animation.frame_count() {
            return Err(ImagePlacementError::AnimationFrameNotFound {
                frame: u32::try_from(frame_index + 1).unwrap_or(u32::MAX),
            });
        }
        let mut next = (**content).clone();
        next.animation = Some(animation.clone());
        next.animation_frame = u32::try_from(frame_index).unwrap_or(0);
        next.image = animation.frames()[frame_index].image_shared();
        match command.state {
            Some(1) => {
                next.animation_running = false;
                next.animation_loading = false;
                next.animation_loops = 0;
                next.animation_elapsed_nanos = 0;
            }
            Some(2) => {
                next.animation_running = true;
                next.animation_loading = true;
            }
            Some(3) => {
                next.animation_running = true;
                next.animation_loading = false;
            }
            None => {}
            Some(_) => return Err(ImagePlacementError::AnimationDataInvalid),
        }
        Ok(Arc::new(next))
    }

    fn apply_kitty_animation_compose(
        &self,
        content: &Arc<ImageContent>,
        command: &koshi_kitty::KittyAnimationCommand,
    ) -> Result<Arc<ImageContent>, ImagePlacementError> {
        let current = content
            .animation
            .as_ref()
            .ok_or(ImagePlacementError::AnimationDataInvalid)?;
        let source_index = command.source_frame.unwrap_or(1).saturating_sub(1) as usize;
        let destination_index = command.destination_frame.unwrap_or(1).saturating_sub(1) as usize;
        let source = current
            .frames()
            .get(source_index)
            .ok_or(ImagePlacementError::AnimationFrameNotFound {
                frame: u32::try_from(source_index + 1).unwrap_or(u32::MAX),
            })?
            .image();
        let destination = current
            .frames()
            .get(destination_index)
            .ok_or(ImagePlacementError::AnimationFrameNotFound {
                frame: u32::try_from(destination_index + 1).unwrap_or(u32::MAX),
            })?
            .image();
        let width = command
            .width
            .unwrap_or(source.width.saturating_sub(command.source_x));
        let height = command
            .height
            .unwrap_or(source.height.saturating_sub(command.source_y));
        let source_right = command
            .source_x
            .checked_add(width)
            .ok_or(ImagePlacementError::AnimationDataInvalid)?;
        let source_bottom = command
            .source_y
            .checked_add(height)
            .ok_or(ImagePlacementError::AnimationDataInvalid)?;
        let destination_right = command
            .destination_x
            .checked_add(width)
            .ok_or(ImagePlacementError::AnimationDataInvalid)?;
        let destination_bottom = command
            .destination_y
            .checked_add(height)
            .ok_or(ImagePlacementError::AnimationDataInvalid)?;
        if width == 0
            || height == 0
            || source_right > source.width
            || source_bottom > source.height
            || destination_right > destination.width
            || destination_bottom > destination.height
        {
            return Err(ImagePlacementError::AnimationDataInvalid);
        }
        if source_index == destination_index
            && command.source_x < destination_right
            && command.destination_x < source_right
            && command.source_y < destination_bottom
            && command.destination_y < source_bottom
        {
            return Err(ImagePlacementError::AnimationDataInvalid);
        }
        let mut rgba = destination.rgba.clone();
        for row in 0..height {
            for column in 0..width {
                let source_at = pixel_index(
                    source.width,
                    command.source_x + column,
                    command.source_y + row,
                )?;
                let destination_at = pixel_index(
                    destination.width,
                    command.destination_x + column,
                    command.destination_y + row,
                )?;
                let source_pixel = &source.rgba[source_at..source_at + 4];
                if command.replace {
                    rgba[destination_at..destination_at + 4].copy_from_slice(source_pixel);
                } else {
                    blend_pixel(&mut rgba[destination_at..destination_at + 4], source_pixel);
                }
            }
        }
        let image = Arc::new(DecodedImage {
            width: destination.width,
            height: destination.height,
            rgba,
        });
        let delay = current.frames()[destination_index].delay();
        let mut frames = current.frames().to_vec();
        frames[destination_index] = AnimationFrame::new(image, delay)
            .map_err(|_| ImagePlacementError::AnimationDataInvalid)?;
        let animation = Arc::new(
            DecodedAnimation::new(frames, current.loop_policy())
                .map_err(|_| ImagePlacementError::AnimationDataInvalid)?,
        );
        let mut next = (**content).clone();
        next.animation = Some(animation.clone());
        let frame_index = (content.animation_frame as usize).min(animation.frame_count() - 1);
        next.image = animation.frames()[frame_index].image_shared();
        Ok(Arc::new(next))
    }

    fn apply_kitty_animation_delete(
        &self,
        content: &Arc<ImageContent>,
        command: &koshi_kitty::KittyAnimationCommand,
    ) -> Result<Arc<ImageContent>, ImagePlacementError> {
        let current = content
            .animation
            .as_ref()
            .ok_or(ImagePlacementError::AnimationDataInvalid)?;
        let frame = command
            .frame
            .unwrap_or(1)
            .min(u32::try_from(current.frame_count()).unwrap_or(u32::MAX))
            .saturating_sub(1) as usize;
        let mut frames = current.frames().to_vec();
        frames.remove(frame);
        let mut next = (**content).clone();
        let selected = content.animation_frame as usize;
        let selected = if selected > frame {
            selected - 1
        } else if selected == frame {
            frame.min(frames.len().saturating_sub(1))
        } else {
            selected
        };
        if frames.len() == 1 {
            next.image = frames[0].image_shared();
            next.animation = None;
            next.animation_frame = 0;
            next.animation_loops = 0;
            next.animation_elapsed_nanos = 0;
            next.animation_running = false;
            next.animation_loading = false;
            return Ok(Arc::new(next));
        }
        let animation = Arc::new(
            DecodedAnimation::new(frames, current.loop_policy())
                .map_err(|_| ImagePlacementError::AnimationDataInvalid)?,
        );
        let frame_index = selected.min(animation.frame_count().saturating_sub(1));
        next.animation = Some(animation.clone());
        next.animation_frame = u32::try_from(frame_index).unwrap_or(0);
        next.image = animation.frames()[frame_index].image_shared();
        Ok(Arc::new(next))
    }

    fn install_kitty_content(&mut self, content_id: ImageContentId, content: Arc<ImageContent>) {
        for placement in &mut self.primary_image_placements {
            if placement.content.id == content_id {
                super::apply_animation_content(placement, &content);
            }
        }
        for placement in &mut self.primary_image_history {
            if placement.content.id == content_id {
                super::apply_animation_history_content(placement, &content);
            }
        }
        for placement in &mut self.alternate_image_placements {
            if placement.content.id == content_id {
                super::apply_animation_content(placement, &content);
            }
        }
        for image in &mut self.kitty_images {
            if image.content.id != content_id {
                continue;
            }
            image.content = Arc::clone(&content);
            let mut record = image.record.as_ref().clone();
            record.image = Arc::clone(&content.image);
            record.animation = content.animation.clone();
            image.record = Arc::new(record);
        }
    }

    fn find_kitty_upload(&self, display: &ImageDisplay) -> Option<KittyImage> {
        self.kitty_images
            .iter()
            .rev()
            .find(|record| {
                !record.virtual_placement && {
                    if let Some(id) = display.image_id.filter(|id| *id != 0) {
                        record.display.image_id == Some(id)
                    } else if let Some(number) = display.image_number.filter(|number| *number != 0)
                    {
                        record.display.image_number == Some(number)
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
                    .map(|placement| KittyImage {
                        record: Arc::clone(&placement.record),
                        content: Arc::clone(&placement.content),
                        virtual_placement: false,
                        virtual_screen: None,
                    })
                    .chain(
                        self.primary_image_history
                            .iter()
                            .map(|placement| KittyImage {
                                record: Arc::clone(&placement.record),
                                content: Arc::clone(&placement.content),
                                virtual_placement: false,
                                virtual_screen: None,
                            }),
                    )
                    .find(|record| {
                        record.protocol == GraphicsProtocol::Kitty
                            && display
                                .image_id
                                .is_some_and(|id| id != 0 && record.display.image_id == Some(id))
                    })
            })
    }

    pub(super) fn retain_kitty_upload(
        &mut self,
        record: &mut ImageRecord,
    ) -> Result<(), ImagePlacementError> {
        if record.protocol != GraphicsProtocol::Kitty
            || !matches!(
                record.action,
                ImageAction::Transmit | ImageAction::TransmitAndDisplay
            )
        {
            return Ok(());
        }
        if record
            .display
            .image_number
            .is_some_and(|number| number != 0)
            && record.display.image_id.is_none()
        {
            let ids: HashSet<u32> = self
                .kitty_images
                .iter()
                .filter_map(|image| image.display.image_id)
                .collect();
            let id = (1..=u32::MAX)
                .find(|id| !ids.contains(id))
                .ok_or(ImagePlacementError::IdentityExhausted)?;
            record.display.image_id = Some(id);
        }
        let Some(id) = kitty_image_id(record) else {
            return Ok(());
        };
        self.remove_kitty_image(id);
        self.kitty_images
            .retain(|image| image.display.image_id != Some(id));
        if self.kitty_images.len() >= MAX_IMAGE_PLACEMENTS && !self.evict_unused_kitty_upload(None)
        {
            return Err(ImagePlacementError::TooManyPlacements {
                count: self.kitty_images.len() + 1,
                limit: MAX_IMAGE_PLACEMENTS,
            });
        }
        let record = Arc::new(record.clone());
        let content = self.allocate_image_content(Arc::clone(&record))?;
        self.kitty_images.push(KittyImage {
            content,
            record,
            virtual_placement: false,
            virtual_screen: None,
        });
        Ok(())
    }

    pub(super) fn check_image_storage(
        &mut self,
        requested_bytes: usize,
        protected_id: Option<u32>,
    ) -> Result<(), ImagePlacementError> {
        loop {
            let used_bytes = self.image_storage_bytes();
            if used_bytes <= MAX_IMAGE_STORAGE_BYTES {
                return Ok(());
            }
            if !self.evict_unused_kitty_upload(protected_id) {
                return Err(ImagePlacementError::StorageLimit {
                    used_bytes: used_bytes.saturating_sub(requested_bytes),
                    requested_bytes,
                    limit_bytes: MAX_IMAGE_STORAGE_BYTES,
                });
            }
        }
    }

    fn referenced_kitty_ids(&self) -> HashSet<u32> {
        self.primary_image_placements
            .iter()
            .chain(&self.alternate_image_placements)
            .map(|p| &p.record)
            .chain(self.primary_image_history.iter().map(|p| &p.record))
            .chain(
                self.kitty_images
                    .iter()
                    .filter(|image| image.virtual_placement)
                    .map(|image| &image.record),
            )
            .filter_map(|record| kitty_image_id(record))
            .collect()
    }

    fn evict_unused_kitty_upload(&mut self, protected_id: Option<u32>) -> bool {
        let referenced = self.referenced_kitty_ids();
        let candidate = self.kitty_images.iter().position(|record| {
            record
                .display
                .image_id
                .is_some_and(|id| Some(id) != protected_id && !referenced.contains(&id))
        });
        if let Some(index) = candidate {
            self.kitty_images.remove(index);
            true
        } else {
            false
        }
    }

    pub(super) fn image_storage_bytes(&self) -> usize {
        let mut seen_contents = HashSet::<ImageContentId>::new();
        let mut seen_images = HashSet::new();
        let mut bytes = self.native_fragment_storage_bytes();
        let mut add_content = |content: &Arc<super::ImageContent>| {
            if seen_contents.insert(content.id) {
                add_image_storage(&mut bytes, &mut seen_images, content.image.as_ref());
                if let Some(animation) = &content.animation {
                    for frame in animation.frames() {
                        add_image_storage(&mut bytes, &mut seen_images, frame.image());
                    }
                }
                if let Some(source) = &content.sixel {
                    bytes = bytes.saturating_add(source.storage_bytes().unwrap_or(usize::MAX));
                }
            }
        };
        for image in &self.kitty_images {
            add_content(&image.content);
        }
        for placement in self
            .primary_image_placements
            .iter()
            .chain(&self.alternate_image_placements)
            .chain(self.native_images.iter().map(|source| &source.placement))
        {
            add_content(&placement.content);
        }
        for placement in &self.primary_image_history {
            add_content(&placement.content);
        }

        for raster in self
            .primary_image_placements
            .iter()
            .chain(&self.alternate_image_placements)
            .chain(self.native_images.iter().map(|source| &source.placement))
            .filter_map(|placement| placement.raster.as_ref())
            .chain(
                self.primary_image_history
                    .iter()
                    .filter_map(|placement| placement.raster.as_ref()),
            )
        {
            add_image_storage(&mut bytes, &mut seen_images, raster.as_ref());
        }
        bytes
    }

    pub(crate) fn reply_kitty(
        &mut self,
        display: &ImageDisplay,
        error: Option<ImagePlacementError>,
        kitty: bool,
    ) {
        if !kitty {
            return;
        }
        let message = match error {
            None => "OK",
            Some(ImagePlacementError::ImageNotFound { .. }) => "ENOENT:image not found",
            Some(ImagePlacementError::NoParent) => "ENOPARENT:relative image parent not found",
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
        self.write_kitty_reply(display, message, error.is_some());
    }

    pub(crate) fn reply_kitty_failure(
        &mut self,
        display: &ImageDisplay,
        error: &crate::graphics::GraphicsError,
    ) {
        use crate::graphics::GraphicsError;
        let message = match error {
            GraphicsError::UnsupportedAction { .. } | GraphicsError::UnsupportedMedia { .. } => {
                "ENOTSUP:unsupported image command"
            }
            GraphicsError::TransferTooLarge { .. } | GraphicsError::ImageTooLarge { .. } => {
                "ENOSPC:image storage limit"
            }
            _ => "EINVAL:invalid image data",
        };
        self.write_kitty_reply(display, message, true);
    }

    fn write_kitty_reply(&mut self, display: &ImageDisplay, message: &str, error: bool) {
        if display.quiet == 2 || (display.quiet == 1 && !error) {
            return;
        }
        if display.image_id.unwrap_or(0) == 0 && display.image_number.unwrap_or(0) == 0 {
            return;
        }
        let mut reply = format!("\x1b_Gi={}", display.image_id.unwrap_or(0));
        if let Some(number) = display.image_number {
            reply.push_str(&format!(",I={number}"));
        }
        if let Some(id) = display.placement_id.filter(|id| *id != 0) {
            reply.push_str(&format!(",p={id}"));
        }
        reply.push(';');
        reply.push_str(message);
        reply.push_str("\x1b\\");
        self.replies.extend_from_slice(reply.as_bytes());
    }

    fn delete_kitty_placements(&mut self, command: &KittyCommand, selector: KittyDelete) {
        let resolved = self
            .find_kitty_upload(command.display())
            .and_then(|image| image.display.image_id);
        let cursor = self.active_cursor_position();
        let live_top = self.scrollback.total_pushed();
        let (grid_rows, grid_columns) = self.active_grid().dimensions();
        let mut removed_image_ids = HashSet::new();
        let mut removed_identities = HashSet::new();
        let mut data_candidates = HashSet::new();
        let mut relative_anchors = HashMap::new();
        match self.active {
            Screen::Primary => {
                for placement in &self.primary_image_placements {
                    if placement.record.display.relative_image_id.is_some()
                        || placement.record.display.relative_placement_id.is_some()
                    {
                        if let Ok(Some((row, column))) =
                            self.resolve_relative_anchor(&placement.record, Some(placement.id))
                        {
                            relative_anchors.insert(placement.id, (i128::from(row), column));
                        }
                    }
                }
                for placement in &self.primary_image_history {
                    if placement.record.display.relative_image_id.is_some()
                        || placement.record.display.relative_placement_id.is_some()
                    {
                        if let Ok(Some((row, column))) =
                            self.resolve_relative_anchor(&placement.record, Some(placement.id))
                        {
                            relative_anchors.insert(placement.id, (i128::from(row), column));
                        }
                    }
                }
            }
            Screen::Alternate => {
                for placement in &self.alternate_image_placements {
                    if placement.record.display.relative_image_id.is_some()
                        || placement.record.display.relative_placement_id.is_some()
                    {
                        if let Ok(Some((row, column))) =
                            self.resolve_relative_anchor(&placement.record, Some(placement.id))
                        {
                            relative_anchors.insert(placement.id, (i128::from(row), column));
                        }
                    }
                }
            }
        }
        let broad_image_delete = matches!(selector, KittyDelete::Id | KittyDelete::Number)
            && command
                .display()
                .placement_id
                .is_none_or(|placement_id| placement_id == 0);
        let mut keep = |record: &Arc<ImageRecord>,
                        placement_id: ImagePlacementId,
                        stored_row: i128,
                        stored_column: u16,
                        rows: u16,
                        columns: u16| {
            if record.protocol != GraphicsProtocol::Kitty {
                return true;
            }
            let display = command.display();
            let relative = record.display.relative_image_id.is_some()
                || record.display.relative_placement_id.is_some();
            let geometry = if relative {
                relative_anchors.get(&placement_id).copied()
            } else {
                Some((stored_row, i64::from(stored_column)))
            };
            let Some((row, col)) = geometry else {
                return true;
            };
            let contains_row = |value: i128| row <= value && value < row + i128::from(rows);
            let contains_column = |value: u32| {
                let value = i64::from(value);
                col <= value && value < col + i64::from(columns)
            };
            let x = display.source_offset_x.unwrap_or(1).saturating_sub(1);
            let y = i128::from(display.source_offset_y.unwrap_or(1)) - 1;
            let by_id = record.display.image_id == resolved && resolved.is_some();
            let by_placement = display
                .placement_id
                .is_none_or(|id| id == 0 || record.display.placement_id == Some(id));
            let matches = match selector {
                KittyDelete::Visible => {
                    row < i128::from(grid_rows)
                        && row + i128::from(rows) > 0
                        && col < i64::from(grid_columns)
                        && col + i64::from(columns) > 0
                }
                KittyDelete::Id | KittyDelete::Number => by_id && by_placement,
                KittyDelete::Cursor => {
                    contains_row(i128::from(cursor.0)) && contains_column(u32::from(cursor.1))
                }
                KittyDelete::Cell => contains_row(y) && contains_column(x),
                KittyDelete::CellAtZ => {
                    contains_row(y)
                        && contains_column(x)
                        && record.display.z_index == display.z_index
                }
                KittyDelete::IdRange => record.display.image_id.is_some_and(|id| {
                    id >= display.source_offset_x.unwrap_or(0)
                        && id <= display.source_offset_y.unwrap_or(0)
                }),
                KittyDelete::Column => contains_column(x),
                KittyDelete::Row => contains_row(y),
                KittyDelete::Z => record.display.z_index == display.z_index,
            };
            if matches {
                if let Some(id) = record.display.image_id {
                    data_candidates.insert(id);
                    if broad_image_delete || record.display.placement_id.is_none() {
                        removed_image_ids.insert(id);
                    } else if let Some(placement_id) =
                        record.display.placement_id.filter(|id| *id != 0)
                    {
                        removed_identities.insert((id, placement_id));
                    }
                }
            }
            !matches
        };
        match self.active {
            Screen::Primary => {
                self.primary_image_placements.retain(|p| {
                    keep(
                        &p.record,
                        p.id,
                        i128::from(p.anchor.0),
                        p.anchor.1,
                        p.rows,
                        p.columns,
                    )
                });
                self.primary_image_history.retain(|p| {
                    keep(
                        &p.record,
                        p.id,
                        i128::from(p.anchor.0) - i128::from(live_top),
                        p.anchor.1,
                        p.rows,
                        p.columns,
                    )
                });
            }
            Screen::Alternate => self.alternate_image_placements.retain(|p| {
                keep(
                    &p.record,
                    p.id,
                    i128::from(p.anchor.0),
                    p.anchor.1,
                    p.rows,
                    p.columns,
                )
            }),
        }
        self.remove_relative_dependents(&mut removed_image_ids, &mut removed_identities);
        self.kitty_images.retain(|image| {
            if !image.virtual_placement || image.virtual_screen != Some(self.active) {
                return true;
            }
            let display = command.display();
            let image_id = image.display.image_id;
            let placement_matches = display
                .placement_id
                .is_none_or(|id| id == 0 || image.display.placement_id == Some(id));
            let matches = match selector {
                KittyDelete::Id | KittyDelete::Number => {
                    resolved.is_some_and(|id| image_id == Some(id)) && placement_matches
                }
                KittyDelete::IdRange => image_id.is_some_and(|id| {
                    id >= display.source_offset_x.unwrap_or(0)
                        && id <= display.source_offset_y.unwrap_or(0)
                }),
                _ => false,
            };
            if matches {
                if let Some(id) = image_id {
                    data_candidates.insert(id);
                }
                if let Some(identity) = super::kitty_placement_identity(&image.record) {
                    removed_identities.insert(identity);
                } else if let Some(id) = image_id {
                    removed_image_ids.insert(id);
                }
            }
            !matches
        });
        self.remove_relative_dependents(&mut removed_image_ids, &mut removed_identities);
        if command.free_data() {
            removed_image_ids.extend(data_candidates);
            if let Some(id) =
                resolved.filter(|_| matches!(selector, KittyDelete::Id | KittyDelete::Number))
            {
                removed_image_ids.insert(id);
            }
            if selector == KittyDelete::IdRange {
                removed_image_ids.extend(
                    self.kitty_images
                        .iter()
                        .filter_map(|record| record.display.image_id)
                        .filter(|id| {
                            *id >= command.display().source_offset_x.unwrap_or(0)
                                && *id <= command.display().source_offset_y.unwrap_or(0)
                        }),
                );
            }
            let referenced = self.referenced_kitty_ids();
            self.kitty_images.retain(|record| {
                record
                    .display
                    .image_id
                    .is_none_or(|id| !removed_image_ids.contains(&id) || referenced.contains(&id))
            });
        }
    }

    fn remove_relative_dependents(
        &mut self,
        removed_image_ids: &mut HashSet<u32>,
        removed_identities: &mut HashSet<(u32, u32)>,
    ) {
        loop {
            let mut changed = false;
            let mut retain = |record: &Arc<ImageRecord>| {
                let Some(parent_id) = record.display.relative_image_id else {
                    return true;
                };
                let parent_placement_id = record
                    .display
                    .relative_placement_id
                    .filter(|placement_id| *placement_id != 0);
                let parent_removed = removed_image_ids.contains(&parent_id)
                    || parent_placement_id.is_some_and(|placement_id| {
                        removed_identities.contains(&(parent_id, placement_id))
                    });
                if !parent_removed {
                    return true;
                }
                changed = true;
                if let Some(identity) = super::kitty_placement_identity(record) {
                    removed_identities.insert(identity);
                } else if let Some(id) = record.display.image_id {
                    removed_image_ids.insert(id);
                }
                false
            };
            self.primary_image_placements
                .retain(|placement| retain(&placement.record));
            self.primary_image_history
                .retain(|placement| retain(&placement.record));
            self.alternate_image_placements
                .retain(|placement| retain(&placement.record));
            if !changed {
                break;
            }
        }
    }

    pub(crate) fn kitty_reply_display(&self, display: &ImageDisplay) -> ImageDisplay {
        let mut reply = display.clone();
        if reply.image_id.is_none() {
            reply.image_id = self
                .find_kitty_upload(display)
                .and_then(|image| image.display.image_id);
        }
        reply
    }
}

fn zero_frame_delay() -> FrameDelay {
    FrameDelay::new(0, 1).expect("a one-millisecond denominator is valid")
}

fn default_frame_delay() -> FrameDelay {
    FrameDelay::new(40, 1).expect("a one-millisecond denominator is valid")
}

fn frame_delay_from_gap(gap_ms: i32) -> Result<FrameDelay, ImagePlacementError> {
    FrameDelay::new(gap_ms.max(0) as u32, 1).map_err(|_| ImagePlacementError::AnimationDataInvalid)
}

fn decode_animation_payload(
    command: &koshi_kitty::KittyAnimationCommand,
    fallback_width: u32,
    fallback_height: u32,
) -> Result<DecodedImage, ImagePlacementError> {
    let bytes = decode_base64(GraphicsProtocol::Kitty, &command.payload)
        .map_err(|_| ImagePlacementError::AnimationDataInvalid)?;
    match command.format.unwrap_or(32) {
        24 => raw_rgb(
            GraphicsProtocol::Kitty,
            command.width.unwrap_or(fallback_width),
            command.height.unwrap_or(fallback_height),
            &bytes,
        )
        .map_err(|_| ImagePlacementError::AnimationDataInvalid),
        32 => raw_rgba(
            GraphicsProtocol::Kitty,
            command.width.unwrap_or(fallback_width),
            command.height.unwrap_or(fallback_height),
            &bytes,
        )
        .map_err(|_| ImagePlacementError::AnimationDataInvalid),
        100 => decode_png(GraphicsProtocol::Kitty, &bytes)
            .map_err(|_| ImagePlacementError::AnimationDataInvalid),
        _ => Err(ImagePlacementError::AnimationDataInvalid),
    }
}

fn pixel_index(width: u32, x: u32, y: u32) -> Result<usize, ImagePlacementError> {
    let index = y
        .checked_mul(width)
        .and_then(|value| value.checked_add(x))
        .and_then(|value| value.checked_mul(4))
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(ImagePlacementError::AnimationDataInvalid)?;
    Ok(index)
}

fn compose_pixels(
    destination: &mut [u8],
    destination_width: u32,
    source: &DecodedImage,
    destination_origin: (u32, u32),
    replace: bool,
) -> Result<(), ImagePlacementError> {
    for row in 0..source.height {
        for column in 0..source.width {
            let source_at = pixel_index(source.width, column, row)?;
            let destination_at = pixel_index(
                destination_width,
                destination_origin.0 + column,
                destination_origin.1 + row,
            )?;
            if replace {
                destination[destination_at..destination_at + 4]
                    .copy_from_slice(&source.rgba[source_at..source_at + 4]);
            } else {
                blend_pixel(
                    &mut destination[destination_at..destination_at + 4],
                    &source.rgba[source_at..source_at + 4],
                );
            }
        }
    }
    Ok(())
}

fn blend_pixel(destination: &mut [u8], source: &[u8]) {
    let source_alpha = u32::from(source[3]);
    let destination_alpha = u32::from(destination[3]);
    let inverse_source_alpha = 255 - source_alpha;
    let output_alpha = source_alpha + (destination_alpha * inverse_source_alpha + 127) / 255;
    if output_alpha == 0 {
        destination.fill(0);
        return;
    }
    for channel in 0..3 {
        let source_part = u32::from(source[channel]) * source_alpha;
        let destination_part =
            u32::from(destination[channel]) * destination_alpha * inverse_source_alpha / 255;
        destination[channel] =
            u8::try_from((source_part + destination_part + output_alpha / 2) / output_alpha)
                .unwrap_or(u8::MAX);
    }
    destination[3] = u8::try_from(output_alpha).unwrap_or(u8::MAX);
}

#[cfg(test)]
mod tests;
