//! Image placements that belong to one terminal screen.

use std::collections::HashSet;
use std::sync::Arc;

use crate::graphics::{GraphicsProtocol, ImageAction, ImageDimension, ImageRecord};
use crate::state::{Screen, TerminalState};

/// A failure that leaves terminal image state unchanged.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error,
)]
pub enum ImagePlacementError {
    /// The display request does not contain enough cell-sized information.
    #[error("image dimensions cannot be converted to terminal cells: width {width:?}, height {height:?}")]
    MissingCellDimensions {
        /// The requested width from the image protocol.
        width: Option<ImageDimension>,
        /// The requested height from the image protocol.
        height: Option<ImageDimension>,
    },
    /// The display request uses a unit that this state model cannot map to cells.
    #[error("image dimensions use unsupported cell units: width {width:?}, height {height:?}")]
    UnsupportedCellDimensions {
        /// The requested width from the image protocol.
        width: Option<ImageDimension>,
        /// The requested height from the image protocol.
        height: Option<ImageDimension>,
    },
    /// The requested placement has no cells.
    #[error("image placement has zero cells: {columns} columns by {rows} rows")]
    ZeroSize { columns: u32, rows: u32 },
    /// The requested cell dimensions cannot fit in the coordinate type.
    #[error("image placement is too large: {columns} columns by {rows} rows")]
    DimensionsTooLarge { columns: u32, rows: u32 },
    /// The complete placement rectangle does not fit in the active grid.
    #[error(
        "image placement at row {row}, column {column} with {columns} columns by {rows} rows exceeds the {grid_rows}-row by {grid_columns}-column grid"
    )]
    OutOfBounds {
        /// The zero-based row of the placement anchor.
        row: u16,
        /// The zero-based column of the placement anchor.
        column: u16,
        /// The number of covered columns.
        columns: u16,
        /// The number of covered rows.
        rows: u16,
        /// The active grid height.
        grid_rows: u16,
        /// The active grid width.
        grid_columns: u16,
    },
    /// The terminal-local placement identity cannot advance.
    #[error("image placement identity space is exhausted")]
    IdentityExhausted,
    /// The terminal has reached its placement-count limit.
    #[error("image placement count {count} exceeds the limit of {limit}")]
    TooManyPlacements { count: usize, limit: usize },
    /// The terminal has reached its retained-image-byte limit.
    #[error(
        "image placement storage would reach {used_bytes} plus {requested_bytes} bytes, exceeding the {limit_bytes}-byte limit"
    )]
    StorageLimit {
        /// Retained RGBA bytes before the requested placement.
        used_bytes: usize,
        /// RGBA bytes in the requested placement.
        requested_bytes: usize,
        /// Maximum retained RGBA bytes across both screens.
        limit_bytes: usize,
    },
}

/// The terminal-local identity assigned to a placement without a replacement
/// identity from the protocol.
pub type ImagePlacementId = u64;

/// The maximum number of image placements retained by one terminal state.
pub(crate) const MAX_IMAGE_PLACEMENTS: usize = 4_096;

/// The maximum RGBA storage retained by one terminal state.
pub(crate) const MAX_IMAGE_STORAGE_BYTES: usize = crate::graphics::MAX_IMAGE_BYTES;

/// One displayed image and the cells covered by its placement rectangle.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ImagePlacement {
    /// The terminal-local identity for this placement.
    id: ImagePlacementId,
    /// The complete image record retained with the placement and terminal state.
    record: Arc<ImageRecord>,
    /// The zero-based row and column of the upper-left covered cell.
    anchor: (u16, u16),
    /// The number of covered columns.
    columns: u16,
    /// The number of covered rows.
    rows: u16,
}

impl ImagePlacement {
    fn new(id: ImagePlacementId, record: &ImageRecord, columns: u16, rows: u16) -> Self {
        ImagePlacement {
            id,
            record: Arc::new(record.clone()),
            anchor: record.anchor,
            columns,
            rows,
        }
    }

    /// Return the terminal-local placement identity.
    #[must_use]
    pub fn id(&self) -> ImagePlacementId {
        self.id
    }

