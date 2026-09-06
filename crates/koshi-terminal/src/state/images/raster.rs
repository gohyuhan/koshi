//! Pixel-sized images fitted and padded to their shared terminal-cell rectangle.

use super::*;
use crate::graphics::{DecodedImage, MAX_IMAGE_SIDE};
use koshi_core::geometry::PixelCellSize;

pub(super) struct PreparedRaster {
    pub(super) columns: u32,
    pub(super) rows: u32,
    pub(super) plan: RasterPlan,
    pub(super) raster: Option<Arc<DecodedImage>>,
}

#[cfg(test)]
pub(super) fn prepare(
    record: &ImageRecord,
    cell: Option<PixelCellSize>,
    grid: (u16, u16),
) -> Result<(u32, u32, Option<Arc<DecodedImage>>), ImagePlacementError> {
    let prepared = prepare_with_plan(record, cell, grid)?;
    Ok((prepared.columns, prepared.rows, prepared.raster))
}

pub(super) fn prepare_with_plan(
    record: &ImageRecord,
    cell: Option<PixelCellSize>,
    grid: (u16, u16),
) -> Result<PreparedRaster, ImagePlacementError> {
    let display = &record.display;
    let kitty = record.protocol == GraphicsProtocol::Kitty;
    let columns = display
        .cell_columns
        .or_else(|| cell_dimension(display.width));
    let rows = display.cell_rows.or_else(|| cell_dimension(display.height));
    if kitty
        && cell.is_some_and(|cell| {
            display.cell_offset_x.unwrap_or(0) >= u32::from(cell.width())
                || display.cell_offset_y.unwrap_or(0) >= u32::from(cell.height())
        })
    {
        return Err(ImagePlacementError::UnsupportedCellDimensions {
            width: display.width,
            height: display.height,
        });
    }
    if kitty
        && columns.is_some()
        && rows.is_some()
        && (cell.is_none()
            || (display.cell_offset_x.unwrap_or(0) == 0 && display.cell_offset_y.unwrap_or(0) == 0))
    {
        let (columns, rows) = cell_dimensions(record)?;
        validate_source_pixels(record)?;
        let plan = identity_plan(record, columns, rows)?;
        return Ok(PreparedRaster {
            columns,
            rows,
            plan,
            raster: None,
        });
    }
    let Some(cell) = cell else {
        if kitty && (columns.is_none() || rows.is_none()) {
            return Err(ImagePlacementError::MissingCellDimensions {
                width: display.width,
                height: display.height,
            });
        }
        let (columns, rows) = cell_dimensions(record)?;
        validate_source_pixels(record)?;
        let plan = identity_plan(record, columns, rows)?;
        return Ok(PreparedRaster {
            columns,
            rows,
            plan,
            raster: None,
        });
    };
    validate_source_pixels(record)?;
    let cw = u64::from(cell.width());
    let ch = u64::from(cell.height());
    let (sx, sy, sw, sh) = record.source_rect()?;
    let x = if kitty {
        u64::from(display.cell_offset_x.unwrap_or(0))
    } else {
        0
    };
    let y = if kitty {
        u64::from(display.cell_offset_y.unwrap_or(0))
    } else {
        0
    };
    if x >= cw || y >= ch {
        return Err(ImagePlacementError::UnsupportedCellDimensions {
            width: display.width,
            height: display.height,
        });
    }
    let requested_width = if kitty {
        columns.map(|value| u64::from(value) * cw)
    } else {
        pixels(display.width, cw, u64::from(grid.1) * cw)
    };
    let requested_height = if kitty {
        rows.map(|value| u64::from(value) * ch)
    } else {
        pixels(display.height, ch, u64::from(grid.0) * ch)
    };
    let (target_width, target_height) = match (requested_width, requested_height) {
        (None, None) => (u64::from(sw), u64::from(sh)),
        (Some(width), None) => {
            let width = width.saturating_sub(x);
            (width, (width * u64::from(sh) / u64::from(sw)).max(1))
        }
        (None, Some(height)) => {
            let height = height.saturating_sub(y);
            ((height * u64::from(sw) / u64::from(sh)).max(1), height)
        }
        (Some(width), Some(height)) if kitty => (width.saturating_sub(x), height.saturating_sub(y)),
        (Some(width), Some(height)) if display.preserve_aspect_ratio => {
            if width * u64::from(sh) <= height * u64::from(sw) {
                (width, (width * u64::from(sh) / u64::from(sw)).max(1))
            } else {
                ((height * u64::from(sw) / u64::from(sh)).max(1), height)
            }
        }
        (Some(width), Some(height)) => (width, height),
    };
    if target_width == 0 || target_height == 0 {
        return Err(ImagePlacementError::ZeroSize {
            columns: u32::try_from(target_width).unwrap_or(u32::MAX),
            rows: u32::try_from(target_height).unwrap_or(u32::MAX),
        });
    }

    let explicit_rectangle = !kitty && requested_width.is_some() && requested_height.is_some();
    let (columns, rows, canvas_width, canvas_height, width, height) = if explicit_rectangle {
        let requested_width = requested_width.expect("checked above");
        let requested_height = requested_height.expect("checked above");
        let columns = requested_width.div_ceil(cw);
        let rows = requested_height.div_ceil(ch);
        (
            columns,
            rows,
            columns * cw,
            rows * ch,
            target_width,
            target_height,
        )
    } else {
        let columns = (target_width + x).div_ceil(cw);
        let rows = (target_height + y).div_ceil(ch);
        (
            columns,
            rows,
            columns * cw,
            rows * ch,
            target_width,
            target_height,
        )
    };
    let bytes = canvas_width
        .checked_mul(canvas_height)
        .and_then(|value| value.checked_mul(4))
        .unwrap_or(u64::MAX);
    if canvas_width > MAX_IMAGE_SIDE as u64
        || canvas_height > MAX_IMAGE_SIDE as u64
        || bytes > MAX_IMAGE_STORAGE_BYTES as u64
    {
        return Err(ImagePlacementError::StorageLimit {
            used_bytes: 0,
            requested_bytes: usize::try_from(bytes).unwrap_or(usize::MAX),
            limit_bytes: MAX_IMAGE_STORAGE_BYTES,
        });
    }
    let columns = u32::try_from(columns).map_err(|_| ImagePlacementError::DimensionsTooLarge {
        columns: u32::MAX,
        rows: u32::MAX,
    })?;
    let rows = u32::try_from(rows).map_err(|_| ImagePlacementError::DimensionsTooLarge {
        columns,
        rows: u32::MAX,
    })?;
    if sx == 0
        && sy == 0
        && sw == record.image.width
        && sh == record.image.height
        && canvas_width == u64::from(sw)
        && canvas_height == u64::from(sh)
        && width == u64::from(sw)
        && height == u64::from(sh)
        && x == 0
        && y == 0
    {
        return Ok(PreparedRaster {
            columns,
            rows,
            plan: RasterPlan {
                geometry: ImageCellGeometry {
                    full_size: Size {
                        cols: u16::try_from(columns).map_err(|_| {
                            ImagePlacementError::DimensionsTooLarge { columns, rows }
                        })?,
                        rows: u16::try_from(rows).map_err(|_| {
                            ImagePlacementError::DimensionsTooLarge { columns, rows }
                        })?,
                    },
                    offset: Point { x: 0, y: 0 },
                },
                source: (sx, sy, sw, sh),
                target: (width as u32, height as u32),
                canvas: (canvas_width as u32, canvas_height as u32),
                pixel_offset: (x as u32, y as u32),
            },
            raster: None,
        });
    }
    let source = image::RgbaImage::from_raw(
        record.image.width,
        record.image.height,
        record.image.rgba.clone(),
    )
    .expect("decoded image dimensions are valid");
    let source = image::imageops::crop_imm(&source, sx, sy, sw, sh);
    let resized = image::imageops::resize(
        &*source,
        u32::try_from(width)
            .map_err(|_| ImagePlacementError::DimensionsTooLarge { columns, rows })?,
        u32::try_from(height)
            .map_err(|_| ImagePlacementError::DimensionsTooLarge { columns, rows })?,
        image::imageops::FilterType::Triangle,
    );
    let mut canvas = image::RgbaImage::new(canvas_width as u32, canvas_height as u32);
    image::imageops::replace(&mut canvas, &resized, x as i64, y as i64);
    let columns_u16 = u16::try_from(columns)
        .map_err(|_| ImagePlacementError::DimensionsTooLarge { columns, rows })?;
    let rows_u16 = u16::try_from(rows)
        .map_err(|_| ImagePlacementError::DimensionsTooLarge { columns, rows })?;
    let raster = Arc::new(DecodedImage {
        width: canvas_width as u32,
        height: canvas_height as u32,
        rgba: canvas.into_raw(),
    });
    Ok(PreparedRaster {
        columns,
        rows,
        plan: RasterPlan {
            geometry: ImageCellGeometry {
                full_size: Size {
                    cols: columns_u16,
                    rows: rows_u16,
                },
                offset: Point { x: 0, y: 0 },
            },
            source: (sx, sy, sw, sh),
            target: (width as u32, height as u32),
            canvas: (canvas_width as u32, canvas_height as u32),
            pixel_offset: (x as u32, y as u32),
        },
        raster: Some(raster),
    })
}

