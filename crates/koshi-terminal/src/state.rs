//! Per-pane terminal state: screen buffers, cursor, pen style (the
//! foreground/background color and attributes applied to newly written
//! text), modes, per-screen Kitty keyboard flag stacks, horizontal tab stops,
//! title, reported working directory, shell integration state and facts,
//! image placements, prompt-row marks, scrollback, and the device-reply
//! queue.
//!
//! One [`TerminalState`] backs a single terminal pane; panes never share
//! buffers. The state travels inside a per-pane
//! [`TerminalEngine`](crate::engine::TerminalEngine) — the runtime owns the
//! `PaneId → TerminalEngine` map — and carries no identity of its own.
//! The VTE performer (see the `perform` submodule) mutates this model as PTY
//! output arrives; device queries in that output (DA/DSR/DECRQM — Device
//! Attributes, Device Status Report, and Request Mode queries) queue their
//! answer bytes on the state, which the runtime drains back into the PTY.
//!
//! The state's component types live in sibling submodules — the active
//! [`Screen`], the per-screen render state and its charset slots, the cursor
//! and its saved snapshot, the mode flags with their
//! [`MouseTracking`]/[`MouseEncoding`] levels, and the [`ReportedWorkingDirectory`]. The
//! ones a caller outside this crate can name are re-exported here, reachable
//! as `koshi_terminal::state::*`.

use std::cmp::min;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use koshi_core::process::PtySize;
use koshi_sixel::{SixelGraphic, SixelPalette};

use serde::de::Visitor;
use serde::{Deserialize, Serialize};

use crate::grid::state::{Cell, Grid, RowMetadata};
use crate::scrollback::{Scrollback, ScrollbackLimit};
use crate::selection::TextView;
use crate::style::Style;

mod cursor;
pub(crate) mod images;
mod keyboard;
mod modes;
mod perform;
mod reflow;
mod render;
mod screen;
mod working_directory;

pub(crate) use cursor::{Cursor, SavedCursor};
pub(crate) use images::SixelImageSource;
pub use images::{ImagePlacement, ImagePlacementError, ImagePlacementId};
pub(crate) use keyboard::KeyboardStack;
pub(crate) use modes::TerminalModes;
pub use modes::{CursorShape, MouseEncoding, MouseTracking};
pub(crate) use render::{Charset, RenderState};
pub use screen::Screen;
pub use working_directory::ReportedWorkingDirectory;

/// The shell lifecycle point last reported through OSC 133.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
enum ShellIntegrationState {
    /// The shell is showing or returning to a prompt.
    #[default]
    Prompt,
    /// The shell has received command input.
    Input,
    /// The shell has started executing the command.
    Running,
}

/// A shell-integration fact produced by an OSC 133 marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellIntegrationFact {
    /// The shell reported that a command started.
    CommandStarted,
    /// The shell reported that a command finished.
    CommandFinished { exit_code: Option<i32> },
}

/// The full emulation state of one terminal pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalState {
    /// The shared pixel-to-cell measurement retained while no viewer is attached.
    cell_size: Option<koshi_core::geometry::PixelCellSize>,
    /// The primary (normal, scrolling) screen buffer, including row metadata,
    /// reference-counted: a render snapshot shares it without copying, and a
    /// write clones it once on demand (copy-on-write via `Arc::make_mut` in
    /// `active_grid_mut`).
    primary: Arc<Grid>,
    /// The alternate screen buffer used by full-screen apps, including row
    /// metadata; swapped in via DEC mode `?1049`/`?47` and never appended to the
    /// `scrollback`. Reference-counted like `primary`.
    alternate: Arc<Grid>,
    /// Which buffer — `primary` or `alternate` — output currently writes to and
    /// the renderer displays.
    active_screen: Screen,
    /// The cursor for the primary screen, holding its own position, origin
    /// mode, visibility, wrap latch, and saved snapshot.
    primary_cursor: Cursor,
    /// The cursor for the alternate screen, independent of the primary cursor:
    /// position, origin mode, and wrap state do not carry across screen switches.
    alternate_cursor: Cursor,
    /// The primary screen's [`RenderState`] (pen, charsets, GL slot).
    primary_render: RenderState,
    /// The alternate screen's [`RenderState`], cloned from `primary_render` on
    /// each alternate-screen entry.
    alternate_render: RenderState,
    /// Image placements whose anchors are currently on the primary live grid.
    primary_image_placements: Vec<ImagePlacement>,
    /// Primary placements whose anchors currently begin in retained history.
    primary_image_history: Vec<images::PrimaryHistoryImagePlacement>,
    /// Image placements anchored to the alternate screen's cell grid.
    alternate_image_placements: Vec<ImagePlacement>,
    /// Native image sources referenced by primary, history, or alternate cells.
    native_images: Vec<images::NativeImageSource>,
    /// Counts of native image fragments retained by source identity.
    native_fragment_count_by_image_source_id: HashMap<u64, usize>,
    /// Kitty uploads retained independently of their on-screen placements.
    kitty_images: Vec<images::KittyImage>,
    /// The next terminal-local identity assigned to a new image placement.
    next_image_placement_id: ImagePlacementId,
    /// The next terminal-local identity assigned to a canonical image source.
    next_image_content_id: images::ImageContentId,
    /// Active terminal modes (bracketed paste, mouse tracking, …).
    modes: TerminalModes,
    /// Shared Sixel color registers used by graphics whose private-register mode is off.
    sixel_palette: SixelPalette,
    /// Horizontal tab stops indexed by zero-based grid column.
    tab_stops: Vec<bool>,
    /// The window/tab title set via OSC 0/1/2; `None` until the app sets one.
    title: Option<String>,
    /// The working directory last reported by the shell via OSC 7 (host +
    /// decoded path), or `None` until the shell reports one. Read by working-directory
    /// inheritance when a new pane spawns.
    reported_working_directory: Option<ReportedWorkingDirectory>,
    /// The shell lifecycle point last reported through OSC 133.
    shell_integration_state: ShellIntegrationState,
    /// Shell-integration facts not yet taken by the terminal engine caller.
    shell_integration_facts: Vec<ShellIntegrationFact>,
    /// Lines that have scrolled off the top of the primary screen.
    scrollback: Scrollback,
    /// Primary screen's DECSTBM scroll-region margins, 0-based inclusive
    /// `(top, bottom)`; `None` scrolls the whole screen. Kept per screen: an
    /// alt-screen app's margins never reach the primary.
    primary_scroll_region: Option<(u16, u16)>,
    /// Alternate screen's scroll-region margins; see `primary_scroll_region`.
    alternate_scroll_region: Option<(u16, u16)>,
    /// Primary screen's DECSLRM left/right margins, 0-based inclusive
    /// `(left, right)`; `None` uses the full width.
    primary_horizontal_margins: Option<(u16, u16)>,
    /// Alternate screen's DECSLRM left/right margins; see
    /// `primary_horizontal_margins`.
    alternate_horizontal_margins: Option<(u16, u16)>,
    /// The primary screen's Kitty keyboard flag stack. Kept per screen (not
    /// shared): an alt-screen app's flags never reach the primary.
    primary_keyboard_stack: KeyboardStack,
    /// The alternate screen's Kitty keyboard flag stack; see
    /// `primary_keyboard_stack`. Starts empty on each actual alternate-buffer
    /// reset and inherits nothing from the primary.
    alternate_keyboard_stack: KeyboardStack,
    /// The grapheme cluster currently being built at the cursor — the run of
    /// printed code points that fold into one cell (a base plus its combining
    /// marks and any emoji continuation: ZWJ-joined parts, variation selectors,
    /// skin-tone modifiers, regional-indicator flags). Empty when no run is
    /// active; any non-printing event resets it.
    cluster: String,
    /// The `(row, column)` of the cell holding `cluster`'s base, or `None` when no
    /// run is active. Continuations attach here and width promotion widens it.
    cluster_base: Option<(u16, u16)>,
    /// Bytes queued for the running app in answer to its device queries
    /// (DA/DSR/DECRQM). The performer appends replies here; the runtime drains
    /// them via `take_device_query_replies` and writes them back into the pane's PTY.
    /// Device-global: one queue regardless of the active screen.
    device_query_replies: Vec<u8>,
}

