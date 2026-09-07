//! Per-pane terminal state: screen buffers, cursor, pen style (the
//! foreground/background color and attributes applied to newly written
//! text), modes, horizontal tab stops, title, reported working directory,
//! shell integration state and facts, image placements, prompt-row marks,
//! scrollback, and the device-reply queue.
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
//! [`MouseTracking`]/[`MouseEncoding`] levels, and the [`ReportedCwd`]. The
//! ones a caller outside this crate can name are re-exported here, reachable
//! as `koshi_terminal::state::*`.

use std::cmp::min;
use std::collections::HashMap;
use std::sync::Arc;

use koshi_core::process::PtySize;
use koshi_sixel::{SixelGraphic, SixelPalette};

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Serialize};

use crate::grid::state::{Cell, Grid, RowMeta};
use crate::scrollback::{Scrollback, ScrollbackLimit};
use crate::selection::TextView;
use crate::style::Style;

mod cursor;
mod cwd;
pub(crate) mod images;
mod modes;
mod perform;
mod reflow;
mod render;
mod screen;

pub(crate) use cursor::{Cursor, SavedCursor};
pub use cwd::ReportedCwd;
pub(crate) use images::SixelImageSource;
pub use images::{ImagePlacement, ImagePlacementError, ImagePlacementId};
pub(crate) use modes::TerminalModes;
pub use modes::{CursorShape, MouseEncoding, MouseTracking};
pub(crate) use render::{Charset, RenderState};
pub use screen::Screen;

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
    active: Screen,
    /// The cursor for the primary screen, holding its own position, visibility,
    /// wrap latch, and saved snapshot.
    primary_cursor: Cursor,
    /// The cursor for the alternate screen, independent of the primary cursor:
    /// position and wrap state do not carry across screen switches.
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
    native_fragment_counts: HashMap<u64, usize>,
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
    /// decoded path), or `None` until the shell reports one. Read by cwd
    /// inheritance when a new pane spawns.
    reported_cwd: Option<ReportedCwd>,
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
    /// The grapheme cluster currently being built at the cursor — the run of
    /// printed code points that fold into one cell (a base plus its combining
    /// marks and any emoji continuation: ZWJ-joined parts, variation selectors,
    /// skin-tone modifiers, regional-indicator flags). Empty when no run is
    /// active; any non-printing event resets it.
    cluster: String,
    /// The `(row, col)` of the cell holding `cluster`'s base, or `None` when no
    /// run is active. Continuations attach here and width promotion widens it.
    cluster_base: Option<(u16, u16)>,
    /// Bytes queued for the running app in answer to its device queries
    /// (DA/DSR/DECRQM). The performer appends replies here; the runtime drains
    /// them via `take_replies` and writes them back into the pane's PTY.
    /// Device-global: one queue regardless of the active screen.
    replies: Vec<u8>,
}

#[derive(Serialize)]
struct TerminalStateSerializeFields<'a> {
    native_image_coverage: bool,
    cell_size: Option<koshi_core::geometry::PixelCellSize>,
    primary: &'a Arc<Grid>,
    alternate: &'a Arc<Grid>,
    active: Screen,
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
    reported_cwd: &'a Option<ReportedCwd>,
    shell_integration_state: ShellIntegrationState,
    shell_integration_facts: &'a Vec<ShellIntegrationFact>,
    scrollback: &'a Scrollback,
    primary_scroll_region: &'a Option<(u16, u16)>,
    alternate_scroll_region: &'a Option<(u16, u16)>,
    cluster: &'a String,
    cluster_base: &'a Option<(u16, u16)>,
    replies: &'a Vec<u8>,
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
            .map(images::serialized_image_placement)
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::ser::Error::custom)?;
        let primary_image_history = self
            .primary_image_history
            .iter()
            .map(images::serialized_history_placement)
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
            .map(images::serialized_image_placement)
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::ser::Error::custom)?;
        let kitty_images = self
            .kitty_images
            .iter()
            .map(images::serialized_kitty_image)
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::ser::Error::custom)?;
        let image_contents =
            images::serialized_content_table(self).map_err(serde::ser::Error::custom)?;
        TerminalStateSerializeFields {
            native_image_coverage: true,
            cell_size: self.cell_size,
            primary: &self.primary,
            alternate: &self.alternate,
            active: self.active,
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
            reported_cwd: &self.reported_cwd,
            shell_integration_state: self.shell_integration_state,
            shell_integration_facts: &self.shell_integration_facts,
            scrollback: &self.scrollback,
            primary_scroll_region: &self.primary_scroll_region,
            alternate_scroll_region: &self.alternate_scroll_region,
            cluster: &self.cluster,
            cluster_base: &self.cluster_base,
            replies: &self.replies,
        }
        .serialize(serializer)
    }
}

