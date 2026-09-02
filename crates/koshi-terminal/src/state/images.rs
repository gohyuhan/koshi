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
    /// The complete placement rectangle is outside the retained primary rows.
    #[error(
        "image placement at primary row {row}, column {column} with {columns} columns by {rows} rows exceeds retained primary rows {first_row} up to but not including {retained_end}"
    )]
    HistoryOutOfBounds {
        /// The absolute row of the placement anchor.
        row: u64,
        /// The zero-based column of the placement anchor.
        column: u16,
        /// The number of covered columns.
        columns: u16,
        /// The number of covered rows.
        rows: u16,
        /// The oldest retained primary row.
        first_row: u64,
        /// The exclusive end of the retained primary row range.
        retained_end: u64,
    },
    /// The complete placement rectangle exceeds the primary grid width.
    #[error(
        "image placement at primary row {row}, column {column} with {columns} columns exceeds the {grid_columns}-column primary grid"
    )]
    HistoryWidthOutOfBounds {
        /// The absolute row of the placement anchor.
        row: u64,
        /// The zero-based column of the placement anchor.
        column: u16,
        /// The number of covered columns.
        columns: u16,
        /// The active primary grid width.
        grid_columns: u16,
    },
    /// The absolute primary row range cannot represent the live grid boundary.
    #[error("primary image row range at {total_pushed} with {grid_rows} live rows overflows u64")]
    HistoryRangeOverflow {
        /// The absolute row immediately above the live grid.
        total_pushed: u64,
        /// The number of rows in the live primary grid.
        grid_rows: u16,
    },
    /// The retained primary rows cannot be assigned nonnegative absolute rows.
    #[error(
        "primary scrollback row count {retained_rows} exceeds total pushed count {total_pushed}"
    )]
    HistoryRowsExceedCounter {
        /// The number of retained primary rows.
        retained_rows: u64,
        /// The absolute row immediately above the live grid.
        total_pushed: u64,
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

/// An image placement whose primary-screen row is addressed in the retained
/// history and live-screen row space.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct PrimaryHistoryImagePlacement {
    /// The terminal-local identity for this placement.
    id: ImagePlacementId,
    /// The complete image record retained with the placement and terminal state.
    record: Arc<ImageRecord>,
    /// The absolute primary row and column of the upper-left covered cell.
    anchor: (u64, u16),
    /// The number of covered columns.
    columns: u16,
    /// The number of covered rows.
    rows: u16,
}

/// One placement addressed by the absolute primary row space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AbsoluteImagePlacement {
    /// The terminal-local identity for this placement.
    id: ImagePlacementId,
    /// The complete image record retained with the placement and terminal state.
    record: Arc<ImageRecord>,
    /// The absolute primary row and column of the upper-left covered cell.
    anchor: (u64, u16),
    /// The number of covered columns.
    columns: u16,
    /// The number of covered rows.
    rows: u16,
}

#[derive(Debug, Clone, Copy)]
enum ActiveImagePlacementSlot {
    Live(usize),
    History(usize),
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