#[derive(Serialize)]
struct TerminalStateSerializeFields<'a> {
    native_image_coverage: bool,
    cell_size: Option<koshi_core::geometry::PixelCellSize>,
    primary: &'a Arc<Grid>,
    alternate: &'a Arc<Grid>,
    #[serde(rename = "active")]
    active_screen: Screen,
    primary_cursor: &'a Cursor,
    alternate_cursor: &'a Cursor,
    primary_render: &'a RenderState,
    alternate_render: &'a RenderState,
    primary_image_placements: Vec<images::SerializedImagePlacement>,
    primary_image_history: Vec<images::SerializedImagePlacement>,
    alternate_image_placements: Vec<images::SerializedImagePlacement>,
    kitty_images: Vec<images::SerializedKittyImage>,
    image_contents: Vec<images::SerializedImageContent>,
    next_image_content_id: images::ImageContentId,
    next_image_placement_id: ImagePlacementId,
    modes: &'a TerminalModes,
    sixel_palette: &'a SixelPalette,
    tab_stops: &'a Vec<bool>,
    title: &'a Option<String>,
    #[serde(rename = "reported_cwd")]
    reported_working_directory: &'a Option<ReportedWorkingDirectory>,
    shell_integration_state: ShellIntegrationState,
    shell_integration_facts: &'a Vec<ShellIntegrationFact>,
    scrollback: &'a Scrollback,
    primary_scroll_region: &'a Option<(u16, u16)>,
    alternate_scroll_region: &'a Option<(u16, u16)>,
    #[serde(rename = "primary_horizontal_margins")]
    primary_horizontal_margins: &'a Option<(u16, u16)>,
    #[serde(rename = "alternate_horizontal_margins")]
    alternate_horizontal_margins: &'a Option<(u16, u16)>,
    primary_keyboard_stack: &'a KeyboardStack,
    alternate_keyboard_stack: &'a KeyboardStack,
    cluster: &'a String,
    cluster_base: &'a Option<(u16, u16)>,
    #[serde(rename = "replies")]
    device_query_replies: &'a Vec<u8>,
}

impl Serialize for TerminalState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let primary_image_placements = self
            .primary_image_placements
            .iter()
            .chain(
                self.native_images
                    .iter()
                    .filter(|source| source.screen == Screen::Primary)
                    .map(|source| &source.placement),
            )
            .map(images::serialize_image_placement)
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::ser::Error::custom)?;
        let primary_image_history = self
            .primary_image_history
            .iter()
            .map(images::serialize_primary_history_image_placement)
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::ser::Error::custom)?;
        let alternate_image_placements = self
            .alternate_image_placements
            .iter()
            .chain(
                self.native_images
                    .iter()
                    .filter(|source| source.screen == Screen::Alternate)
                    .map(|source| &source.placement),
            )
            .map(images::serialize_image_placement)
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::ser::Error::custom)?;
        let kitty_images = self
            .kitty_images
            .iter()
            .map(images::serialize_kitty_image)
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::ser::Error::custom)?;
        let image_contents =
            images::serialize_image_content_table(self).map_err(serde::ser::Error::custom)?;
        TerminalStateSerializeFields {
            native_image_coverage: true,
            cell_size: self.cell_size,
            primary: &self.primary,
            alternate: &self.alternate,
            active_screen: self.active_screen,
            primary_cursor: &self.primary_cursor,
            alternate_cursor: &self.alternate_cursor,
            primary_render: &self.primary_render,
            alternate_render: &self.alternate_render,
            primary_image_placements,
            primary_image_history,
            alternate_image_placements,
            kitty_images,
            image_contents,
            next_image_content_id: self.next_image_content_id,
            next_image_placement_id: self.next_image_placement_id,
            modes: &self.modes,
            sixel_palette: &self.sixel_palette,
            tab_stops: &self.tab_stops,
            title: &self.title,
            reported_working_directory: &self.reported_working_directory,
            shell_integration_state: self.shell_integration_state,
            shell_integration_facts: &self.shell_integration_facts,
            scrollback: &self.scrollback,
            primary_scroll_region: &self.primary_scroll_region,
            alternate_scroll_region: &self.alternate_scroll_region,
            primary_horizontal_margins: &self.primary_horizontal_margins,
            alternate_horizontal_margins: &self.alternate_horizontal_margins,
            primary_keyboard_stack: &self.primary_keyboard_stack,
            alternate_keyboard_stack: &self.alternate_keyboard_stack,
            cluster: &self.cluster,
            cluster_base: &self.cluster_base,
            device_query_replies: &self.device_query_replies,
        }
        .serialize(serializer)
    }
}

struct TerminalStateFields {
    native_image_coverage: bool,
    cell_size: Option<koshi_core::geometry::PixelCellSize>,
    primary: Arc<Grid>,
    alternate: Arc<Grid>,
    active_screen: Screen,
    primary_cursor: Cursor,
    alternate_cursor: Cursor,
    primary_render: RenderState,
    alternate_render: RenderState,
    primary_image_placements: Vec<ImagePlacement>,
    primary_image_history: Vec<images::PrimaryHistoryImagePlacement>,
    alternate_image_placements: Vec<ImagePlacement>,
    kitty_images: Vec<images::KittyImage>,
    next_image_placement_id: ImagePlacementId,
    next_image_content_id: images::ImageContentId,
    modes: TerminalModes,
    sixel_palette: SixelPalette,
    tab_stops: Vec<bool>,
    title: Option<String>,
    reported_working_directory: Option<ReportedWorkingDirectory>,
    shell_integration_state: ShellIntegrationState,
    shell_integration_facts: Vec<ShellIntegrationFact>,
    scrollback: Scrollback,
    primary_scroll_region: Option<(u16, u16)>,
    alternate_scroll_region: Option<(u16, u16)>,
    primary_horizontal_margins: Option<(u16, u16)>,
    alternate_horizontal_margins: Option<(u16, u16)>,
    primary_keyboard_stack: KeyboardStack,
    alternate_keyboard_stack: KeyboardStack,
    cluster: String,
    cluster_base: Option<(u16, u16)>,
    device_query_replies: Vec<u8>,
}

struct RawTerminalStateFields {
    native_image_coverage: bool,
    cell_size: Option<koshi_core::geometry::PixelCellSize>,
    primary: Arc<Grid>,
    alternate: Arc<Grid>,
    active_screen: Screen,
    primary_cursor: Cursor,
    alternate_cursor: Cursor,
    primary_render: RenderState,
    alternate_render: RenderState,
    primary_image_placements: Vec<images::SerializedImagePlacement>,
    primary_image_history: Vec<images::SerializedImagePlacement>,
    alternate_image_placements: Vec<images::SerializedImagePlacement>,
    kitty_images: Vec<images::SerializedKittyImage>,
    image_contents: Option<Vec<images::SerializedImageContent>>,
    next_image_content_id: Option<images::ImageContentId>,
    next_image_placement_id: ImagePlacementId,
    modes: TerminalModes,
    sixel_palette: SixelPalette,
    tab_stops: Vec<bool>,
    title: Option<String>,
    reported_working_directory: Option<ReportedWorkingDirectory>,
    shell_integration_state: ShellIntegrationState,
    shell_integration_facts: Vec<ShellIntegrationFact>,
    scrollback: Scrollback,
    primary_scroll_region: Option<(u16, u16)>,
    alternate_scroll_region: Option<(u16, u16)>,
    primary_horizontal_margins: Option<(u16, u16)>,
    alternate_horizontal_margins: Option<(u16, u16)>,
    primary_keyboard_stack: KeyboardStack,
    alternate_keyboard_stack: KeyboardStack,
    cluster: String,
    cluster_base: Option<(u16, u16)>,
    device_query_replies: Vec<u8>,
}