    /// Return the complete image record retained by this placement.
    #[must_use]
    pub fn record(&self) -> &ImageRecord {
        self.record.as_ref()
    }

    /// Return the zero-based row and column of the placement anchor.
    #[must_use]
    pub fn anchor(&self) -> (u16, u16) {
        self.anchor
    }

    /// Return the placement dimensions as `(rows, columns)`.
    #[must_use]
    pub fn dimensions(&self) -> (u16, u16) {
        (self.rows, self.columns)
    }

    /// Return whether (`row`, `column`) is one of the cells covered by this
    /// placement.
    #[must_use]
    pub fn covers(&self, row: u16, column: u16) -> bool {
        u32::from(row) >= u32::from(self.anchor.0)
            && u32::from(column) >= u32::from(self.anchor.1)
            && u32::from(row) < u32::from(self.anchor.0) + u32::from(self.rows)
            && u32::from(column) < u32::from(self.anchor.1) + u32::from(self.columns)
    }

    /// Visit covered cells in row-major order.
    pub fn covered_cells(&self) -> impl Iterator<Item = (u16, u16)> + '_ {
        let (anchor_row, anchor_column) = self.anchor;
        (0..self.rows).flat_map(move |row| {
            (0..self.columns).map(move |column| {
                (
                    anchor_row
                        .checked_add(row)
                        .expect("validated image placement row fits in u16"),
                    anchor_column
                        .checked_add(column)
                        .expect("validated image placement column fits in u16"),
                )
            })
        })
    }
}

impl<'de> serde::Deserialize<'de> for ImagePlacement {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct ImagePlacementFields {
            id: ImagePlacementId,
            record: Arc<ImageRecord>,
            anchor: (u16, u16),
            columns: u16,
            rows: u16,
        }

        let fields = <ImagePlacementFields as serde::Deserialize>::deserialize(deserializer)?;
        if fields.id == 0 {
            return Err(serde::de::Error::custom(
                "image placement identity must be nonzero",
            ));
        }
        if fields.record.anchor != fields.anchor {
            return Err(serde::de::Error::custom(
                "image placement anchor does not match its image record",
            ));
        }
        if !matches!(
            fields.record.action,
            ImageAction::Display | ImageAction::TransmitAndDisplay
        ) {
            return Err(serde::de::Error::custom(
                "image placement image record must be a display record",
            ));
        }
        if fields.columns == 0 || fields.rows == 0 {
            return Err(serde::de::Error::custom(
                "image placement dimensions must be nonzero",
            ));
        }
        let (record_columns, record_rows) = cell_dimensions(fields.record.as_ref())
            .map_err(|error| serde::de::Error::custom(error.to_string()))?;
        if record_columns != u32::from(fields.columns) || record_rows != u32::from(fields.rows) {
            return Err(serde::de::Error::custom(
                "image placement dimensions do not match its image record",
            ));
        }
        let row_end = u32::from(fields.anchor.0) + u32::from(fields.rows);
        let column_end = u32::from(fields.anchor.1) + u32::from(fields.columns);
        if row_end > u32::from(u16::MAX) + 1 || column_end > u32::from(u16::MAX) + 1 {
            return Err(serde::de::Error::custom(
                "image placement coordinate extent does not fit in u16",
            ));
        }

        Ok(ImagePlacement {
            id: fields.id,
            record: fields.record,
            anchor: fields.anchor,
            columns: fields.columns,
            rows: fields.rows,
        })
    }
}

pub(super) fn default_next_image_placement_id() -> ImagePlacementId {
    1
}

