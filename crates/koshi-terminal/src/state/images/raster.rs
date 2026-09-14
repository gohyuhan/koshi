//! Pixel-sized images fitted and padded to their shared terminal-cell rectangle.

use super::*;
use crate::graphics::{DecodedImage, MAX_IMAGE_SIDE_PIXEL_COUNT};
use koshi_core::geometry::PixelCellSize;

pub(super) struct PreparedRaster {
    pub(super) column_count: u32,
    pub(super) row_count: u32,
    pub(super) plan: RasterPlan,
    pub(super) raster: Option<Arc<DecodedImage>>,
}

#[cfg(test)]
pub(super) fn prepare_image_raster(
    image_record: &ImageRecord,
    pixel_cell_size: Option<PixelCellSize>,
    grid_dimensions: (u16, u16),
) -> Result<(u32, u32, Option<Arc<DecodedImage>>), ImagePlacementError> {
    let prepared = prepare_image_with_raster_plan(image_record, pixel_cell_size, grid_dimensions)?;
    Ok((prepared.column_count, prepared.row_count, prepared.raster))
}

pub(super) fn prepare_image_with_raster_plan(
    image_record: &ImageRecord,
    pixel_cell_size: Option<PixelCellSize>,
    grid_dimensions: (u16, u16),
) -> Result<PreparedRaster, ImagePlacementError> {
    let image_display = &image_record.display;
    let is_kitty_protocol = image_record.protocol == GraphicsProtocol::Kitty;
    let requested_column_count = image_display
        .requested_column_count
        .or_else(|| find_requested_cell_count(image_display.requested_width));
    let requested_row_count = image_display
        .requested_row_count
        .or_else(|| find_requested_cell_count(image_display.requested_height));
    if is_kitty_protocol
        && pixel_cell_size.is_some_and(|pixel_cell_size| {
            image_display.cell_pixel_offset_x.unwrap_or(0)
                >= u32::from(pixel_cell_size.get_pixel_width())
                || image_display.cell_pixel_offset_y.unwrap_or(0)
                    >= u32::from(pixel_cell_size.get_pixel_height())
        })
    {
        return Err(ImagePlacementError::UnsupportedCellDimensions {
            requested_width: image_display.requested_width,
            requested_height: image_display.requested_height,
        });
    }
    if is_kitty_protocol
        && !image_display.is_unicode_placeholder
        && requested_column_count.is_some()
        && requested_row_count.is_some()
        && (pixel_cell_size.is_none()
            || (image_display.cell_pixel_offset_x.unwrap_or(0) == 0
                && image_display.cell_pixel_offset_y.unwrap_or(0) == 0))
    {
        let (column_count, row_count) = compute_image_cell_dimensions(image_record)?;
        validate_source_pixels(image_record)?;
        let raster_plan = build_identity_raster_plan(image_record, column_count, row_count)?;
        return Ok(PreparedRaster {
            column_count,
            row_count,
            plan: raster_plan,
            raster: None,
        });
    }
    let Some(pixel_cell_size) = pixel_cell_size else {
        if is_kitty_protocol && (requested_column_count.is_none() || requested_row_count.is_none())
        {
            return Err(ImagePlacementError::MissingCellDimensions {
                requested_width: image_display.requested_width,
                requested_height: image_display.requested_height,
            });
        }
        let (column_count, row_count) = compute_image_cell_dimensions(image_record)?;
        validate_source_pixels(image_record)?;
        let raster_plan = build_identity_raster_plan(image_record, column_count, row_count)?;
        return Ok(PreparedRaster {
            column_count,
            row_count,
            plan: raster_plan,
            raster: None,
        });
    };
    validate_source_pixels(image_record)?;
    let cell_pixel_width = u64::from(pixel_cell_size.get_pixel_width());
    let cell_pixel_height = u64::from(pixel_cell_size.get_pixel_height());
    let (source_x_pixel, source_y_pixel, source_pixel_width, source_pixel_height) =
        image_record.compute_source_rect()?;
    let output_pixel_offset_x = if is_kitty_protocol {
        u64::from(image_display.cell_pixel_offset_x.unwrap_or(0))
    } else {
        0
    };
    let output_pixel_offset_y = if is_kitty_protocol {
        u64::from(image_display.cell_pixel_offset_y.unwrap_or(0))
    } else {
        0
    };
    if output_pixel_offset_x >= cell_pixel_width || output_pixel_offset_y >= cell_pixel_height {
        return Err(ImagePlacementError::UnsupportedCellDimensions {
            requested_width: image_display.requested_width,
            requested_height: image_display.requested_height,
        });
    }
    let is_iterm_protocol = image_record.protocol == GraphicsProtocol::Iterm2;
    let requested_width_pixels = if is_kitty_protocol {
        requested_column_count.map(|column_count| u64::from(column_count) * cell_pixel_width)
    } else {
        compute_requested_image_pixel_size(
            image_display.requested_width,
            cell_pixel_width,
            u64::from(grid_dimensions.1) * cell_pixel_width,
            is_iterm_protocol,
        )
    };
    let requested_height_pixels = if is_kitty_protocol {
        requested_row_count.map(|row_count| u64::from(row_count) * cell_pixel_height)
    } else {
        compute_requested_image_pixel_size(
            image_display.requested_height,
            cell_pixel_height,
            u64::from(grid_dimensions.0) * cell_pixel_height,
            is_iterm_protocol,
        )
    };
    let (target_width_pixels, target_height_pixels) =
        match (requested_width_pixels, requested_height_pixels) {
            (None, None) => (
                u64::from(source_pixel_width),
                u64::from(source_pixel_height),
            ),
            (Some(width_pixels), None) => {
                let width_pixels = width_pixels.saturating_sub(output_pixel_offset_x);
                (
                    width_pixels,
                    (width_pixels * u64::from(source_pixel_height) / u64::from(source_pixel_width))
                        .max(1),
                )
            }
            (None, Some(height_pixels)) => {
                let height_pixels = height_pixels.saturating_sub(output_pixel_offset_y);
                (
                    (height_pixels * u64::from(source_pixel_width)
                        / u64::from(source_pixel_height))
                    .max(1),
                    height_pixels,
                )
            }
            (Some(width_pixels), Some(height_pixels))
                if is_kitty_protocol && !image_display.is_unicode_placeholder =>
            {
                (
                    width_pixels.saturating_sub(output_pixel_offset_x),
                    height_pixels.saturating_sub(output_pixel_offset_y),
                )
            }
            (Some(width_pixels), Some(height_pixels))
                if image_display.is_aspect_ratio_preserved =>
            {
                if width_pixels * u64::from(source_pixel_height)
                    <= height_pixels * u64::from(source_pixel_width)
                {
                    (
                        width_pixels,
                        (width_pixels * u64::from(source_pixel_height)
                            / u64::from(source_pixel_width))
                        .max(1),
                    )
                } else {
                    (
                        (height_pixels * u64::from(source_pixel_width)
                            / u64::from(source_pixel_height))
                        .max(1),
                        height_pixels,
                    )
                }
            }
            (Some(width_pixels), Some(height_pixels)) => (width_pixels, height_pixels),
        };
    if target_width_pixels == 0 || target_height_pixels == 0 {
        return Err(ImagePlacementError::ZeroSize {
            column_count: u32::try_from(target_width_pixels).unwrap_or(u32::MAX),
            row_count: u32::try_from(target_height_pixels).unwrap_or(u32::MAX),
        });
    }

    let is_explicit_rectangle = (!is_kitty_protocol || image_display.is_unicode_placeholder)
        && requested_width_pixels.is_some()
        && requested_height_pixels.is_some();
    let (
        mut column_count,
        mut row_count,
        mut canvas_width_pixels,
        mut canvas_height_pixels,
        mut target_width_pixels,
        mut target_height_pixels,
    ) = if is_explicit_rectangle {
        let requested_width_pixels = requested_width_pixels.expect("checked above");
        let requested_height_pixels = requested_height_pixels.expect("checked above");
        let column_count = requested_width_pixels.div_ceil(cell_pixel_width);
        let row_count = requested_height_pixels.div_ceil(cell_pixel_height);
        (
            column_count,
            row_count,
            column_count * cell_pixel_width,
            row_count * cell_pixel_height,
            target_width_pixels,
            target_height_pixels,
        )
    } else {
        let column_count = (target_width_pixels + output_pixel_offset_x).div_ceil(cell_pixel_width);
        let row_count = (target_height_pixels + output_pixel_offset_y).div_ceil(cell_pixel_height);
        (
            column_count,
            row_count,
            column_count * cell_pixel_width,
            row_count * cell_pixel_height,
            target_width_pixels,
            target_height_pixels,
        )
    };
    if is_iterm_protocol {
        let available_column_count =
            u64::from(grid_dimensions.1.saturating_sub(image_record.anchor.1)).max(1);
        let is_scaling_both_axes = image_display.is_aspect_ratio_preserved
            || requested_width_pixels.is_none()
            || requested_height_pixels.is_none();
        let mut is_constrained = false;
        if column_count > available_column_count {
            if is_scaling_both_axes {
                row_count = (row_count * available_column_count / column_count).max(1);
            }
            column_count = available_column_count;
            is_constrained = true;
        }
        if row_count > 255 {
            if is_scaling_both_axes {
                column_count = (column_count * 255 / row_count).max(1);
            }
            row_count = 255;
            is_constrained = true;
        }
        if is_constrained {
            canvas_width_pixels = column_count * cell_pixel_width;
            canvas_height_pixels = row_count * cell_pixel_height;
            if is_scaling_both_axes {
                (target_width_pixels, target_height_pixels) = compute_aspect_fit_size(
                    u64::from(source_pixel_width),
                    u64::from(source_pixel_height),
                    canvas_width_pixels,
                    canvas_height_pixels,
                );
            } else {
                target_width_pixels = canvas_width_pixels;
                target_height_pixels = canvas_height_pixels;
            }
        }
    }
    let raster_byte_count = canvas_width_pixels
        .checked_mul(canvas_height_pixels)
        .and_then(|pixel_count| pixel_count.checked_mul(4))
        .unwrap_or(u64::MAX);
    if canvas_width_pixels > MAX_IMAGE_SIDE_PIXEL_COUNT as u64
        || canvas_height_pixels > MAX_IMAGE_SIDE_PIXEL_COUNT as u64
        || raster_byte_count > MAX_IMAGE_STORAGE_BYTE_COUNT as u64
    {
        return Err(ImagePlacementError::StorageLimit {
            used_byte_count: 0,
            requested_byte_count: usize::try_from(raster_byte_count).unwrap_or(usize::MAX),
            byte_limit: MAX_IMAGE_STORAGE_BYTE_COUNT,
        });
    }
    let column_count =
        u32::try_from(column_count).map_err(|_| ImagePlacementError::DimensionsTooLarge {
            column_count: u32::MAX,
            row_count: u32::MAX,
        })?;
    let row_count =
        u32::try_from(row_count).map_err(|_| ImagePlacementError::DimensionsTooLarge {
            column_count,
            row_count: u32::MAX,
        })?;
    if source_x_pixel == 0
        && source_y_pixel == 0
        && source_pixel_width == image_record.image.pixel_width
        && source_pixel_height == image_record.image.pixel_height
        && canvas_width_pixels == u64::from(source_pixel_width)
        && canvas_height_pixels == u64::from(source_pixel_height)
        && target_width_pixels == u64::from(source_pixel_width)
        && target_height_pixels == u64::from(source_pixel_height)
        && output_pixel_offset_x == 0
        && output_pixel_offset_y == 0
    {
        return Ok(PreparedRaster {
            column_count,
            row_count,
            plan: RasterPlan {
                geometry: ImageCellGeometry {
                    full_size: Size {
                        column_count: u16::try_from(column_count).map_err(|_| {
                            ImagePlacementError::DimensionsTooLarge {
                                column_count,
                                row_count,
                            }
                        })?,
                        row_count: u16::try_from(row_count).map_err(|_| {
                            ImagePlacementError::DimensionsTooLarge {
                                column_count,
                                row_count,
                            }
                        })?,
                    },
                    cell_offset: Point { column: 0, row: 0 },
                },
                source_rect: (
                    source_x_pixel,
                    source_y_pixel,
                    source_pixel_width,
                    source_pixel_height,
                ),
                target_size: (target_width_pixels as u32, target_height_pixels as u32),
                canvas_size: (canvas_width_pixels as u32, canvas_height_pixels as u32),
                pixel_offset: (output_pixel_offset_x as u32, output_pixel_offset_y as u32),
            },
            raster: None,
        });
    }
    let source_rgba_image = image::RgbaImage::from_raw(
        image_record.image.pixel_width,
        image_record.image.pixel_height,
        image_record.image.rgba_bytes.clone(),
    )
    .expect("decoded image dimensions are valid");
    let cropped_source_image = image::imageops::crop_imm(
        &source_rgba_image,
        source_x_pixel,
        source_y_pixel,
        source_pixel_width,
        source_pixel_height,
    );
    let resized_image = image::imageops::resize(
        &*cropped_source_image,
        u32::try_from(target_width_pixels).map_err(|_| {
            ImagePlacementError::DimensionsTooLarge {
                column_count,
                row_count,
            }
        })?,
        u32::try_from(target_height_pixels).map_err(|_| {
            ImagePlacementError::DimensionsTooLarge {
                column_count,
                row_count,
            }
        })?,
        image::imageops::FilterType::Triangle,
    );
    let mut raster_canvas =
        image::RgbaImage::new(canvas_width_pixels as u32, canvas_height_pixels as u32);
    image::imageops::replace(
        &mut raster_canvas,
        &resized_image,
        output_pixel_offset_x as i64,
        output_pixel_offset_y as i64,
    );
    let validated_column_count =
        u16::try_from(column_count).map_err(|_| ImagePlacementError::DimensionsTooLarge {
            column_count,
            row_count,
        })?;
    let validated_row_count =
        u16::try_from(row_count).map_err(|_| ImagePlacementError::DimensionsTooLarge {
            column_count,
            row_count,
        })?;
    let raster_image = Arc::new(DecodedImage {
        pixel_width: canvas_width_pixels as u32,
        pixel_height: canvas_height_pixels as u32,
        rgba_bytes: raster_canvas.into_raw(),
    });
    Ok(PreparedRaster {
        column_count,
        row_count,
        plan: RasterPlan {
            geometry: ImageCellGeometry {
                full_size: Size {
                    column_count: validated_column_count,
                    row_count: validated_row_count,
                },
                cell_offset: Point { column: 0, row: 0 },
            },
            source_rect: (
                source_x_pixel,
                source_y_pixel,
                source_pixel_width,
                source_pixel_height,
            ),
            target_size: (target_width_pixels as u32, target_height_pixels as u32),
            canvas_size: (canvas_width_pixels as u32, canvas_height_pixels as u32),
            pixel_offset: (output_pixel_offset_x as u32, output_pixel_offset_y as u32),
        },
        raster: Some(raster_image),
    })
}