pub(super) fn rebuild(
    record: &ImageRecord,
    plan: &RasterPlan,
) -> Result<Option<Arc<DecodedImage>>, ImagePlacementError> {
    if plan.target == plan.canvas
        && plan.pixel_offset == (0, 0)
        && plan.source == record.source_rect()?
        && plan.target == (record.image.width, record.image.height)
    {
        return Ok(None);
    }
    validate_source_pixels(record)?;
    let source = image::RgbaImage::from_raw(
        record.image.width,
        record.image.height,
        record.image.rgba.clone(),
    )
    .ok_or(ImagePlacementError::UnsupportedPlacement)?;
    let (sx, sy, sw, sh) = plan.source;
    let source = image::imageops::crop_imm(&source, sx, sy, sw, sh);
    let resized = image::imageops::resize(
        &*source,
        plan.target.0,
        plan.target.1,
        image::imageops::FilterType::Triangle,
    );
    let mut canvas = image::RgbaImage::new(plan.canvas.0, plan.canvas.1);
    image::imageops::replace(
        &mut canvas,
        &resized,
        i64::from(plan.pixel_offset.0),
        i64::from(plan.pixel_offset.1),
    );
    Ok(Some(Arc::new(DecodedImage {
        width: plan.canvas.0,
        height: plan.canvas.1,
        rgba: canvas.into_raw(),
    })))
}