#[derive(Debug)]
enum HorizontalMarginsRestoreError {
    EmptyGrid {
        screen_name: &'static str,
    },
    InvalidRange {
        screen_name: &'static str,
        left_column_index: u16,
        right_column_index: u16,
        last_column_index: u16,
    },
}

impl Display for HorizontalMarginsRestoreError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyGrid { screen_name } => {
                write!(formatter, "{screen_name} horizontal margins require a non-empty grid")
            }
            Self::InvalidRange {
                screen_name,
                left_column_index,
                right_column_index,
                last_column_index,
            } => write!(formatter, "{screen_name} horizontal margins ({left_column_index}, {right_column_index}) must satisfy 0 <= left < right <= {last_column_index}"),
        }
    }
}

impl std::error::Error for HorizontalMarginsRestoreError {}

fn normalize_restored_horizontal_margins(
    grid: &Grid,
    horizontal_margins: Option<(u16, u16)>,
    is_declrmm_enabled: bool,
    screen_name: &'static str,
) -> Result<Option<(u16, u16)>, HorizontalMarginsRestoreError> {
    let Some((left_column_index, right_column_index)) = horizontal_margins else {
        return Ok(None);
    };
    let column_count = grid.get_grid_dimensions().1;
    let Some(last_column_index) = column_count.checked_sub(1) else {
        return Err(HorizontalMarginsRestoreError::EmptyGrid { screen_name });
    };
    if left_column_index == 0 && right_column_index == last_column_index {
        return Ok(None);
    }
    if left_column_index >= right_column_index || right_column_index > last_column_index {
        return Err(HorizontalMarginsRestoreError::InvalidRange {
            screen_name,
            left_column_index,
            right_column_index,
            last_column_index,
        });
    }
    if !is_declrmm_enabled {
        return Ok(None);
    }
    Ok(Some((left_column_index, right_column_index)))
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum RawTerminalStateField {
    NativeImageCoverage,
    CellSize,
    Primary,
    Alternate,
    Active,
    PrimaryCursor,
    AlternateCursor,
    PrimaryRender,
    AlternateRender,
    PrimaryImagePlacements,
    PrimaryImageHistory,
    AlternateImagePlacements,
    KittyImages,
    ImageContents,
    NextImageContentId,
    NextImagePlacementId,
    Modes,
    SixelPalette,
    TabStops,
    Title,
    #[serde(rename = "reported_cwd")]
    ReportedWorkingDirectory,
    ShellIntegrationState,
    ShellIntegrationFacts,
    Scrollback,
    PrimaryScrollRegion,
    AlternateScrollRegion,
    #[serde(rename = "primary_horizontal_margins")]
    PrimaryHorizontalMargins,
    #[serde(rename = "alternate_horizontal_margins")]
    AlternateHorizontalMargins,
    PrimaryKeyboardStack,
    AlternateKeyboardStack,
    Cluster,
    ClusterBase,
    #[serde(rename = "replies")]
    DeviceQueryReplies,
    #[serde(other)]
    Other,
}

struct RawTerminalStateVisitor<'a> {
    budget: &'a mut images::ImageStateBudget,
}