struct TerminalStateFields {
    native_image_coverage: bool,
    cell_size: Option<koshi_core::geometry::PixelCellSize>,
    primary: Arc<Grid>,
    alternate: Arc<Grid>,
    active: Screen,
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
    reported_cwd: Option<ReportedCwd>,
    shell_integration_state: ShellIntegrationState,
    shell_integration_facts: Vec<ShellIntegrationFact>,
    scrollback: Scrollback,
    primary_scroll_region: Option<(u16, u16)>,
    alternate_scroll_region: Option<(u16, u16)>,
    cluster: String,
    cluster_base: Option<(u16, u16)>,
    replies: Vec<u8>,
}

struct RawTerminalStateFields {
    native_image_coverage: bool,
    cell_size: Option<koshi_core::geometry::PixelCellSize>,
    primary: Arc<Grid>,
    alternate: Arc<Grid>,
    active: Screen,
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
    reported_cwd: Option<ReportedCwd>,
    shell_integration_state: ShellIntegrationState,
    shell_integration_facts: Vec<ShellIntegrationFact>,
    scrollback: Scrollback,
    primary_scroll_region: Option<(u16, u16)>,
    alternate_scroll_region: Option<(u16, u16)>,
    cluster: String,
    cluster_base: Option<(u16, u16)>,
    replies: Vec<u8>,
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
    ReportedCwd,
    ShellIntegrationState,
    ShellIntegrationFacts,
    Scrollback,
    PrimaryScrollRegion,
    AlternateScrollRegion,
    Cluster,
    ClusterBase,
    Replies,
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

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut native_image_coverage = None;
        let mut cell_size = None;
        let mut primary = None;
        let mut alternate = None;
        let mut active = None;
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
        let mut reported_cwd = None;
        let mut shell_integration_state = None;
        let mut shell_integration_facts = None;
        let mut scrollback = None;
        let mut primary_scroll_region = None;
        let mut alternate_scroll_region = None;
        let mut cluster = None;
        let mut cluster_base = None;
        let mut replies = None;