pub(super) fn deserialize_image_placements<'de, D>(
    deserializer: D,
) -> Result<Vec<ImagePlacement>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct ImagePlacementListVisitor;

    impl<'de> serde::de::Visitor<'de> for ImagePlacementListVisitor {
        type Value = Vec<ImagePlacement>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded image placement sequence")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut placements = Vec::with_capacity(
                sequence
                    .size_hint()
                    .unwrap_or_default()
                    .min(MAX_IMAGE_PLACEMENTS),
            );
            let mut ids = HashSet::with_capacity(MAX_IMAGE_PLACEMENTS.min(placements.capacity()));
            let mut kitty_identities = HashSet::new();
            let mut storage_bytes: usize = 0;
            while let Some(placement) = sequence.next_element::<ImagePlacement>()? {
                let count = placements.len().saturating_add(1);
                if count > MAX_IMAGE_PLACEMENTS {
                    return Err(serde::de::Error::custom(
                        ImagePlacementError::TooManyPlacements {
                            count,
                            limit: MAX_IMAGE_PLACEMENTS,
                        },
                    ));
                }
                if !ids.insert(placement.id) {
                    return Err(serde::de::Error::custom(
                        "image placement identities must be unique per screen",
                    ));
                }
                if let Some(identity) = kitty_placement_identity(placement.record()) {
                    if !kitty_identities.insert(identity) {
                        return Err(serde::de::Error::custom(
                            "kitty image and placement identities must be unique per screen",
                        ));
                    }
                }
                let requested_bytes = placement.record.image.rgba.len();
                storage_bytes = storage_bytes.checked_add(requested_bytes).ok_or_else(|| {
                    serde::de::Error::custom(ImagePlacementError::StorageLimit {
                        used_bytes: storage_bytes,
                        requested_bytes,
                        limit_bytes: MAX_IMAGE_STORAGE_BYTES,
                    })
                })?;
                if storage_bytes > MAX_IMAGE_STORAGE_BYTES {
                    return Err(serde::de::Error::custom(
                        ImagePlacementError::StorageLimit {
                            used_bytes: storage_bytes - requested_bytes,
                            requested_bytes,
                            limit_bytes: MAX_IMAGE_STORAGE_BYTES,
                        },
                    ));
                }
                placements.push(placement);
            }
            Ok(placements)
        }
    }

    deserializer.deserialize_seq(ImagePlacementListVisitor)
}

pub(super) fn validate_image_state(
    primary: &[ImagePlacement],
    alternate: &[ImagePlacement],
    next_image_placement_id: ImagePlacementId,
    primary_dimensions: (u16, u16),
    alternate_dimensions: (u16, u16),
) -> Result<(), String> {
    if next_image_placement_id == 0 {
        return Err("next image placement identity must be nonzero".to_string());
    }
    let total_count = primary.len().saturating_add(alternate.len());
    if total_count > MAX_IMAGE_PLACEMENTS {
        return Err(ImagePlacementError::TooManyPlacements {
            count: total_count,
            limit: MAX_IMAGE_PLACEMENTS,
        }
        .to_string());
    }
    let mut ids = HashSet::with_capacity(total_count);
    let mut storage_bytes: usize = 0;
    for (placements, (grid_rows, grid_columns)) in [
        (primary, primary_dimensions),
        (alternate, alternate_dimensions),
    ] {
        for placement in placements {
            if !ids.insert(placement.id) {
                return Err("image placement identities must be unique across screens".to_string());
            }
            let row_end = u32::from(placement.anchor.0) + u32::from(placement.rows);
            let column_end = u32::from(placement.anchor.1) + u32::from(placement.columns);
            if row_end > u32::from(grid_rows) || column_end > u32::from(grid_columns) {
                return Err(ImagePlacementError::OutOfBounds {
                    row: placement.anchor.0,
                    column: placement.anchor.1,
                    columns: placement.columns,
                    rows: placement.rows,
                    grid_rows,
                    grid_columns,
                }
                .to_string());
            }
            let requested_bytes = placement.record.image.rgba.len();
            storage_bytes = storage_bytes.checked_add(requested_bytes).ok_or_else(|| {
                ImagePlacementError::StorageLimit {
                    used_bytes: storage_bytes,
                    requested_bytes,
                    limit_bytes: MAX_IMAGE_STORAGE_BYTES,
                }
                .to_string()
            })?;
            if storage_bytes > MAX_IMAGE_STORAGE_BYTES {
                return Err(ImagePlacementError::StorageLimit {
                    used_bytes: storage_bytes - requested_bytes,
                    requested_bytes,
                    limit_bytes: MAX_IMAGE_STORAGE_BYTES,
                }
                .to_string());
            }
        }
    }
    Ok(())
}