impl<'de> Visitor<'de> for RawTerminalStateVisitor<'_> {
    type Value = RawTerminalStateFields;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a terminal state")
    }

    fn visit_map<MapAccess>(self, mut map: MapAccess) -> Result<Self::Value, MapAccess::Error>
    where
        MapAccess: serde::de::MapAccess<'de>,
    {
        let mut native_image_coverage = None;
        let mut cell_size = None;
        let mut primary_grid = None;
        let mut alternate_grid = None;
        let mut active_screen = None;
        let mut primary_cursor = None;
        let mut alternate_cursor = None;
        let mut primary_render = None;
        let mut alternate_render = None;
        let mut primary_image_placements = None;
        let mut primary_image_history = None;
        let mut alternate_image_placements = None;
        let mut kitty_images = None;
        let mut image_contents = None;
        let mut next_image_content_id = None;
        let mut next_image_placement_id = None;
        let mut modes = None;
        let mut sixel_palette = None;
        let mut tab_stops = None;
        let mut title = None;
        let mut reported_working_directory = None;
        let mut shell_integration_state = None;
        let mut shell_integration_facts = None;
        let mut scrollback = None;
        let mut primary_scroll_region = None;
        let mut alternate_scroll_region = None;
        let mut primary_horizontal_margins = None;
        let mut alternate_horizontal_margins = None;
        let mut primary_keyboard_stack = None;
        let mut alternate_keyboard_stack = None;
        let mut cluster = None;
        let mut cluster_base = None;
        let mut device_query_replies = None;

        while let Some(terminal_state_field) = map.next_key::<RawTerminalStateField>()? {
            match terminal_state_field {
                RawTerminalStateField::NativeImageCoverage => {
                    if native_image_coverage.is_some() {
                        return Err(serde::de::Error::duplicate_field("native_image_coverage"));
                    }
                    native_image_coverage = Some(map.next_value()?);
                }
                RawTerminalStateField::CellSize => {
                    if cell_size.is_some() {
                        return Err(serde::de::Error::duplicate_field("cell_size"));
                    }
                    cell_size = Some(map.next_value()?);
                }
                RawTerminalStateField::Primary => {
                    if primary_grid.is_some() {
                        return Err(serde::de::Error::duplicate_field("primary"));
                    }
                    primary_grid = Some(map.next_value()?);
                }
                RawTerminalStateField::Alternate => {
                    if alternate_grid.is_some() {
                        return Err(serde::de::Error::duplicate_field("alternate"));
                    }
                    alternate_grid = Some(map.next_value()?);
                }
                RawTerminalStateField::Active => {
                    if active_screen.is_some() {
                        return Err(serde::de::Error::duplicate_field("active"));
                    }
                    active_screen = Some(map.next_value()?);
                }
                RawTerminalStateField::PrimaryCursor => {
                    if primary_cursor.is_some() {
                        return Err(serde::de::Error::duplicate_field("primary_cursor"));
                    }
                    primary_cursor = Some(map.next_value()?);
                }
                RawTerminalStateField::AlternateCursor => {
                    if alternate_cursor.is_some() {
                        return Err(serde::de::Error::duplicate_field("alternate_cursor"));
                    }
                    alternate_cursor = Some(map.next_value()?);
                }
                RawTerminalStateField::PrimaryRender => {
                    if primary_render.is_some() {
                        return Err(serde::de::Error::duplicate_field("primary_render"));
                    }
                    primary_render = Some(map.next_value()?);
                }
                RawTerminalStateField::AlternateRender => {
                    if alternate_render.is_some() {
                        return Err(serde::de::Error::duplicate_field("alternate_render"));
                    }
                    alternate_render = Some(map.next_value()?);
                }
                RawTerminalStateField::PrimaryImagePlacements => {
                    if primary_image_placements.is_some() {
                        return Err(serde::de::Error::duplicate_field(
                            "primary_image_placements",
                        ));
                    }
                    primary_image_placements =
                        Some(map.next_value_seed(images::SerializedImagePlacementsSeed {
                            budget: self.budget,
                        })?);
                }
                RawTerminalStateField::PrimaryImageHistory => {
                    if primary_image_history.is_some() {
                        return Err(serde::de::Error::duplicate_field("primary_image_history"));
                    }
                    primary_image_history =
                        Some(map.next_value_seed(images::SerializedImagePlacementsSeed {
                            budget: self.budget,
                        })?);
                }
                RawTerminalStateField::AlternateImagePlacements => {
                    if alternate_image_placements.is_some() {
                        return Err(serde::de::Error::duplicate_field(
                            "alternate_image_placements",
                        ));
                    }
                    alternate_image_placements =
                        Some(map.next_value_seed(images::SerializedImagePlacementsSeed {
                            budget: self.budget,
                        })?);
                }
                RawTerminalStateField::KittyImages => {
                    if kitty_images.is_some() {
                        return Err(serde::de::Error::duplicate_field("kitty_images"));
                    }
                    kitty_images =
                        Some(map.next_value_seed(images::SerializedKittyImagesSeed {
                            budget: self.budget,
                        })?);
                }
                RawTerminalStateField::ImageContents => {
                    if image_contents.is_some() {
                        return Err(serde::de::Error::duplicate_field("image_contents"));
                    }
                    image_contents = Some(map.next_value_seed(images::OptionalContentsSeed {
                        budget: self.budget,
                    })?);
                }
                RawTerminalStateField::NextImageContentId => {
                    if next_image_content_id.is_some() {
                        return Err(serde::de::Error::duplicate_field("next_image_content_id"));
                    }
                    next_image_content_id = Some(map.next_value()?);
                }
                RawTerminalStateField::NextImagePlacementId => {
                    if next_image_placement_id.is_some() {
                        return Err(serde::de::Error::duplicate_field("next_image_placement_id"));
                    }
                    next_image_placement_id = Some(map.next_value()?);
                }
                RawTerminalStateField::Modes => {
                    if modes.is_some() {
                        return Err(serde::de::Error::duplicate_field("modes"));
                    }
                    modes = Some(map.next_value()?);
                }
                RawTerminalStateField::SixelPalette => {
                    if sixel_palette.is_some() {
                        return Err(serde::de::Error::duplicate_field("sixel_palette"));
                    }
                    sixel_palette = Some(map.next_value()?);
                }
                RawTerminalStateField::TabStops => {
                    if tab_stops.is_some() {
                        return Err(serde::de::Error::duplicate_field("tab_stops"));
                    }
                    tab_stops = Some(map.next_value()?);
                }
                RawTerminalStateField::Title => {
                    if title.is_some() {
                        return Err(serde::de::Error::duplicate_field("title"));
                    }
                    title = Some(map.next_value()?);
                }
                RawTerminalStateField::ReportedWorkingDirectory => {
                    if reported_working_directory.is_some() {
                        return Err(serde::de::Error::duplicate_field("reported_cwd"));
                    }
                    reported_working_directory = Some(map.next_value()?);
                }
                RawTerminalStateField::ShellIntegrationState => {
                    if shell_integration_state.is_some() {
                        return Err(serde::de::Error::duplicate_field("shell_integration_state"));
                    }
                    shell_integration_state = Some(map.next_value()?);
                }
                RawTerminalStateField::ShellIntegrationFacts => {
                    if shell_integration_facts.is_some() {
                        return Err(serde::de::Error::duplicate_field("shell_integration_facts"));
                    }
                    shell_integration_facts = Some(map.next_value()?);
                }
                RawTerminalStateField::Scrollback => {
                    if scrollback.is_some() {
                        return Err(serde::de::Error::duplicate_field("scrollback"));
                    }
                    scrollback = Some(map.next_value()?);
                }
                RawTerminalStateField::PrimaryScrollRegion => {
                    if primary_scroll_region.is_some() {
                        return Err(serde::de::Error::duplicate_field("primary_scroll_region"));
                    }
                    primary_scroll_region = Some(map.next_value()?);
                }
                RawTerminalStateField::AlternateScrollRegion => {
                    if alternate_scroll_region.is_some() {
                        return Err(serde::de::Error::duplicate_field("alternate_scroll_region"));
                    }
                    alternate_scroll_region = Some(map.next_value()?);
                }
                RawTerminalStateField::PrimaryHorizontalMargins => {
                    if primary_horizontal_margins.is_some() {
                        return Err(serde::de::Error::duplicate_field(
                            "primary_horizontal_margins",
                        ));
                    }
                    primary_horizontal_margins = Some(map.next_value()?);
                }
                RawTerminalStateField::AlternateHorizontalMargins => {
                    if alternate_horizontal_margins.is_some() {
                        return Err(serde::de::Error::duplicate_field(
                            "alternate_horizontal_margins",
                        ));
                    }
                    alternate_horizontal_margins = Some(map.next_value()?);
                }
                RawTerminalStateField::PrimaryKeyboardStack => {
                    if primary_keyboard_stack.is_some() {
                        return Err(serde::de::Error::duplicate_field("primary_keyboard_stack"));
                    }
                    primary_keyboard_stack = Some(map.next_value()?);
                }
                RawTerminalStateField::AlternateKeyboardStack => {
                    if alternate_keyboard_stack.is_some() {
                        return Err(serde::de::Error::duplicate_field(
                            "alternate_keyboard_stack",
                        ));
                    }
                    alternate_keyboard_stack = Some(map.next_value()?);
                }
                RawTerminalStateField::Cluster => {
                    if cluster.is_some() {
                        return Err(serde::de::Error::duplicate_field("cluster"));
                    }
                    cluster = Some(map.next_value()?);
                }
                RawTerminalStateField::ClusterBase => {
                    if cluster_base.is_some() {
                        return Err(serde::de::Error::duplicate_field("cluster_base"));
                    }
                    cluster_base = Some(map.next_value()?);
                }
                RawTerminalStateField::DeviceQueryReplies => {
                    if device_query_replies.is_some() {
                        return Err(serde::de::Error::duplicate_field("replies"));
                    }
                    device_query_replies = Some(map.next_value()?);
                }
                RawTerminalStateField::Other => {
                    let _: serde::de::IgnoredAny = map.next_value()?;
                }
            }
        }

        Ok(RawTerminalStateFields {
            native_image_coverage: native_image_coverage.unwrap_or(false),
            cell_size: cell_size.unwrap_or_default(),
            primary: primary_grid.ok_or_else(|| serde::de::Error::missing_field("primary"))?,
            alternate: alternate_grid
                .ok_or_else(|| serde::de::Error::missing_field("alternate"))?,
            active_screen: active_screen
                .ok_or_else(|| serde::de::Error::missing_field("active"))?,
            primary_cursor: primary_cursor
                .ok_or_else(|| serde::de::Error::missing_field("primary_cursor"))?,
            alternate_cursor: alternate_cursor
                .ok_or_else(|| serde::de::Error::missing_field("alternate_cursor"))?,
            primary_render: primary_render
                .ok_or_else(|| serde::de::Error::missing_field("primary_render"))?,
            alternate_render: alternate_render
                .ok_or_else(|| serde::de::Error::missing_field("alternate_render"))?,
            primary_image_placements: primary_image_placements.unwrap_or_default(),
            primary_image_history: primary_image_history.unwrap_or_default(),
            alternate_image_placements: alternate_image_placements.unwrap_or_default(),
            kitty_images: kitty_images.unwrap_or_default(),
            image_contents: image_contents.unwrap_or_default(),
            next_image_content_id: next_image_content_id.unwrap_or_default(),
            next_image_placement_id: next_image_placement_id
                .unwrap_or_else(images::default_next_image_placement_id),
            modes: modes.ok_or_else(|| serde::de::Error::missing_field("modes"))?,
            sixel_palette: sixel_palette.unwrap_or_default(),
            tab_stops: tab_stops.ok_or_else(|| serde::de::Error::missing_field("tab_stops"))?,
            title: title.ok_or_else(|| serde::de::Error::missing_field("title"))?,
            reported_working_directory: reported_working_directory
                .ok_or_else(|| serde::de::Error::missing_field("reported_cwd"))?,
            shell_integration_state: shell_integration_state.unwrap_or_default(),
            shell_integration_facts: shell_integration_facts.unwrap_or_default(),
            scrollback: scrollback.ok_or_else(|| serde::de::Error::missing_field("scrollback"))?,
            primary_scroll_region: primary_scroll_region
                .ok_or_else(|| serde::de::Error::missing_field("primary_scroll_region"))?,
            alternate_scroll_region: alternate_scroll_region
                .ok_or_else(|| serde::de::Error::missing_field("alternate_scroll_region"))?,
            primary_horizontal_margins: primary_horizontal_margins.unwrap_or_default(),
            alternate_horizontal_margins: alternate_horizontal_margins.unwrap_or_default(),
            primary_keyboard_stack: primary_keyboard_stack.unwrap_or_default(),
            alternate_keyboard_stack: alternate_keyboard_stack.unwrap_or_default(),
            cluster: cluster.ok_or_else(|| serde::de::Error::missing_field("cluster"))?,
            cluster_base: cluster_base
                .ok_or_else(|| serde::de::Error::missing_field("cluster_base"))?,
            device_query_replies: device_query_replies
                .ok_or_else(|| serde::de::Error::missing_field("replies"))?,
        })
    }
}