        while let Some(field) = map.next_key::<RawTerminalStateField>()? {
            match field {
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
                    if primary.is_some() {
                        return Err(serde::de::Error::duplicate_field("primary"));
                    }
                    primary = Some(map.next_value()?);
                }
                RawTerminalStateField::Alternate => {
                    if alternate.is_some() {
                        return Err(serde::de::Error::duplicate_field("alternate"));
                    }
                    alternate = Some(map.next_value()?);
                }
                RawTerminalStateField::Active => {
                    if active.is_some() {
                        return Err(serde::de::Error::duplicate_field("active"));
                    }
                    active = Some(map.next_value()?);
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
                        Some(map.next_value_seed(images::SerializedPlacementsSeed {
                            budget: self.budget,
                        })?);
                }
                RawTerminalStateField::PrimaryImageHistory => {
                    if primary_image_history.is_some() {
                        return Err(serde::de::Error::duplicate_field("primary_image_history"));
                    }
                    primary_image_history =
                        Some(map.next_value_seed(images::SerializedPlacementsSeed {
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
                        Some(map.next_value_seed(images::SerializedPlacementsSeed {
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
                RawTerminalStateField::ReportedCwd => {
                    if reported_cwd.is_some() {
                        return Err(serde::de::Error::duplicate_field("reported_cwd"));
                    }
                    reported_cwd = Some(map.next_value()?);
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
                RawTerminalStateField::Replies => {
                    if replies.is_some() {
                        return Err(serde::de::Error::duplicate_field("replies"));
                    }
                    replies = Some(map.next_value()?);
                }
                RawTerminalStateField::Other => {
                    let _: serde::de::IgnoredAny = map.next_value()?;
                }
            }
        }

        Ok(RawTerminalStateFields {
            native_image_coverage: native_image_coverage.unwrap_or(false),
            cell_size: cell_size.unwrap_or_default(),
            primary: primary.ok_or_else(|| serde::de::Error::missing_field("primary"))?,
            alternate: alternate.ok_or_else(|| serde::de::Error::missing_field("alternate"))?,
            active: active.ok_or_else(|| serde::de::Error::missing_field("active"))?,
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
            reported_cwd: reported_cwd
                .ok_or_else(|| serde::de::Error::missing_field("reported_cwd"))?,
            shell_integration_state: shell_integration_state.unwrap_or_default(),
            shell_integration_facts: shell_integration_facts.unwrap_or_default(),
            scrollback: scrollback.ok_or_else(|| serde::de::Error::missing_field("scrollback"))?,
            primary_scroll_region: primary_scroll_region
                .ok_or_else(|| serde::de::Error::missing_field("primary_scroll_region"))?,
            alternate_scroll_region: alternate_scroll_region
                .ok_or_else(|| serde::de::Error::missing_field("alternate_scroll_region"))?,
            cluster: cluster.ok_or_else(|| serde::de::Error::missing_field("cluster"))?,
            cluster_base: cluster_base
                .ok_or_else(|| serde::de::Error::missing_field("cluster_base"))?,
            replies: replies.ok_or_else(|| serde::de::Error::missing_field("replies"))?,
        })
    }
}

impl<'de> Deserialize<'de> for RawTerminalStateFields {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut budget = images::ImageStateBudget::new();
        Self::deserialize_with_budget(deserializer, &mut budget)
    }
}

impl RawTerminalStateFields {
    fn deserialize_with_budget<'de, D>(
        deserializer: D,
        budget: &mut images::ImageStateBudget,
    ) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(RawTerminalStateVisitor { budget })
    }
}

impl<'de> Deserialize<'de> for TerminalStateFields {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawTerminalStateFields::deserialize(deserializer)?;
        let images = images::restore_serialized_image_state(
            raw.primary_image_placements,
            raw.primary_image_history,
            raw.alternate_image_placements,
            raw.kitty_images,
            raw.image_contents,
            raw.next_image_content_id,
        )
        .map_err(serde::de::Error::custom)?;
        Ok(Self {
            native_image_coverage: raw.native_image_coverage,
            cell_size: raw.cell_size,
            primary: raw.primary,
            alternate: raw.alternate,
            active: raw.active,
            primary_cursor: raw.primary_cursor,
            alternate_cursor: raw.alternate_cursor,
            primary_render: raw.primary_render,
            alternate_render: raw.alternate_render,
            primary_image_placements: images.primary_image_placements,
            primary_image_history: images.primary_image_history,
            alternate_image_placements: images.alternate_image_placements,
            kitty_images: images.kitty_images,
            next_image_placement_id: raw.next_image_placement_id,
            next_image_content_id: images.next_image_content_id,
            modes: raw.modes,
            sixel_palette: raw.sixel_palette,
            tab_stops: raw.tab_stops,
            title: raw.title,
            reported_cwd: raw.reported_cwd,
            shell_integration_state: raw.shell_integration_state,
            shell_integration_facts: raw.shell_integration_facts,
            scrollback: raw.scrollback,
            primary_scroll_region: raw.primary_scroll_region,
            alternate_scroll_region: raw.alternate_scroll_region,
            cluster: raw.cluster,
            cluster_base: raw.cluster_base,
            replies: raw.replies,
        })
    }
}

impl<'de> Deserialize<'de> for TerminalState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let fields = TerminalStateFields::deserialize(deserializer)?;
        images::validate_image_state(&fields).map_err(serde::de::Error::custom)?;

        let native_image_coverage = fields.native_image_coverage;
        let mut state = TerminalState {
            native_images: Vec::new(),
            native_fragment_counts: HashMap::new(),
            cell_size: fields.cell_size,
            primary: fields.primary,
            alternate: fields.alternate,
            active: fields.active,
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
            reported_cwd: fields.reported_cwd,
            shell_integration_state: fields.shell_integration_state,
            shell_integration_facts: fields.shell_integration_facts,
            scrollback: fields.scrollback,
            primary_scroll_region: fields.primary_scroll_region,
            alternate_scroll_region: fields.alternate_scroll_region,
            cluster: fields.cluster,
            cluster_base: fields.cluster_base,
            replies: fields.replies,
        };
        state
            .restore_native_image_coverage(native_image_coverage)
            .map_err(serde::de::Error::custom)?;
        Ok(state)
    }
}