impl TerminalState {
    /// Return image placements on the active screen in insertion order.
    #[must_use]
    pub fn image_placements(&self) -> &[ImagePlacement] {
        match self.active {
            Screen::Primary => &self.primary_image_placements,
            Screen::Alternate => &self.alternate_image_placements,
        }
    }

    /// Apply one decoded image record to terminal image state.
    pub(crate) fn apply_image_record(
        &mut self,
        record: &ImageRecord,
    ) -> Result<(), ImagePlacementError> {
        match record.action {
            ImageAction::Transmit => {
                if let Some(image_id) = kitty_image_id(record) {
                    self.remove_kitty_image(image_id);
                }
                Ok(())
            }
            ImageAction::Display => self.place_image(record, None),
            ImageAction::TransmitAndDisplay => self.place_image(record, kitty_image_id(record)),
        }
    }

    fn place_image(
        &mut self,
        record: &ImageRecord,
        retransmitted_image_id: Option<u32>,
    ) -> Result<(), ImagePlacementError> {
        let (columns, rows) = cell_dimensions(record)?;
        let columns = u16::try_from(columns)
            .map_err(|_| ImagePlacementError::DimensionsTooLarge { columns, rows })?;
        let rows = u16::try_from(rows).map_err(|_| ImagePlacementError::DimensionsTooLarge {
            columns: u32::from(columns),
            rows,
        })?;
        if columns == 0 || rows == 0 {
            return Err(ImagePlacementError::ZeroSize {
                columns: u32::from(columns),
                rows: u32::from(rows),
            });
        }

        let (grid_rows, grid_columns) = self.active_grid().dimensions();
        let row_end = u32::from(record.anchor.0) + u32::from(rows);
        let column_end = u32::from(record.anchor.1) + u32::from(columns);
        if row_end > u32::from(grid_rows) || column_end > u32::from(grid_columns) {
            return Err(ImagePlacementError::OutOfBounds {
                row: record.anchor.0,
                column: record.anchor.1,
                columns,
                rows,
                grid_rows,
                grid_columns,
            });
        }

        let replacement_index = retransmitted_image_id.is_none().then(|| {
            kitty_placement_identity(record).and_then(|identity| {
                self.active_image_placements().iter().position(|placement| {
                    kitty_placement_identity(placement.record()) == Some(identity)
                })
            })
        });
        let replacement_index = replacement_index.flatten();
        let (removed_count, removed_bytes) = retransmitted_image_id
            .map(|image_id| self.kitty_image_usage(image_id))
            .unwrap_or((0, 0));
        if replacement_index.is_none() {
            let count = self
                .primary_image_placements
                .len()
                .saturating_add(self.alternate_image_placements.len())
                .saturating_sub(removed_count)
                .saturating_add(1);
            if count > MAX_IMAGE_PLACEMENTS {
                return Err(ImagePlacementError::TooManyPlacements {
                    count,
                    limit: MAX_IMAGE_PLACEMENTS,
                });
            }
        }
        let replaced_bytes = replacement_index
            .map(|index| {
                self.active_image_placements()[index]
                    .record
                    .image
                    .rgba
                    .len()
            })
            .unwrap_or(0);
        let used_bytes = self
            .image_storage_bytes()
            .saturating_sub(removed_bytes)
            .saturating_sub(replaced_bytes);
        let requested_bytes = record.image.rgba.len();
        let retained_bytes =
            used_bytes
                .checked_add(requested_bytes)
                .ok_or(ImagePlacementError::StorageLimit {
                    used_bytes,
                    requested_bytes,
                    limit_bytes: MAX_IMAGE_STORAGE_BYTES,
                })?;
        if retained_bytes > MAX_IMAGE_STORAGE_BYTES {
            return Err(ImagePlacementError::StorageLimit {
                used_bytes,
                requested_bytes,
                limit_bytes: MAX_IMAGE_STORAGE_BYTES,
            });
        }
        let id = if let Some(index) = replacement_index {
            self.active_image_placements()[index].id
        } else {
            self.allocate_image_placement_id()?
        };
        if let Some(image_id) = retransmitted_image_id {
            self.remove_kitty_image(image_id);
        }
        let placement = ImagePlacement::new(id, record, columns, rows);
        let placements = self.active_image_placements_mut();
        if let Some(index) = replacement_index {
            placements[index] = placement;
        } else {
            placements.push(placement);
        }

        if record.display.move_cursor {
            self.move_cursor_after_image(columns, rows);
        }
        Ok(())
    }