impl<'de> Deserialize<'de> for RawTerminalStateFields {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let mut budget = images::ImageStateBudget::new();
        Self::deserialize_with_budget(deserializer, &mut budget)
    }
}

impl RawTerminalStateFields {
    fn deserialize_with_budget<'de, Deserializer>(
        deserializer: Deserializer,
        budget: &mut images::ImageStateBudget,
    ) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(RawTerminalStateVisitor { budget })
    }
}

impl<'de> Deserialize<'de> for TerminalStateFields {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let serialized_terminal_state = RawTerminalStateFields::deserialize(deserializer)?;
        let is_declrmm_enabled = serialized_terminal_state.modes.declrmm;
        let primary_horizontal_margins = normalize_restored_horizontal_margins(
            &serialized_terminal_state.primary,
            serialized_terminal_state.primary_horizontal_margins,
            is_declrmm_enabled,
            "primary",
        )
        .map_err(serde::de::Error::custom)?;
        let alternate_horizontal_margins = normalize_restored_horizontal_margins(
            &serialized_terminal_state.alternate,
            serialized_terminal_state.alternate_horizontal_margins,
            is_declrmm_enabled,
            "alternate",
        )
        .map_err(serde::de::Error::custom)?;
        let images = images::restore_serialized_image_state(
            serialized_terminal_state.primary_image_placements,
            serialized_terminal_state.primary_image_history,
            serialized_terminal_state.alternate_image_placements,
            serialized_terminal_state.kitty_images,
            serialized_terminal_state.image_contents,
            serialized_terminal_state.next_image_content_id,
        )
        .map_err(serde::de::Error::custom)?;
        Ok(Self {
            native_image_coverage: serialized_terminal_state.native_image_coverage,
            cell_size: serialized_terminal_state.cell_size,
            primary: serialized_terminal_state.primary,
            alternate: serialized_terminal_state.alternate,
            active_screen: serialized_terminal_state.active_screen,
            primary_cursor: serialized_terminal_state.primary_cursor,
            alternate_cursor: serialized_terminal_state.alternate_cursor,
            primary_render: serialized_terminal_state.primary_render,
            alternate_render: serialized_terminal_state.alternate_render,
            primary_image_placements: images.primary_image_placements,
            primary_image_history: images.primary_image_history,
            alternate_image_placements: images.alternate_image_placements,
            kitty_images: images.kitty_images,
            next_image_placement_id: serialized_terminal_state.next_image_placement_id,
            next_image_content_id: images.next_image_content_id,
            modes: serialized_terminal_state.modes,
            sixel_palette: serialized_terminal_state.sixel_palette,
            tab_stops: serialized_terminal_state.tab_stops,
            title: serialized_terminal_state.title,
            reported_working_directory: serialized_terminal_state.reported_working_directory,
            shell_integration_state: serialized_terminal_state.shell_integration_state,
            shell_integration_facts: serialized_terminal_state.shell_integration_facts,
            scrollback: serialized_terminal_state.scrollback,
            primary_scroll_region: serialized_terminal_state.primary_scroll_region,
            alternate_scroll_region: serialized_terminal_state.alternate_scroll_region,
            primary_horizontal_margins,
            alternate_horizontal_margins,
            primary_keyboard_stack: serialized_terminal_state.primary_keyboard_stack,
            alternate_keyboard_stack: serialized_terminal_state.alternate_keyboard_stack,
            cluster: serialized_terminal_state.cluster,
            cluster_base: serialized_terminal_state.cluster_base,
            device_query_replies: serialized_terminal_state.device_query_replies,
        })
    }
}

impl<'de> Deserialize<'de> for TerminalState {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let fields = TerminalStateFields::deserialize(deserializer)?;
        images::validate_image_state(&fields).map_err(serde::de::Error::custom)?;

        let native_image_coverage = fields.native_image_coverage;
        let mut terminal_state = TerminalState {
            native_images: Vec::new(),
            native_fragment_count_by_image_source_id: HashMap::new(),
            cell_size: fields.cell_size,
            primary: fields.primary,
            alternate: fields.alternate,
            active_screen: fields.active_screen,
            primary_cursor: fields.primary_cursor,
            alternate_cursor: fields.alternate_cursor,
            primary_render: fields.primary_render,
            alternate_render: fields.alternate_render,
            primary_image_placements: fields.primary_image_placements,
            primary_image_history: fields.primary_image_history,
            alternate_image_placements: fields.alternate_image_placements,
            kitty_images: fields.kitty_images,
            next_image_placement_id: fields.next_image_placement_id,
            next_image_content_id: fields.next_image_content_id,
            modes: fields.modes,
            sixel_palette: fields.sixel_palette,
            tab_stops: fields.tab_stops,
            title: fields.title,
            reported_working_directory: fields.reported_working_directory,
            shell_integration_state: fields.shell_integration_state,
            shell_integration_facts: fields.shell_integration_facts,
            scrollback: fields.scrollback,
            primary_scroll_region: fields.primary_scroll_region,
            alternate_scroll_region: fields.alternate_scroll_region,
            primary_horizontal_margins: fields.primary_horizontal_margins,
            alternate_horizontal_margins: fields.alternate_horizontal_margins,
            primary_keyboard_stack: fields.primary_keyboard_stack,
            alternate_keyboard_stack: fields.alternate_keyboard_stack,
            cluster: fields.cluster,
            cluster_base: fields.cluster_base,
            device_query_replies: fields.device_query_replies,
        };
        terminal_state
            .restore_native_image_coverage(native_image_coverage)
            .map_err(serde::de::Error::custom)?;
        Ok(terminal_state)
    }
}

impl TerminalState {
    /// Set the shared pixel measurement used by new images and size reports.
    pub fn set_cell_size(&mut self, pixel_cell_size: koshi_core::geometry::PixelCellSize) {
        self.cell_size = Some(pixel_cell_size);
    }

    /// Return the shared terminal cell measurement in pixels.
    #[must_use]
    pub fn get_cell_size(&self) -> Option<koshi_core::geometry::PixelCellSize> {
        self.cell_size
    }