    fn with_anchor(&self, anchor: (u16, u16)) -> Self {
        ImagePlacement {
            id: self.id,
            record: Arc::clone(&self.record),
            anchor,
            columns: self.columns,
            rows: self.rows,
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

impl PrimaryHistoryImagePlacement {
    fn from_absolute(placement: AbsoluteImagePlacement) -> Self {
        PrimaryHistoryImagePlacement {
            id: placement.id,
            record: placement.record,
            anchor: placement.anchor,
            columns: placement.columns,
            rows: placement.rows,
        }
    }

    fn into_absolute(self) -> AbsoluteImagePlacement {
        AbsoluteImagePlacement {
            id: self.id,
            record: self.record,
            anchor: self.anchor,
            columns: self.columns,
            rows: self.rows,
        }
    }
}

impl AbsoluteImagePlacement {
    fn from_live(placement: ImagePlacement, live_top: u64) -> Option<Self> {
        Some(AbsoluteImagePlacement {
            id: placement.id,
            record: placement.record,
            anchor: (
                live_top.checked_add(u64::from(placement.anchor.0))?,
                placement.anchor.1,
            ),
            columns: placement.columns,
            rows: placement.rows,
        })
    }

    fn into_live(self, live_top: u64) -> Option<ImagePlacement> {
        Some(ImagePlacement {
            id: self.id,
            record: self.record,
            anchor: (
                u16::try_from(self.anchor.0.checked_sub(live_top)?).ok()?,
                self.anchor.1,
            ),
            columns: self.columns,
            rows: self.rows,
        })
    }

    fn with_anchor(mut self, anchor: (u64, u16)) -> Self {
        self.anchor = anchor;
        self
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

impl<'de> serde::Deserialize<'de> for PrimaryHistoryImagePlacement {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct PrimaryHistoryImagePlacementFields {
            id: ImagePlacementId,
            record: Arc<ImageRecord>,
            anchor: (u64, u16),
            columns: u16,
            rows: u16,
        }

        let fields =
            <PrimaryHistoryImagePlacementFields as serde::Deserialize>::deserialize(deserializer)?;
        validate_history_placement(&fields.record, fields.anchor, fields.columns, fields.rows)
            .map_err(serde::de::Error::custom)?;
        if fields.id == 0 {
            return Err(serde::de::Error::custom(
                "image placement identity must be nonzero",
            ));
        }

        Ok(PrimaryHistoryImagePlacement {
            id: fields.id,
            record: fields.record,
            anchor: fields.anchor,
            columns: fields.columns,
            rows: fields.rows,
        })
    }
}

fn validate_history_placement(
    record: &ImageRecord,
    anchor: (u64, u16),
    columns: u16,
    rows: u16,
) -> Result<(), String> {
    if !matches!(
        record.action,
        ImageAction::Display | ImageAction::TransmitAndDisplay
    ) {
        return Err("image placement image record must be a display record".to_string());
    }
    if columns == 0 || rows == 0 {
        return Err("image placement dimensions must be nonzero".to_string());
    }
    let (record_columns, record_rows) =
        cell_dimensions(record).map_err(|error| error.to_string())?;
    if record_columns != u32::from(columns) || record_rows != u32::from(rows) {
        return Err("image placement dimensions do not match its image record".to_string());
    }
    anchor
        .0
        .checked_add(u64::from(rows))
        .ok_or_else(|| "image placement row extent overflows u64".to_string())?;
    let column_end = u32::from(anchor.1) + u32::from(columns);
    if column_end > u32::from(u16::MAX) + 1 {
        return Err("image placement coordinate extent does not fit in u16".to_string());
    }
    Ok(())
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

pub(super) fn deserialize_primary_image_history<'de, D>(
    deserializer: D,
) -> Result<Vec<PrimaryHistoryImagePlacement>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct PrimaryHistoryImagePlacementListVisitor;

    impl<'de> serde::de::Visitor<'de> for PrimaryHistoryImagePlacementListVisitor {
        type Value = Vec<PrimaryHistoryImagePlacement>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded primary image placement history sequence")
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
            let mut storage_bytes: usize = 0;
            let mut kitty_identities = HashSet::new();
            while let Some(placement) = sequence.next_element::<PrimaryHistoryImagePlacement>()? {
                let count = placements.len().saturating_add(1);
                if count > MAX_IMAGE_PLACEMENTS {
                    return Err(serde::de::Error::custom(
                        ImagePlacementError::TooManyPlacements {
                            count,
                            limit: MAX_IMAGE_PLACEMENTS,
                        },
                    ));
                }
                if let Some(identity) = kitty_placement_identity(placement.record.as_ref()) {
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

    deserializer.deserialize_seq(PrimaryHistoryImagePlacementListVisitor)
}

pub(super) fn validate_image_state(fields: &super::TerminalStateFields) -> Result<(), String> {
    if fields.next_image_placement_id == 0 {
        return Err("next image placement identity must be nonzero".to_string());
    }
    let total_count = fields
        .primary_image_placements
        .len()
        .saturating_add(fields.primary_image_history.len())
        .saturating_add(fields.alternate_image_placements.len());
    if total_count > MAX_IMAGE_PLACEMENTS {
        return Err(ImagePlacementError::TooManyPlacements {
            count: total_count,
            limit: MAX_IMAGE_PLACEMENTS,
        }
        .to_string());
    }
    let mut ids = HashSet::with_capacity(total_count);
    let mut storage_bytes: usize = 0;
    let mut primary_kitty_identities = HashSet::new();
    for placement in &fields.primary_image_placements {
        validate_live_image_placement(
            placement,
            fields.primary.dimensions(),
            &mut ids,
            &mut primary_kitty_identities,
            &mut storage_bytes,
        )?;
    }
    let total_pushed = fields.scrollback.total_pushed();
    let retained_rows = fields.scrollback.len() as u64;
    if retained_rows > total_pushed {
        return Err(ImagePlacementError::HistoryRowsExceedCounter {
            retained_rows,
            total_pushed,
        }
        .to_string());
    }
    let grid_rows = fields.primary.dimensions().0;
    let history_first = total_pushed.saturating_sub(fields.scrollback.len() as u64);
    let live_end = total_pushed
        .checked_add(u64::from(grid_rows))
        .ok_or_else(|| {
            ImagePlacementError::HistoryRangeOverflow {
                total_pushed,
                grid_rows,
            }
            .to_string()
        })?;
    let grid_columns = fields.primary.dimensions().1;
    for placement in &fields.primary_image_history {
        if !ids.insert(placement.id) {
            return Err("image placement identities must be unique across screens".to_string());
        }
        if let Some(identity) = kitty_placement_identity(placement.record.as_ref()) {
            if !primary_kitty_identities.insert(identity) {
                return Err(
                    "kitty image and placement identities must be unique per screen".to_string(),
                );
            }
        }
        let history_end = placement
            .anchor
            .0
            .checked_add(u64::from(placement.rows))
            .ok_or_else(|| "image placement row extent overflows u64".to_string())?;
        if placement.anchor.0 >= total_pushed
            || placement.anchor.0 < history_first
            || history_end > live_end
        {
            return Err(ImagePlacementError::HistoryOutOfBounds {
                row: placement.anchor.0,
                column: placement.anchor.1,
                columns: placement.columns,
                rows: placement.rows,
                first_row: history_first,
                retained_end: live_end,
            }
            .to_string());
        }
        if u32::from(placement.anchor.1) + u32::from(placement.columns) > u32::from(grid_columns) {
            return Err(ImagePlacementError::HistoryWidthOutOfBounds {
                row: placement.anchor.0,
                column: placement.anchor.1,
                columns: placement.columns,
                grid_columns,
            }
            .to_string());
        }
        validate_image_storage(&placement.record, &mut storage_bytes)?;
    }
    let mut alternate_kitty_identities = HashSet::new();
    for placement in &fields.alternate_image_placements {
        validate_live_image_placement(
            placement,
            fields.alternate.dimensions(),
            &mut ids,
            &mut alternate_kitty_identities,
            &mut storage_bytes,
        )?;
    }
    Ok(())
}

fn validate_live_image_placement(
    placement: &ImagePlacement,
    (grid_rows, grid_columns): (u16, u16),
    ids: &mut HashSet<ImagePlacementId>,
    kitty_identities: &mut HashSet<(u32, u32)>,
    storage_bytes: &mut usize,
) -> Result<(), String> {
    if !ids.insert(placement.id) {
        return Err("image placement identities must be unique across screens".to_string());
    }
    if let Some(identity) = kitty_placement_identity(placement.record()) {
        if !kitty_identities.insert(identity) {
            return Err(
                "kitty image and placement identities must be unique per screen".to_string(),
            );
        }
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
    validate_image_storage(&placement.record, storage_bytes)
}

fn validate_image_storage(record: &ImageRecord, storage_bytes: &mut usize) -> Result<(), String> {
    let requested_bytes = record.image.rgba.len();
    *storage_bytes = storage_bytes.checked_add(requested_bytes).ok_or_else(|| {
        ImagePlacementError::StorageLimit {
            used_bytes: *storage_bytes,
            requested_bytes,
            limit_bytes: MAX_IMAGE_STORAGE_BYTES,
        }
        .to_string()
    })?;
    if *storage_bytes > MAX_IMAGE_STORAGE_BYTES {
        return Err(ImagePlacementError::StorageLimit {
            used_bytes: *storage_bytes - requested_bytes,
            requested_bytes,
            limit_bytes: MAX_IMAGE_STORAGE_BYTES,
        }
        .to_string());
    }
    Ok(())
}

impl TerminalState {
    /// Return image placements whose anchors are on the active live screen in
    /// insertion order. Primary placements in retained history are returned by
    /// [`image_placements_for_view`](Self::image_placements_for_view).
    #[must_use]
    pub fn image_placements(&self) -> &[ImagePlacement] {
        match self.active {
            Screen::Primary => &self.primary_image_placements,
            Screen::Alternate => &self.alternate_image_placements,
        }
    }

    /// Return complete image placements that fit inside the displayed view at
    /// `offset`. A primary view combines retained history with the live grid;
    /// an alternate view always uses its live grid. Placements that cross the
    /// view edge are omitted until the complete rectangle is visible.
    #[must_use]
    pub fn image_placements_for_view(&self, offset: usize) -> Vec<ImagePlacement> {
        if self.active == Screen::Alternate {
            return self.alternate_image_placements.clone();
        }

        let scrolled = self.effective_view_offset(offset);
        let (grid_rows, grid_columns) = self.primary.dimensions();
        let view_top = self
            .scrollback
            .total_pushed()
            .saturating_sub(scrolled as u64);
        let view_end = view_top.saturating_add(u64::from(grid_rows));
        let mut placements = self
            .primary_absolute_image_placements()
            .into_iter()
            .filter_map(|placement| {
                let placement_end = placement.anchor.0.checked_add(u64::from(placement.rows))?;
                let column_end = u32::from(placement.anchor.1) + u32::from(placement.columns);
                if placement.anchor.0 < view_top
                    || placement_end > view_end
                    || column_end > u32::from(grid_columns)
                {
                    return None;
                }
                let row = u16::try_from(placement.anchor.0 - view_top).ok()?;
                let column = placement.anchor.1;
                placement
                    .into_live(view_top)
                    .map(|placement| placement.with_anchor((row, column)))
            })
            .collect::<Vec<_>>();
        placements.sort_unstable_by_key(|placement| placement.id);
        placements
    }

    pub(super) fn primary_absolute_image_placements(&self) -> Vec<AbsoluteImagePlacement> {
        self.primary_absolute_image_placements_at(self.scrollback.total_pushed())
    }

    fn primary_absolute_image_placements_at(&self, live_top: u64) -> Vec<AbsoluteImagePlacement> {
        let mut placements = self
            .primary_image_history
            .iter()
            .cloned()
            .map(PrimaryHistoryImagePlacement::into_absolute)
            .collect::<Vec<_>>();
        placements.extend(
            self.primary_image_placements
                .iter()
                .cloned()
                .filter_map(|placement| AbsoluteImagePlacement::from_live(placement, live_top)),
        );
        placements
    }

    pub(super) fn remap_primary_image_placements<F>(&mut self, old_live_top: u64, mut map: F)
    where
        F: FnMut(u64, u16) -> Option<(u64, u16)>,
    {
        let mut placements = self
            .primary_absolute_image_placements_at(old_live_top)
            .into_iter()
            .filter_map(|placement| {
                let mapped_anchor = map(placement.anchor.0, placement.anchor.1)?;
                for row in 1..placement.rows {
                    let old_row = placement.anchor.0.checked_add(u64::from(row))?;
                    let mapped = map(old_row, placement.anchor.1)?;
                    if mapped
                        != (
                            mapped_anchor.0.checked_add(u64::from(row))?,
                            mapped_anchor.1,
                        )
                    {
                        return None;
                    }
                }
                Some(placement.with_anchor(mapped_anchor))
            })
            .collect::<Vec<_>>();
        self.set_primary_absolute_image_placements(&mut placements);
    }

    pub(super) fn remap_alternate_image_placements<F>(&mut self, mut map: F)
    where
        F: FnMut(u16, u16) -> Option<(u16, u16)>,
    {
        let placements = std::mem::take(&mut self.alternate_image_placements)
            .into_iter()
            .filter_map(|placement| {
                let (mapped_row, mapped_column) = map(placement.anchor.0, placement.anchor.1)?;
                for delta in 1..placement.rows {
                    let old_row = placement.anchor.0.checked_add(delta)?;
                    let mapped = map(old_row, placement.anchor.1)?;
                    if mapped != (mapped_row.checked_add(delta)?, mapped_column) {
                        return None;
                    }
                }
                Some(placement.with_anchor((mapped_row, mapped_column)))
            })
            .filter(|placement| {
                let (rows, columns) = self.alternate.dimensions();
                u32::from(placement.anchor.0) + u32::from(placement.rows) <= u32::from(rows)
                    && u32::from(placement.anchor.1) + u32::from(placement.columns)
                        <= u32::from(columns)
            })
            .collect();
        self.alternate_image_placements = placements;
    }

    fn set_primary_absolute_image_placements(
        &mut self,
        placements: &mut Vec<AbsoluteImagePlacement>,
    ) {
        placements.sort_unstable_by_key(|placement| placement.id);
        self.primary_image_placements.clear();
        self.primary_image_history.clear();

        let history_len = self.scrollback.len() as u64;
        let live_top = self.scrollback.total_pushed();
        let oldest_history_row = live_top.saturating_sub(history_len);
        let live_end = live_top.saturating_add(u64::from(self.primary.dimensions().0));
        let grid_columns = self.primary.dimensions().1;

        for placement in placements.drain(..) {
            let Some(row_end) = placement.anchor.0.checked_add(u64::from(placement.rows)) else {
                continue;
            };
            let column_end = u32::from(placement.anchor.1) + u32::from(placement.columns);
            if placement.anchor.0 < oldest_history_row
                || row_end > live_end
                || column_end > u32::from(grid_columns)
            {
                continue;
            }
            if placement.anchor.0 < live_top {
                self.primary_image_history
                    .push(PrimaryHistoryImagePlacement::from_absolute(placement));
            } else if let Some(placement) = placement.into_live(live_top) {
                self.primary_image_placements.push(placement);
            }
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

        let replacement_slot = retransmitted_image_id
            .is_none()
            .then(|| {
                kitty_placement_identity(record)
                    .and_then(|identity| self.active_image_placement_slot(identity))
            })
            .flatten();
        let (removed_count, removed_bytes) = retransmitted_image_id
            .map(|image_id| self.kitty_image_usage(image_id))
            .unwrap_or((0, 0));
        if replacement_slot.is_none() {
            let count = self
                .primary_image_placements
                .len()
                .saturating_add(self.primary_image_history.len())
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
        let replaced_bytes = replacement_slot
            .map(|slot| match slot {
                ActiveImagePlacementSlot::Live(index) => self.active_image_placements()[index]
                    .record
                    .image
                    .rgba
                    .len(),
                ActiveImagePlacementSlot::History(index) => {
                    self.primary_image_history[index].record.image.rgba.len()
                }
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
        let id = if let Some(slot) = replacement_slot {
            match slot {
                ActiveImagePlacementSlot::Live(index) => self.active_image_placements()[index].id,
                ActiveImagePlacementSlot::History(index) => self.primary_image_history[index].id,
            }
        } else {
            self.allocate_image_placement_id()?
        };
        if let Some(image_id) = retransmitted_image_id {
            self.remove_kitty_image(image_id);
        }
        let placement = ImagePlacement::new(id, record, columns, rows);
        match replacement_slot {
            Some(ActiveImagePlacementSlot::Live(index)) => {
                self.active_image_placements_mut()[index] = placement;
            }
            Some(ActiveImagePlacementSlot::History(index)) => {
                self.primary_image_history.remove(index);
                self.insert_active_image_placement_in_order(placement);
            }
            None => self.active_image_placements_mut().push(placement),
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
                .map(|placement| placement.id)
                .chain(
                    self.primary_image_history
                        .iter()
                        .map(|placement| placement.id),
                )
                .chain(
                    self.alternate_image_placements
                        .iter()
                        .map(|placement| placement.id),
                )
                .any(|id| id == candidate);
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
            .map(|placement| placement.record.image.rgba.len())
            .chain(
                self.primary_image_history
                    .iter()
                    .map(|placement| placement.record.image.rgba.len()),
            )
            .chain(
                self.alternate_image_placements
                    .iter()
                    .map(|placement| placement.record.image.rgba.len()),
            )
            .fold(0, usize::saturating_add)
    }

    fn kitty_image_usage(&self, image_id: u32) -> (usize, usize) {
        self.primary_image_placements
            .iter()
            .filter(|placement| {
                placement.record.protocol == GraphicsProtocol::Kitty
                    && placement.record.display.image_id == Some(image_id)
            })
            .map(|placement| placement.record.image.rgba.len())
            .chain(self.primary_image_history.iter().filter_map(|placement| {
                (placement.record.protocol == GraphicsProtocol::Kitty
                    && placement.record.display.image_id == Some(image_id))
                .then_some(placement.record.image.rgba.len())
            }))
            .chain(
                self.alternate_image_placements
                    .iter()
                    .filter_map(|placement| {
                        (placement.record.protocol == GraphicsProtocol::Kitty
                            && placement.record.display.image_id == Some(image_id))
                        .then_some(placement.record.image.rgba.len())
                    }),
            )
            .fold((0, 0), |(count, bytes), image_bytes| {
                (count.saturating_add(1), bytes.saturating_add(image_bytes))
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

    fn insert_active_image_placement_in_order(&mut self, placement: ImagePlacement) {
        let placements = self.active_image_placements_mut();
        let index = placements
            .iter()
            .position(|existing| existing.id > placement.id)
            .unwrap_or(placements.len());
        placements.insert(index, placement);
    }

    fn active_image_placement_slot(
        &self,
        identity: (u32, u32),
    ) -> Option<ActiveImagePlacementSlot> {
        match self.active {
            Screen::Primary => self
                .primary_image_placements
                .iter()
                .position(|placement| {
                    kitty_placement_identity(placement.record()) == Some(identity)
                })
                .map(ActiveImagePlacementSlot::Live)
                .or_else(|| {
                    self.primary_image_history
                        .iter()
                        .position(|placement| {
                            kitty_placement_identity(placement.record.as_ref()) == Some(identity)
                        })
                        .map(ActiveImagePlacementSlot::History)
                }),
            Screen::Alternate => self
                .alternate_image_placements
                .iter()
                .position(|placement| {
                    kitty_placement_identity(placement.record()) == Some(identity)
                })
                .map(ActiveImagePlacementSlot::Live),
        }
    }

    fn remove_kitty_image(&mut self, image_id: u32) {
        self.primary_image_placements.retain(|placement| {
            placement.record.protocol != GraphicsProtocol::Kitty
                || placement.record.display.image_id != Some(image_id)
        });
        self.primary_image_history.retain(|placement| {
            placement.record.protocol != GraphicsProtocol::Kitty
                || placement.record.display.image_id != Some(image_id)
        });
        self.alternate_image_placements.retain(|placement| {
            placement.record.protocol != GraphicsProtocol::Kitty
                || placement.record.display.image_id != Some(image_id)
        });
    }

    pub(super) fn clear_active_image_placements(&mut self) {
        match self.active {
            Screen::Primary => {
                let live_top = self.scrollback.total_pushed();
                self.primary_image_placements.clear();
                self.primary_image_history.retain(|placement| {
                    placement
                        .anchor
                        .0
                        .checked_add(u64::from(placement.rows))
                        .is_some_and(|end| end <= live_top)
                });
            }
            Screen::Alternate => self.alternate_image_placements.clear(),
        }
    }

    pub(super) fn clear_alternate_image_placements(&mut self) {
        self.alternate_image_placements.clear();
    }

    pub(super) fn clear_primary_image_history(&mut self) {
        self.primary_image_history.clear();
    }

    pub(super) fn clear_all_image_placements(&mut self) {
        self.primary_image_placements.clear();
        self.primary_image_history.clear();
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