    fn allocate_image_placement_id(&mut self) -> Result<ImagePlacementId, ImagePlacementError> {
        let mut candidate = self.next_image_placement_id;
        loop {
            if candidate == 0 {
                return Err(ImagePlacementError::IdentityExhausted);
            }
            let next = candidate
                .checked_add(1)
                .ok_or(ImagePlacementError::IdentityExhausted)?;
            let used = self
                .primary_image_placements
                .iter()
                .chain(&self.alternate_image_placements)
                .any(|placement| placement.id == candidate);
            if !used {
                self.next_image_placement_id = next;
                return Ok(candidate);
            }
            candidate = next;
        }
    }

    fn image_storage_bytes(&self) -> usize {
        self.primary_image_placements
            .iter()
            .chain(&self.alternate_image_placements)
            .fold(0, |total, placement| {
                total.saturating_add(placement.record.image.rgba.len())
            })
    }

    fn kitty_image_usage(&self, image_id: u32) -> (usize, usize) {
        self.primary_image_placements
            .iter()
            .chain(&self.alternate_image_placements)
            .filter(|placement| {
                placement.record.protocol == GraphicsProtocol::Kitty
                    && placement.record.display.image_id == Some(image_id)
            })
            .fold((0, 0), |(count, bytes), placement| {
                (
                    count.saturating_add(1),
                    bytes.saturating_add(placement.record.image.rgba.len()),
                )
            })
    }

    fn active_image_placements_mut(&mut self) -> &mut Vec<ImagePlacement> {
        match self.active {
            Screen::Primary => &mut self.primary_image_placements,
            Screen::Alternate => &mut self.alternate_image_placements,
        }
    }

    fn active_image_placements(&self) -> &[ImagePlacement] {
        match self.active {
            Screen::Primary => &self.primary_image_placements,
            Screen::Alternate => &self.alternate_image_placements,
        }
    }

    fn remove_kitty_image(&mut self, image_id: u32) {
        self.primary_image_placements.retain(|placement| {
            placement.record.protocol != GraphicsProtocol::Kitty
                || placement.record.display.image_id != Some(image_id)
        });
        self.alternate_image_placements.retain(|placement| {
            placement.record.protocol != GraphicsProtocol::Kitty
                || placement.record.display.image_id != Some(image_id)
        });
    }

    pub(super) fn clear_active_image_placements(&mut self) {
        self.active_image_placements_mut().clear();
    }

    pub(super) fn clear_alternate_image_placements(&mut self) {
        self.alternate_image_placements.clear();
    }

    pub(super) fn clear_all_image_placements(&mut self) {
        self.primary_image_placements.clear();
        self.alternate_image_placements.clear();
    }

    fn move_cursor_after_image(&mut self, columns: u16, rows: u16) {
        let (grid_rows, grid_columns) = self.active_grid().dimensions();
        let cursor = self.active_cursor_mut();
        cursor.row = cursor
            .row
            .saturating_add(rows)
            .min(grid_rows.saturating_sub(1));
        cursor.col = cursor
            .col
            .saturating_add(columns)
            .min(grid_columns.saturating_sub(1));
        cursor.pending_wrap = false;
    }
}

fn kitty_placement_identity(record: &ImageRecord) -> Option<(u32, u32)> {
    (record.protocol == GraphicsProtocol::Kitty)
        .then_some((record.display.image_id?, record.display.placement_id?))
        .filter(|(image_id, placement_id)| *image_id != 0 && *placement_id != 0)
}

fn kitty_image_id(record: &ImageRecord) -> Option<u32> {
    (record.protocol == GraphicsProtocol::Kitty)
        .then_some(record.display.image_id)
        .flatten()
        .filter(|image_id| *image_id != 0)
}