    pub(crate) fn apply_sixel_graphic(
        &mut self,
        sixel_graphic: SixelGraphic,
        anchor_position: (u16, u16),
    ) -> Result<Option<crate::graphics::ImageRecord>, crate::graphics::GraphicsError> {
        let mut palette = if self.modes.sixel_private_color_registers {
            SixelPalette::default()
        } else {
            self.sixel_palette.clone()
        };
        palette.apply_palette_changes(sixel_graphic.get_palette_changes());
        if !self.modes.sixel_private_color_registers {
            self.sixel_palette = palette.clone();
        }

        let is_shared_palette = !self.modes.sixel_private_color_registers;
        if is_shared_palette
            && !sixel_graphic
                .get_palette_changes()
                .list_palette_changes()
                .is_empty()
        {
            self.refresh_shared_sixel_images(&palette)?;
        }

        let Some(indexed_image) = sixel_graphic.get_indexed_image() else {
            return Ok(None);
        };
        let sixel_image_source = SixelImageSource::from_indexed_image(
            indexed_image.clone(),
            palette.clone(),
            is_shared_palette,
        );
        let sixel_image =
            self.get_or_share_image_pixels(sixel_image_source.resolve_sixel_image(&palette)?);
        let should_scroll_cursor = self.modes.sixel_scrolling;
        let image_record = crate::graphics::ImageRecord {
            protocol: crate::graphics::GraphicsProtocol::Sixel,
            image: sixel_image,
            animation: None,
            action: crate::graphics::ImageAction::Display,
            display: crate::graphics::ImageDisplay {
                should_move_cursor: should_scroll_cursor,
                sixel_background: Some(sixel_graphic.get_sixel_background()),
                ..crate::graphics::ImageDisplay::default()
            },
            anchor: if should_scroll_cursor {
                anchor_position
            } else {
                (0, 0)
            },
        };
        self.apply_sixel_image_record(
            &image_record,
            should_scroll_cursor,
            self.modes.sixel_cursor_right,
            sixel_image_source,
        )
        .map_err(
            |placement_error| crate::graphics::GraphicsError::PlacementRejected {
                protocol: crate::graphics::GraphicsProtocol::Sixel,
                placement_error,
            },
        )?;
        Ok(Some(image_record))
    }

    /// Create per-pane state for a terminal of `pty_size`: both screen buffers
    /// blank, the cursor at the top-left and visible, default pen, no title.
    pub fn from_pty_size(pty_size: PtySize) -> Self {
        Self::with_scrollback(pty_size, ScrollbackLimit::default())
    }

    /// Like [`from_pty_size`](Self::from_pty_size), but with an explicit scrollback limit.
    pub fn with_scrollback(pty_size: PtySize, scrollback_limit: ScrollbackLimit) -> Self {
        let blank_screen = Grid::blank(pty_size.row_count, pty_size.column_count, Style::default());
        let home_cursor = Cursor {
            row: 0,
            column: 0,
            is_visible: true,
            pending_wrap: false,
            origin: false,
            saved: None,
        };
        TerminalState {
            cell_size: None,
            primary: Arc::new(blank_screen.clone()),
            alternate: Arc::new(blank_screen),
            active_screen: Screen::Primary,
            primary_cursor: home_cursor,
            alternate_cursor: home_cursor,
            primary_render: RenderState::fresh(),
            alternate_render: RenderState::fresh(),
            native_images: Vec::new(),
            native_fragment_count_by_image_source_id: HashMap::new(),
            primary_image_placements: Vec::new(),
            primary_image_history: Vec::new(),
            alternate_image_placements: Vec::new(),
            kitty_images: Vec::new(),
            next_image_placement_id: images::default_next_image_placement_id(),
            next_image_content_id: images::default_next_image_content_id(),
            modes: TerminalModes::default(),
            sixel_palette: SixelPalette::default(),
            tab_stops: build_default_tab_stops(pty_size.column_count),
            title: None,
            reported_working_directory: None,
            shell_integration_state: ShellIntegrationState::default(),
            shell_integration_facts: Vec::new(),
            scrollback: Scrollback::from_scrollback_limit(scrollback_limit),
            primary_scroll_region: None,
            alternate_scroll_region: None,
            primary_horizontal_margins: None,
            alternate_horizontal_margins: None,
            primary_keyboard_stack: KeyboardStack::default(),
            alternate_keyboard_stack: KeyboardStack::default(),
            cluster: String::new(),
            cluster_base: None,
            device_query_replies: Vec::new(),
        }
    }

    /// Resize the tab-stop table while keeping stops in surviving columns.
    fn resize_tab_stops(&mut self, column_count: u16) {
        let retained_tab_stop_count = self.tab_stops.len().min(column_count as usize);
        self.tab_stops.truncate(column_count as usize);
        self.tab_stops
            .extend((retained_tab_stop_count..column_count as usize).map(|column| column % 8 == 0));
    }

    /// Resize both screen buffers to `size`, preserving their contents.
    ///
    /// The primary screen REFLOWS: soft-wrapped rows re-join into logical
    /// lines ([`RowEnd`](crate::grid::state::RowEnd)) and re-wrap to the new
    /// width, while prompt marks stay with their rows. Text wider than the new
    /// width wraps onto continuation rows, and widening re-joins what an
    /// earlier narrow width wrapped. Rows past the new height scroll into
    /// history (trailing blank padding rows drop instead), a taller screen
    /// pulls history back in, and the cursor stays on its logical line at its
    /// content offset. Cursor-line tracking holds for heights of one row or
    /// more; a zero-row resize parks every row in history without panicking,
    /// and after regrowing the cursor restarts on the first logical line.
    /// The alternate screen has no history: each row crops on the right or
    /// pads with the screen's own background (a wide glyph whose right half is
    /// cut off is blanked), and a height shrink crops off the top. Both
    /// screens' vertical and horizontal margins are dropped until the app
    /// issues DECSTBM or DECSLRM again.
    /// Primary image anchors follow their row's reflowed text and remain in the
    /// primary history list while any of their cells stay addressable.
    /// Alternate placements follow the cropped rows and retain their image scale.
    /// Both cursors are clamped into the new bounds with their wrap latch
    /// cleared, and an in-progress grapheme cluster is dropped.
    pub fn resize_terminal_state(&mut self, pty_size: PtySize) {
        let alternate_fill = self.alternate_render.style.get_background_fill_style();

        self.resize_tab_stops(pty_size.column_count);
        self.reflow_primary(pty_size);

        // The alternate screen keeps what fits: crop off the top, pad at the
        // bottom, no history on either side. Row metadata follows each row.
        let mut alternate_rows: Vec<(Vec<Cell>, RowMetadata)> = self
            .alternate
            .list_rows()
            .iter()
            .enumerate()
            .map(|(row_index, row_cells)| {
                (
                    row_cells.clone(),
                    self.alternate.get_row_metadata(row_index as u16),
                )
            })
            .collect();
        for (row_cells, _) in &mut alternate_rows {
            crop_columns(row_cells, pty_size.column_count, alternate_fill);
        }
        let dropped_top_row_count = alternate_rows
            .len()
            .saturating_sub(pty_size.row_count as usize);
        alternate_rows.drain(..dropped_top_row_count);
        self.alternate_cursor.row = self
            .alternate_cursor
            .row
            .saturating_sub(u16::try_from(dropped_top_row_count).unwrap_or(u16::MAX));
        alternate_rows.resize(
            pty_size.row_count as usize,
            (
                vec![Cell::blank_with(alternate_fill); pty_size.column_count as usize],
                RowMetadata::default(),
            ),
        );
        self.alternate = Arc::new(Grid::from_rows_with_metadata(
            alternate_rows,
            pty_size.column_count,
            alternate_fill,
        ));

        self.remap_alternate_image_placements(|row_index, column_index| {
            Some((
                row_index.checked_sub(u16::try_from(dropped_top_row_count).ok()?)?,
                column_index,
            ))
        });
        self.rebuild_native_fragment_count_by_image_source_id();

        // Clamp both cursors to the new bounds.
        self.primary_cursor.row = min(
            self.primary_cursor.row,
            pty_size.row_count.saturating_sub(1),
        );
        self.primary_cursor.column = min(
            self.primary_cursor.column,
            pty_size.column_count.saturating_sub(1),
        );
        self.primary_cursor.pending_wrap = false;

        self.alternate_cursor.row = min(
            self.alternate_cursor.row,
            pty_size.row_count.saturating_sub(1),
        );
        self.alternate_cursor.column = min(
            self.alternate_cursor.column,
            pty_size.column_count.saturating_sub(1),
        );
        self.alternate_cursor.pending_wrap = false;

        // Both margin axes are dropped: the resized screens use their full
        // dimensions until the app issues DECSTBM or DECSLRM again.
        self.primary_scroll_region = None;
        self.alternate_scroll_region = None;
        self.primary_horizontal_margins = None;
        self.alternate_horizontal_margins = None;

        // An in-progress cluster is dropped: its recorded base position indexes
        // the old geometry.
        self.cluster.clear();
        self.cluster_base = None;
    }