pub(super) fn rebuild_raster_image(
    image_record: &ImageRecord,
    raster_plan: &RasterPlan,
) -> Result<Option<Arc<DecodedImage>>, ImagePlacementError> {
    if raster_plan.target_size == raster_plan.canvas_size
        && raster_plan.pixel_offset == (0, 0)
        && raster_plan.source_rect == image_record.compute_source_rect()?
        && raster_plan.target_size
            == (
                image_record.image.pixel_width,
                image_record.image.pixel_height,
            )
    {
        return Ok(None);
    }
    validate_source_pixels(image_record)?;
    let source_rgba_image = image::RgbaImage::from_raw(
        image_record.image.pixel_width,
        image_record.image.pixel_height,
        image_record.image.rgba_bytes.clone(),
    )
    .ok_or(ImagePlacementError::UnsupportedPlacement)?;
    let (source_x_pixel, source_y_pixel, source_pixel_width, source_pixel_height) =
        raster_plan.source_rect;
    let cropped_source_image = image::imageops::crop_imm(
        &source_rgba_image,
        source_x_pixel,
        source_y_pixel,
        source_pixel_width,
        source_pixel_height,
    );
    let resized_image = image::imageops::resize(
        &*cropped_source_image,
        raster_plan.target_size.0,
        raster_plan.target_size.1,
        image::imageops::FilterType::Triangle,
    );
    let mut raster_canvas =
        image::RgbaImage::new(raster_plan.canvas_size.0, raster_plan.canvas_size.1);
    image::imageops::replace(
        &mut raster_canvas,
        &resized_image,
        i64::from(raster_plan.pixel_offset.0),
        i64::from(raster_plan.pixel_offset.1),
    );
    Ok(Some(Arc::new(DecodedImage {
        pixel_width: raster_plan.canvas_size.0,
        pixel_height: raster_plan.canvas_size.1,
        rgba_bytes: raster_canvas.into_raw(),
    })))
}