impl TerminalState {
    /// Set the shared pixel measurement used by new images and size reports.
    pub fn set_cell_size(&mut self, size: koshi_core::geometry::PixelCellSize) {
        self.cell_size = Some(size);
    }

    /// Return the shared terminal cell measurement in pixels.
    #[must_use]
    pub fn cell_size(&self) -> Option<koshi_core::geometry::PixelCellSize> {
        self.cell_size
    }

    pub(crate) fn apply_sixel_graphic(
        &mut self,
        graphic: SixelGraphic,
        anchor: (u16, u16),
    ) -> Result<Option<crate::graphics::ImageRecord>, crate::graphics::GraphicsError> {
        let mut palette = if self.modes.sixel_private_color_registers {
            SixelPalette::default()
        } else {
            self.sixel_palette.clone()
        };
        palette.apply_changes(graphic.palette_changes());
        if !self.modes.sixel_private_color_registers {
            self.sixel_palette = palette.clone();
        }

        let shared_palette = !self.modes.sixel_private_color_registers;
        if shared_palette && !graphic.palette_changes().entries().is_empty() {
            self.refresh_shared_sixel_images(&palette)?;
        }

        let Some(indexed) = graphic.image() else {
            return Ok(None);
        };
        let source = SixelImageSource::new(indexed.clone(), palette.clone(), shared_palette);
        let image = self.shared_image_pixels(source.resolved(&palette)?);
        let scrolling = self.modes.sixel_scrolling;
        let record = crate::graphics::ImageRecord {
            protocol: crate::graphics::GraphicsProtocol::Sixel,
            image,
            animation: None,
            action: crate::graphics::ImageAction::Display,
            display: crate::graphics::ImageDisplay {
                move_cursor: scrolling,
                sixel_background: Some(graphic.background()),
                ..crate::graphics::ImageDisplay::default()
            },
            anchor: if scrolling { anchor } else { (0, 0) },
        };
        self.apply_sixel_image_record(&record, scrolling, self.modes.sixel_cursor_right, source)
            .map_err(|reason| crate::graphics::GraphicsError::PlacementRejected {
                protocol: crate::graphics::GraphicsProtocol::Sixel,
                reason,
            })?;
        Ok(Some(record))
    }

    /// Create per-pane state for a terminal of `size`: both screen buffers
    /// blank, the cursor at the top-left and visible, default pen, no title.
    pub fn new(size: PtySize) -> Self {
        Self::with_scrollback(size, ScrollbackLimit::default())
    }

    /// Like [`new`](Self::new), but with an explicit scrollback limit.
    pub fn with_scrollback(size: PtySize, limit: ScrollbackLimit) -> Self {
        let blank_screen = Grid::blank(size.rows, size.cols, Style::default());
        let home_cursor = Cursor {
            row: 0,
            col: 0,
            is_visible: true,
            pending_wrap: false,
            saved: None,
        };
        TerminalState {
            cell_size: None,
            primary: Arc::new(blank_screen.clone()),
            alternate: Arc::new(blank_screen),
            active: Screen::Primary,
            primary_cursor: home_cursor,
            alternate_cursor: home_cursor,
            primary_render: RenderState::fresh(),
            alternate_render: RenderState::fresh(),
            native_images: Vec::new(),
            native_fragment_counts: HashMap::new(),
            primary_image_placements: Vec::new(),
            primary_image_history: Vec::new(),
            alternate_image_placements: Vec::new(),
            kitty_images: Vec::new(),
            next_image_placement_id: images::default_next_image_placement_id(),
            next_image_content_id: images::default_next_image_content_id(),
            modes: TerminalModes::default(),
            sixel_palette: SixelPalette::default(),
            tab_stops: default_tab_stops(size.cols),
            title: None,
            reported_cwd: None,
            shell_integration_state: ShellIntegrationState::default(),
            shell_integration_facts: Vec::new(),
            scrollback: Scrollback::new(limit),
            primary_scroll_region: None,
            alternate_scroll_region: None,
            cluster: String::new(),
            cluster_base: None,
            replies: Vec::new(),
        }
    }