    /// Which screen (primary or alternate) is currently displayed and written to.
    pub fn get_active_screen(&self) -> Screen {
        self.active_screen
    }

    /// Whether the primary screen — the one that keeps scrollback history — is
    /// the active one. `false` while a full-screen program holds the alternate
    /// screen, which keeps no history.
    pub fn is_primary_screen_active(&self) -> bool {
        matches!(self.active_screen, Screen::Primary)
    }

    /// The screen buffer currently displayed and written to — `primary` or
    /// `alternate`, per the active screen.
    pub fn get_active_grid(&self) -> &Grid {
        match self.active_screen {
            Screen::Primary => self.primary.as_ref(),
            Screen::Alternate => self.alternate.as_ref(),
        }
    }

    /// Mutable access to the active screen buffer, for writing cells. Clones the
    /// buffer once (copy-on-write) if a render snapshot still shares it; the
    /// snapshot keeps the pre-write contents.
    pub(crate) fn active_grid_mut(&mut self) -> &mut Grid {
        match self.active_screen {
            Screen::Primary => Arc::make_mut(&mut self.primary),
            Screen::Alternate => Arc::make_mut(&mut self.alternate),
        }
    }

    /// A reference-counted handle to the active screen buffer for the render
    /// snapshot: clones the `Arc`, not the grid. The next write to this screen
    /// clones the buffer once, leaving this handle pointing at the frozen
    /// contents.
    pub fn get_active_grid_arc(&self) -> Arc<Grid> {
        match self.active_screen {
            Screen::Primary => Arc::clone(&self.primary),
            Screen::Alternate => Arc::clone(&self.alternate),
        }
    }