fn build_identity_raster_plan(
    image_record: &ImageRecord,
    column_count: u32,
    row_count: u32,
) -> Result<RasterPlan, ImagePlacementError> {
    let source_rect = image_record.compute_source_rect()?;
    let column_count =
        u16::try_from(column_count).map_err(|_| ImagePlacementError::DimensionsTooLarge {
            column_count,
            row_count,
        })?;
    let row_count =
        u16::try_from(row_count).map_err(|_| ImagePlacementError::DimensionsTooLarge {
            column_count: u32::from(column_count),
            row_count,
        })?;
    Ok(RasterPlan {
        geometry: ImageCellGeometry {
            full_size: Size {
                column_count,
                row_count,
            },
            cell_offset: Point { column: 0, row: 0 },
        },
        source_rect,
        target_size: (
            image_record.image.pixel_width,
            image_record.image.pixel_height,
        ),
        canvas_size: (
            image_record.image.pixel_width,
            image_record.image.pixel_height,
        ),
        pixel_offset: (0, 0),
    })
}

fn validate_source_pixels(image_record: &ImageRecord) -> Result<(), ImagePlacementError> {
    let image_pixel_width = usize::try_from(image_record.image.pixel_width).map_err(|_| {
        ImagePlacementError::DimensionsTooLarge {
            column_count: image_record.image.pixel_width,
            row_count: image_record.image.pixel_height,
        }
    })?;
    let image_pixel_height = usize::try_from(image_record.image.pixel_height).map_err(|_| {
        ImagePlacementError::DimensionsTooLarge {
            column_count: image_record.image.pixel_width,
            row_count: image_record.image.pixel_height,
        }
    })?;
    let expected_rgba_byte_count = crate::graphics::compute_rgba_byte_count(
        image_record.protocol,
        image_pixel_width,
        image_pixel_height,
    )
    .map_err(|_| ImagePlacementError::DimensionsTooLarge {
        column_count: image_record.image.pixel_width,
        row_count: image_record.image.pixel_height,
    })?;
    if image_record.image.rgba_bytes.len() != expected_rgba_byte_count {
        return Err(ImagePlacementError::DimensionsTooLarge {
            column_count: image_record.image.pixel_width,
            row_count: image_record.image.pixel_height,
        });
    }
    Ok(())
}