fn cell_dimensions(record: &ImageRecord) -> Result<(u32, u32), ImagePlacementError> {
    match record.protocol {
        GraphicsProtocol::Kitty => kitty_cell_dimensions(record),
        GraphicsProtocol::Sixel | GraphicsProtocol::Iterm2 => explicit_cell_dimensions(record),
    }
}

fn kitty_cell_dimensions(record: &ImageRecord) -> Result<(u32, u32), ImagePlacementError> {
    if kitty_has_unsupported_dimension(record) {
        return Err(ImagePlacementError::UnsupportedCellDimensions {
            width: record.display.width,
            height: record.display.height,
        });
    }

    let columns = record
        .display
        .cell_columns
        .or_else(|| cell_dimension(record.display.width));
    let rows = record
        .display
        .cell_rows
        .or_else(|| cell_dimension(record.display.height));
    match (columns, rows) {
        (Some(columns), Some(rows)) => Ok((columns, rows)),
        (Some(columns), None) => Ok((columns, scaled_dimension(columns, record, false)?)),
        (None, Some(rows)) => Ok((scaled_dimension(rows, record, true)?, rows)),
        (None, None) => Err(ImagePlacementError::MissingCellDimensions {
            width: record.display.width,
            height: record.display.height,
        }),
    }
}

fn explicit_cell_dimensions(record: &ImageRecord) -> Result<(u32, u32), ImagePlacementError> {
    if record.display.width.is_some_and(is_non_cell_dimension)
        || record.display.height.is_some_and(is_non_cell_dimension)
    {
        return Err(ImagePlacementError::UnsupportedCellDimensions {
            width: record.display.width,
            height: record.display.height,
        });
    }

    match (
        cell_dimension(record.display.width),
        cell_dimension(record.display.height),
    ) {
        (Some(columns), Some(rows)) => Ok((columns, rows)),
        _ => Err(ImagePlacementError::MissingCellDimensions {
            width: record.display.width,
            height: record.display.height,
        }),
    }
}

fn cell_dimension(dimension: Option<ImageDimension>) -> Option<u32> {
    match dimension {
        Some(ImageDimension::Cells(value)) => Some(value),
        Some(ImageDimension::Auto) | None => None,
        Some(ImageDimension::Pixels(_) | ImageDimension::Percent(_)) => None,
    }
}

fn kitty_has_unsupported_dimension(record: &ImageRecord) -> bool {
    [record.display.width, record.display.height]
        .into_iter()
        .flatten()
        .any(|dimension| matches!(dimension, ImageDimension::Percent(_)))
}

fn is_non_cell_dimension(dimension: ImageDimension) -> bool {
    matches!(
        dimension,
        ImageDimension::Pixels(_) | ImageDimension::Percent(_)
    )
}

fn scaled_dimension(
    fixed: u32,
    record: &ImageRecord,
    width_from_height: bool,
) -> Result<u32, ImagePlacementError> {
    let source_width = match record.display.width {
        Some(ImageDimension::Pixels(value)) => value,
        _ => record.image.width,
    };
    let source_height = match record.display.height {
        Some(ImageDimension::Pixels(value)) => value,
        _ => record.image.height,
    };
    if fixed == 0 || source_width == 0 || source_height == 0 {
        return Err(ImagePlacementError::ZeroSize {
            columns: if width_from_height { 0 } else { fixed },
            rows: if width_from_height { fixed } else { 0 },
        });
    }
    let (numerator, denominator) = if width_from_height {
        (
            u64::from(fixed) * u64::from(source_width),
            u64::from(source_height),
        )
    } else {
        (
            u64::from(fixed) * u64::from(source_height),
            u64::from(source_width),
        )
    };
    let scaled = numerator
        .checked_add(denominator - 1)
        .map(|value| value / denominator)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(ImagePlacementError::DimensionsTooLarge {
            columns: if width_from_height { 0 } else { fixed },
            rows: if width_from_height { fixed } else { 0 },
        })?;
    Ok(scaled.max(1))
}