fn identity_plan(
    record: &ImageRecord,
    columns: u32,
    rows: u32,
) -> Result<RasterPlan, ImagePlacementError> {
    let source = record.source_rect()?;
    let columns = u16::try_from(columns)
        .map_err(|_| ImagePlacementError::DimensionsTooLarge { columns, rows })?;
    let rows = u16::try_from(rows).map_err(|_| ImagePlacementError::DimensionsTooLarge {
        columns: u32::from(columns),
        rows,
    })?;
    Ok(RasterPlan {
        geometry: ImageCellGeometry {
            full_size: Size {
                cols: columns,
                rows,
            },
            offset: Point { x: 0, y: 0 },
        },
        source,
        target: (record.image.width, record.image.height),
        canvas: (record.image.width, record.image.height),
        pixel_offset: (0, 0),
    })
}

fn validate_source_pixels(record: &ImageRecord) -> Result<(), ImagePlacementError> {
    let width = usize::try_from(record.image.width).map_err(|_| {
        ImagePlacementError::DimensionsTooLarge {
            columns: record.image.width,
            rows: record.image.height,
        }
    })?;
    let height = usize::try_from(record.image.height).map_err(|_| {
        ImagePlacementError::DimensionsTooLarge {
            columns: record.image.width,
            rows: record.image.height,
        }
    })?;
    let expected =
        crate::graphics::checked_rgba_len(record.protocol, width, height).map_err(|_| {
            ImagePlacementError::DimensionsTooLarge {
                columns: record.image.width,
                rows: record.image.height,
            }
        })?;
    if record.image.rgba.len() != expected {
        return Err(ImagePlacementError::DimensionsTooLarge {
            columns: record.image.width,
            rows: record.image.height,
        });
    }
    Ok(())
}

fn pixels(dimension: Option<ImageDimension>, cell: u64, available: u64) -> Option<u64> {
    match dimension {
        Some(ImageDimension::Cells(value)) => Some(u64::from(value) * cell),
        Some(ImageDimension::Pixels(value)) => Some(u64::from(value)),
        Some(ImageDimension::Percent(value)) => Some(available * u64::from(value) / 100),
        Some(ImageDimension::Auto) | None => None,
    }
}

#[cfg(test)]
mod tests;