fn compute_aspect_fit_size(
    source_pixel_width: u64,
    source_pixel_height: u64,
    available_pixel_width: u64,
    available_pixel_height: u64,
) -> (u64, u64) {
    if available_pixel_width * source_pixel_height <= available_pixel_height * source_pixel_width {
        (
            available_pixel_width,
            (available_pixel_width * source_pixel_height / source_pixel_width).max(1),
        )
    } else {
        (
            (available_pixel_height * source_pixel_width / source_pixel_height).max(1),
            available_pixel_height,
        )
    }
}

fn compute_requested_image_pixel_size(
    requested_dimension: Option<ImageDimension>,
    cell_pixel_size: u64,
    available_pixel_size: u64,
    should_round_percent_up: bool,
) -> Option<u64> {
    match requested_dimension {
        Some(ImageDimension::Cells(cell_count)) => Some(u64::from(cell_count) * cell_pixel_size),
        Some(ImageDimension::Pixels(pixel_count)) => Some(u64::from(pixel_count)),
        Some(ImageDimension::Percent(percent)) if should_round_percent_up => {
            Some((available_pixel_size * u64::from(percent)).div_ceil(100))
        }
        Some(ImageDimension::Percent(percent)) => {
            Some(available_pixel_size * u64::from(percent) / 100)
        }
        Some(ImageDimension::Auto) | None => None,
    }
}

#[cfg(test)]
mod tests;