    /// This pane's text as one space addressed by absolute row number: the
    /// retained history plus the live screen on the primary, and the screen
    /// alone on the alternate, which keeps no history of its own.
    ///
    /// The scrollback belongs to the primary and stays while the alternate is
    /// up; the alternate's view holds its grid alone. A word or line grown from
    /// the alternate's top row stops at that row.
    pub fn get_text_view(&self) -> TextView<'_> {
        match self.active_screen {
            Screen::Primary => {
                TextView::from_scrollback_and_grid(&self.scrollback, self.get_active_grid())
            }
            Screen::Alternate => TextView::from_grid_without_scrollback(
                self.get_active_grid(),
                self.scrollback.get_total_pushed_line_count(),
            ),
        }
    }

    /// How far the view is *actually* scrolled when `offset` was asked for:
    /// `offset` clamped to the retained line count, and `0` on the alternate
    /// screen (which keeps no scrollback) or with no history to show.
    ///
    /// This is the one place the clamp happens; the composed grid, the scroll
    /// indicator, cursor suppression, and the row a selection resolves to all
    /// read it.
    pub fn effective_view_offset(&self, offset: usize) -> usize {
        if !self.is_primary_screen_active() {
            return 0;
        }
        offset.min(self.scrollback.get_retained_line_count())
    }

    /// The active screen buffer the renderer should draw at scrollback view
    /// `offset` — lines scrolled up from the live bottom, `0` following live
    /// output — paired with the *effective* offset actually shown.
    ///
    /// The effective offset is the single source of truth for how far the view is
    /// scrolled: it is `0` (and the buffer travels by reference, no copy) when
    /// `offset` is `0`, on the alternate screen (which keeps no scrollback), or
    /// with empty history. In every other case it is `offset` clamped to the
    /// retained line count: an over-scrolled or stale value stops at the oldest
    /// line. The composed grid, the scroll indicator, and cursor suppression all
    /// read the returned value.
    ///
    /// A non-zero effective offset composes a fresh window `rows` tall from the
    /// primary screen: its top rows are the newest scrollback lines, its lower
    /// rows the top of the live grid. A view scrolled that many lines up shows
    /// that much history with the rest of the live screen below.
    ///
    /// History stores a row's text without the default blanks that padded it out
    /// to the screen width; composing the window pads each history row back out
    /// with default blanks, not the running program's current background. Live
    /// rows already span the full width.
    pub fn scrolled_view(&self, offset: usize) -> (Arc<Grid>, usize) {
        let scrolled = self.effective_view_offset(offset);
        if scrolled == 0 {
            return (self.get_active_grid_arc(), 0);
        }

        let grid = self.primary.as_ref();
        let (row_count, column_count) = grid.get_grid_dimensions();
        let history = self.scrollback.list_retained_lines();
        let retained_line_count = history.len();

        // The visible window: the `scrolled` newest history rows, then the live
        // rows, capped at the screen height. The live grid alone is `rows` tall;
        // the chain always yields a full window and keeps row metadata.
        let window: Vec<(Vec<Cell>, RowMetadata)> = history
            .iter()
            .skip(retained_line_count - scrolled)
            .map(|(row_cells, row_metadata)| (row_cells.clone(), *row_metadata))
            .chain(
                grid.list_rows()
                    .iter()
                    .enumerate()
                    .map(|(row_index, row_cells)| {
                        (row_cells.clone(), grid.get_row_metadata(row_index as u16))
                    }),
            )
            .take(row_count as usize)
            .collect();
        (
            Arc::new(Grid::from_rows_with_metadata(
                window,
                column_count,
                Style::default(),
            )),
            scrolled,
        )
    }

    /// The window/tab title set by OSC 0/1/2, or `None` if the app has not set
    /// one.
    pub fn get_title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// The working directory last reported by the shell via OSC 7 (its host and
    /// decoded path), or `None` if none has been reported. The pane-spawn layer
    /// compares the host to the local machine before inheriting the path; a
    /// directory reported from a remote host (over SSH) is not opened locally.
    pub fn get_current_working_directory(&self) -> Option<&ReportedWorkingDirectory> {
        self.reported_working_directory.as_ref()
    }

    /// Whether the cursor should be drawn — toggled by DECTCEM (`?25`).
    pub fn is_cursor_visible(&self) -> bool {
        self.active_cursor().is_visible
    }

    /// Whether bracketed-paste mode (`?2004`) is active — the input layer reads
    /// this to decide whether to bracket a paste in `ESC[200~`…`ESC[201~`.
    pub fn is_bracketed_paste_enabled(&self) -> bool {
        self.modes.bracketed_paste
    }

    /// The active mouse tracking level (`?9`/`?1000`/`?1002`/`?1003`) — the
    /// mouse layer reads this to decide which events to report to the app.
    pub fn get_mouse_tracking(&self) -> MouseTracking {
        self.modes.mouse_tracking
    }

    /// The active mouse report encoding (`?1005`/`?1006`/`?1015`) — the mouse
    /// layer reads this to format the coordinates of a report.
    pub fn get_mouse_encoding(&self) -> MouseEncoding {
        self.modes.mouse_encoding
    }

    /// Whether alternate-scroll mode (`?1007`) is active — the mouse layer reads
    /// this to translate wheel motion into arrow keys on the alternate screen.
    pub fn is_alternate_scroll_enabled(&self) -> bool {
        self.modes.alternate_scroll
    }

    /// Whether autowrap (DECAWM `?7`) is active — `print` reads this to decide
    /// whether a glyph at the effective right bound wraps onto a new line.
    /// Default on.
    pub fn is_autowrap_enabled(&self) -> bool {
        self.modes.autowrap
    }

    /// Whether application-cursor-keys mode (DECCKM `?1`) is active — the input
    /// layer reads this to pick the arrow-key byte form.
    pub fn are_application_cursor_keys_enabled(&self) -> bool {
        self.modes.application_cursor_keys
    }

    /// Whether reverse-video mode (DECSCNM `?5`) is active — the renderer reads
    /// this to swap foreground and background across the screen.
    pub fn is_reverse_video_enabled(&self) -> bool {
        self.modes.reverse_video
    }

    /// Whether cursor-blink mode is active — the renderer reads this to blink
    /// the cursor cell. Set by `?12` (att610) and by DECSCUSR, whose style
    /// value says both shape and blink; the last of the two to arrive wins.
    pub fn is_cursor_blink_enabled(&self) -> bool {
        self.modes.cursor_blink
    }

    /// The shape the cursor is drawn as (DECSCUSR), or `None` while the pane has
    /// asked for no shape — the renderer reads this to pick the outer terminal's
    /// cursor style: vim's insert-mode bar shows as a bar, and a pane that never
    /// asked leaves the user's own cursor alone.
    pub fn get_cursor_shape(&self) -> Option<CursorShape> {
        self.modes.cursor_shape
    }

    /// The pane's scrollback history. A snapshot reads its truncation tally as
    /// `ScrollbackMeta::truncated`, and the renderer reads its rows to compose
    /// a scrolled-back view.
    pub fn get_scrollback(&self) -> &Scrollback {
        &self.scrollback
    }

    /// Drain the queued shell-integration facts, leaving the queue empty.
    pub(crate) fn take_shell_integration_facts(&mut self) -> Vec<ShellIntegrationFact> {
        std::mem::take(&mut self.shell_integration_facts)
    }

    /// Drain the queued device-query replies (DA/DSR/DECRQM answers), leaving
    /// the queue empty. The caller writes the returned bytes back into the
    /// pane's PTY.
    #[must_use = "undelivered replies hang the querying app"]
    pub(crate) fn take_device_query_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.device_query_replies)
    }

    /// The scroll region (top and bottom margins) for the active screen, or
    /// `None` if scrolling uses the full height. Margins are zero-based and
    /// inclusive.
    pub fn get_scroll_region(&self) -> Option<(u16, u16)> {
        match self.active_screen {
            Screen::Primary => self.primary_scroll_region,
            Screen::Alternate => self.alternate_scroll_region,
        }
    }

    /// Mutable access to the scroll region for the active screen.
    pub(crate) fn scroll_region_mut(&mut self) -> &mut Option<(u16, u16)> {
        match self.active_screen {
            Screen::Primary => &mut self.primary_scroll_region,
            Screen::Alternate => &mut self.alternate_scroll_region,
        }
    }

    /// The horizontal margins `(left, right)` for the active screen, or `None`
    /// when horizontal operations use the full width.
    pub fn get_horizontal_margins(&self) -> Option<(u16, u16)> {
        match self.active_screen {
            Screen::Primary => self.primary_horizontal_margins,
            Screen::Alternate => self.alternate_horizontal_margins,
        }
    }

    /// Mutable access to the horizontal margins for the active screen.
    pub(crate) fn horizontal_margins_mut(&mut self) -> &mut Option<(u16, u16)> {
        match self.active_screen {
            Screen::Primary => &mut self.primary_horizontal_margins,
            Screen::Alternate => &mut self.alternate_horizontal_margins,
        }
    }

    /// The Kitty keyboard flags in effect on the active screen: the top entry
    /// of that screen's stack, or `0` when the stack is empty.
    pub(crate) fn get_keyboard_flags(&self) -> u8 {
        match self.active_screen {
            Screen::Primary => self.primary_keyboard_stack.get_current_flags(),
            Screen::Alternate => self.alternate_keyboard_stack.get_current_flags(),
        }
    }

    /// Mutable access to the Kitty keyboard flag stack for the active screen.
    pub(crate) fn active_keyboard_stack_mut(&mut self) -> &mut KeyboardStack {
        match self.active_screen {
            Screen::Primary => &mut self.primary_keyboard_stack,
            Screen::Alternate => &mut self.alternate_keyboard_stack,
        }
    }

    /// The cursor position `(row, column)` on the active screen, both zero-based.
    pub fn get_active_cursor_position(&self) -> (u16, u16) {
        (self.active_cursor().row, self.active_cursor().column)
    }

    /// The cursor for the active screen.
    fn active_cursor(&self) -> &Cursor {
        match self.active_screen {
            Screen::Primary => &self.primary_cursor,
            Screen::Alternate => &self.alternate_cursor,
        }
    }

    /// Mutable access to the cursor for the active screen.
    fn active_cursor_mut(&mut self) -> &mut Cursor {
        match self.active_screen {
            Screen::Primary => &mut self.primary_cursor,
            Screen::Alternate => &mut self.alternate_cursor,
        }
    }

    /// The render state (pen, charsets, GL slot) for the active screen.
    fn active_render(&self) -> &RenderState {
        match self.active_screen {
            Screen::Primary => &self.primary_render,
            Screen::Alternate => &self.alternate_render,
        }
    }

    /// Mutable access to the render state for the active screen.
    fn active_render_mut(&mut self) -> &mut RenderState {
        match self.active_screen {
            Screen::Primary => &mut self.primary_render,
            Screen::Alternate => &mut self.alternate_render,
        }
    }
}

/// Build the default tab stops at columns 0, 8, 16, and every eighth column.
fn build_default_tab_stops(column_count: u16) -> Vec<bool> {
    (0..column_count)
        .map(|column_index| column_index % 8 == 0)
        .collect()
}

/// `terminal_cell` rebuilt with display `cell_width`, keeping its character, combining
/// marks, and style. `Cell::from_character('가', 2, style)` with a `~` combining mark
/// re-widthed to 1 gives the same character, mark, and style in one column.
fn rebuild_cell_with_width(terminal_cell: &Cell, cell_width: u8) -> Cell {
    let mut rebuilt_cell = Cell::from_character(
        terminal_cell.get_character(),
        cell_width,
        terminal_cell.get_style(),
    );
    for combining_mark in terminal_cell.list_combining_characters() {
        rebuilt_cell.push_combining(*combining_mark);
    }
    rebuilt_cell
}

/// Normalize `row_cells` to exactly `column_count` cells: truncate on the right or pad with
/// blanks in `fill`. A wide glyph whose right (width-0) half falls past the new
/// edge leaves its base as the last cell; that dangling base is blanked.
fn crop_columns(row_cells: &mut Vec<Cell>, column_count: u16, fill_style: Style) {
    row_cells.resize(column_count as usize, Cell::blank_with(fill_style));
    if let Some(trailing_cell) = row_cells.last_mut() {
        if trailing_cell.get_display_width() > 1 {
            *trailing_cell = Cell::blank_with(fill_style);
        }
    }
}

#[cfg(test)]
mod tests;