    /// Resize the tab-stop table while keeping stops in surviving columns.
    fn resize_tab_stops(&mut self, columns: u16) {
        let kept = self.tab_stops.len().min(columns as usize);
        self.tab_stops.truncate(columns as usize);
        self.tab_stops
            .extend((kept..columns as usize).map(|column| column % 8 == 0));
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
    /// screens' scroll margins are dropped until the app issues DECSTBM again.
    /// Primary image anchors follow their row's reflowed text and remain in the
    /// primary history list while any of their cells stay addressable.
    /// Alternate placements follow the cropped rows and retain their image scale.
    /// Both cursors are clamped into the new bounds with their wrap latch
    /// cleared, and an in-progress grapheme cluster is dropped.
    pub fn resize(&mut self, size: PtySize) {
        let alternate_fill = self.alternate_render.style.bg_fill();

        self.resize_tab_stops(size.cols);
        self.reflow_primary(size);

        // The alternate screen keeps what fits: crop off the top, pad at the
        // bottom, no history on either side. Row metadata follows each row.
        let mut rows: Vec<(Vec<Cell>, RowMeta)> = self
            .alternate
            .rows()
            .iter()
            .enumerate()
            .map(|(row, cells)| (cells.clone(), self.alternate.row_meta(row as u16)))
            .collect();
        for (cells, _) in &mut rows {
            crop_columns(cells, size.cols, alternate_fill);
        }
        let cropped_top = rows.len().saturating_sub(size.rows as usize);
        rows.drain(..cropped_top);
        self.alternate_cursor.row = self
            .alternate_cursor
            .row
            .saturating_sub(u16::try_from(cropped_top).unwrap_or(u16::MAX));
        rows.resize(
            size.rows as usize,
            (
                vec![Cell::blank_with(alternate_fill); size.cols as usize],
                RowMeta::default(),
            ),
        );
        self.alternate = Arc::new(Grid::from_rows_with_meta(rows, size.cols, alternate_fill));

        self.remap_alternate_image_placements(|row, column| {
            Some((row.checked_sub(u16::try_from(cropped_top).ok()?)?, column))
        });
        self.rebuild_native_fragment_counts();

        // Clamp both cursors to the new bounds.
        self.primary_cursor.row = min(self.primary_cursor.row, size.rows.saturating_sub(1));
        self.primary_cursor.col = min(self.primary_cursor.col, size.cols.saturating_sub(1));
        self.primary_cursor.pending_wrap = false;

        self.alternate_cursor.row = min(self.alternate_cursor.row, size.rows.saturating_sub(1));
        self.alternate_cursor.col = min(self.alternate_cursor.col, size.cols.saturating_sub(1));
        self.alternate_cursor.pending_wrap = false;

        // Both scroll regions are dropped: the resized screen scrolls in full
        // until the app issues DECSTBM again.
        self.primary_scroll_region = None;
        self.alternate_scroll_region = None;

        // An in-progress cluster is dropped: its recorded base position indexes
        // the old geometry.
        self.cluster.clear();
        self.cluster_base = None;
    }

    /// Which screen (primary or alternate) is currently displayed and written to.
    pub fn active_screen(&self) -> Screen {
        self.active
    }

    /// Whether the primary screen — the one that keeps scrollback history — is
    /// the active one. `false` while a full-screen program holds the alternate
    /// screen, which keeps no history.
    pub fn on_primary_screen(&self) -> bool {
        matches!(self.active, Screen::Primary)
    }

    /// The screen buffer currently displayed and written to — `primary` or
    /// `alternate`, per the active screen.
    pub fn active_grid(&self) -> &Grid {
        match self.active {
            Screen::Primary => self.primary.as_ref(),
            Screen::Alternate => self.alternate.as_ref(),
        }
    }

    /// Mutable access to the active screen buffer, for writing cells. Clones the
    /// buffer once (copy-on-write) if a render snapshot still shares it; the
    /// snapshot keeps the pre-write contents.
    pub(crate) fn active_grid_mut(&mut self) -> &mut Grid {
        match self.active {
            Screen::Primary => Arc::make_mut(&mut self.primary),
            Screen::Alternate => Arc::make_mut(&mut self.alternate),
        }
    }

    /// A reference-counted handle to the active screen buffer for the render
    /// snapshot: clones the `Arc`, not the grid. The next write to this screen
    /// clones the buffer once, leaving this handle pointing at the frozen
    /// contents.
    pub fn active_grid_arc(&self) -> Arc<Grid> {
        match self.active {
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
    pub fn text_view(&self) -> TextView<'_> {
        match self.active {
            Screen::Primary => TextView::new(&self.scrollback, self.active_grid()),
            Screen::Alternate => {
                TextView::screen_only(self.active_grid(), self.scrollback.total_pushed())
            }
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
        if !self.on_primary_screen() {
            return 0;
        }
        offset.min(self.scrollback.len())
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
            return (self.active_grid_arc(), 0);
        }

        let grid = self.primary.as_ref();
        let (rows, cols) = grid.dimensions();
        let history = self.scrollback.lines();
        let retained = history.len();

        // The visible window: the `scrolled` newest history rows, then the live
        // rows, capped at the screen height. The live grid alone is `rows` tall;
        // the chain always yields a full window and keeps row metadata.
        let window: Vec<(Vec<Cell>, RowMeta)> = history
            .iter()
            .skip(retained - scrolled)
            .map(|(cells, meta)| (cells.clone(), *meta))
            .chain(
                grid.rows()
                    .iter()
                    .enumerate()
                    .map(|(row, cells)| (cells.clone(), grid.row_meta(row as u16))),
            )
            .take(rows as usize)
            .collect();
        (
            Arc::new(Grid::from_rows_with_meta(window, cols, Style::default())),
            scrolled,
        )
    }

    /// The window/tab title set by OSC 0/1/2, or `None` if the app has not set
    /// one.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// The working directory last reported by the shell via OSC 7 (its host and
    /// decoded path), or `None` if none has been reported. The pane-spawn layer
    /// compares the host to the local machine before inheriting the path; a
    /// directory reported from a remote host (over SSH) is not opened locally.
    pub fn current_cwd(&self) -> Option<&ReportedCwd> {
        self.reported_cwd.as_ref()
    }

    /// Whether the cursor should be drawn — toggled by DECTCEM (`?25`).
    pub fn cursor_visible(&self) -> bool {
        self.active_cursor().is_visible
    }

    /// Whether bracketed-paste mode (`?2004`) is active — the input layer reads
    /// this to decide whether to bracket a paste in `ESC[200~`…`ESC[201~`.
    pub fn bracketed_paste(&self) -> bool {
        self.modes.bracketed_paste
    }

    /// The active mouse tracking level (`?9`/`?1000`/`?1002`/`?1003`) — the
    /// mouse layer reads this to decide which events to report to the app.
    pub fn mouse_tracking(&self) -> MouseTracking {
        self.modes.mouse_tracking
    }

    /// The active mouse report encoding (`?1005`/`?1006`/`?1015`) — the mouse
    /// layer reads this to format the coordinates of a report.
    pub fn mouse_encoding(&self) -> MouseEncoding {
        self.modes.mouse_encoding
    }

    /// Whether alternate-scroll mode (`?1007`) is active — the mouse layer reads
    /// this to translate wheel motion into arrow keys on the alternate screen.
    pub fn alt_scroll(&self) -> bool {
        self.modes.alt_scroll
    }

    /// Whether autowrap (DECAWM `?7`) is active — `print` reads this to decide
    /// whether a glyph at the last column wraps onto a new line. Default on.
    pub fn autowrap(&self) -> bool {
        self.modes.autowrap
    }

    /// Whether application-cursor-keys mode (DECCKM `?1`) is active — the input
    /// layer reads this to pick the arrow-key byte form.
    pub fn app_cursor_keys(&self) -> bool {
        self.modes.app_cursor_keys
    }

    /// Whether reverse-video mode (DECSCNM `?5`) is active — the renderer reads
    /// this to swap foreground and background across the screen.
    pub fn reverse_video(&self) -> bool {
        self.modes.reverse_video
    }

    /// Whether cursor-blink mode is active — the renderer reads this to blink
    /// the cursor cell. Set by `?12` (att610) and by DECSCUSR, whose style
    /// value says both shape and blink; the last of the two to arrive wins.
    pub fn cursor_blink(&self) -> bool {
        self.modes.cursor_blink
    }

    /// The shape the cursor is drawn as (DECSCUSR), or `None` while the pane has
    /// asked for no shape — the renderer reads this to pick the outer terminal's
    /// cursor style: vim's insert-mode bar shows as a bar, and a pane that never
    /// asked leaves the user's own cursor alone.
    pub fn cursor_shape(&self) -> Option<CursorShape> {
        self.modes.cursor_shape
    }

    /// The pane's scrollback history. A snapshot reads its truncation tally as
    /// `ScrollbackMeta::truncated`, and the renderer reads its rows to compose
    /// a scrolled-back view.
    pub fn scrollback(&self) -> &Scrollback {
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
    pub(crate) fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    /// The scroll region (top and bottom margins) for the active screen, or
    /// `None` if scrolling uses the full height. Margins are zero-based and
    /// inclusive.
    pub fn scroll_region(&self) -> Option<(u16, u16)> {
        match self.active {
            Screen::Primary => self.primary_scroll_region,
            Screen::Alternate => self.alternate_scroll_region,
        }
    }

    /// Mutable access to the scroll region for the active screen.
    pub(crate) fn scroll_region_mut(&mut self) -> &mut Option<(u16, u16)> {
        match self.active {
            Screen::Primary => &mut self.primary_scroll_region,
            Screen::Alternate => &mut self.alternate_scroll_region,
        }
    }

    /// The cursor position `(row, col)` on the active screen, both zero-based.
    pub fn active_cursor_position(&self) -> (u16, u16) {
        (self.active_cursor().row, self.active_cursor().col)
    }

    /// The cursor for the active screen.
    fn active_cursor(&self) -> &Cursor {
        match self.active {
            Screen::Primary => &self.primary_cursor,
            Screen::Alternate => &self.alternate_cursor,
        }
    }

    /// Mutable access to the cursor for the active screen.
    fn active_cursor_mut(&mut self) -> &mut Cursor {
        match self.active {
            Screen::Primary => &mut self.primary_cursor,
            Screen::Alternate => &mut self.alternate_cursor,
        }
    }

    /// The render state (pen, charsets, GL slot) for the active screen.
    fn active_render(&self) -> &RenderState {
        match self.active {
            Screen::Primary => &self.primary_render,
            Screen::Alternate => &self.alternate_render,
        }
    }

    /// Mutable access to the render state for the active screen.
    fn active_render_mut(&mut self) -> &mut RenderState {
        match self.active {
            Screen::Primary => &mut self.primary_render,
            Screen::Alternate => &mut self.alternate_render,
        }
    }
}

/// Build the default tab stops at columns 0, 8, 16, and every eighth column.
fn default_tab_stops(columns: u16) -> Vec<bool> {
    (0..columns).map(|column| column % 8 == 0).collect()
}

/// `cell` rebuilt with display `width`, keeping its character, combining
/// marks, and style. `Cell::new('가', 2, style)` with a `~` combining mark
/// re-widthed to 1 gives the same character, mark, and style in one column.
fn rebuilt_with_width(cell: &Cell, width: u8) -> Cell {
    let mut out = Cell::new(cell.ch(), width, cell.style());
    for mark in cell.combining() {
        out.push_combining(*mark);
    }
    out
}

/// Normalize `row` to exactly `cols` cells: truncate on the right or pad with
/// blanks in `fill`. A wide glyph whose right (width-0) half falls past the new
/// edge leaves its base as the last cell; that dangling base is blanked.
fn crop_columns(row: &mut Vec<Cell>, cols: u16, fill: Style) {
    row.resize(cols as usize, Cell::blank_with(fill));
    if let Some(last) = row.last_mut() {
        if last.width() > 1 {
            *last = Cell::blank_with(fill);
        }
    }
}

#[cfg(test)]
mod tests;
