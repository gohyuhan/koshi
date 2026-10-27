//! Parser for `koshi.kdl`, the app-settings config file.
//!
//! Turns the top-level sections of `koshi.kdl` into a [`PartialKoshiConfig`]
//! override layer that folds onto the built-in defaults. Does no file I/O: the
//! caller reads the file and hands the text in.
//!
//! # Field-partial, except `update`
//!
//! Every section but `update` is **field-partial**: a field whose value is the
//! wrong kind is skipped, its default stands, and every other field in the
//! file still applies. Each skipped field is named in the returned warnings.
//!
//! The `update` section is **strict**: a field there whose value is the wrong
//! kind refuses the whole file. A refused file gives every problem it has,
//! in file order: the refusals and the skipped fields alike.
//!
//! # The `theme` line
//!
//! `theme "midnight"` names which color theme to use; the colors live in a
//! separate `themes/midnight.kdl`. This parser records only the name and
//! returns it beside the layer, not inside it. See [`AppConfigFile`].
//!
//! # Example
//! A `koshi.kdl` of
//! ```kdl
//! version 2
//! theme "midnight"
//! scrollback {
//!     max-lines 50000
//! }
//! layout {
//!     new-pane-direction "down"
//! }
//! ```
//! yields `theme = Some("midnight")` and a layer setting
//! `scrollback.maximum_line_count = 50000` and the default new-pane direction to
//! [`Direction::Down`], leaving every other field at its built-in default.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use kdl::{KdlDocument, KdlNode};
use koshi_core::constant::{
    HIGHEST_MAX_FRAMES_PER_SECOND, LOWEST_MAX_FRAMES_PER_SECOND, MIN_LINK_TIMEOUT_MS,
};
use koshi_core::geometry::Direction;
use koshi_core::key::ExtendedKeysMode;
use koshi_core::log::{LogFormat, LogLevel};

use crate::error::ConfigError;
use crate::layer::{
    PartialCopyConfig, PartialKoshiConfig, PartialLayoutDefaults, PartialLoggingConfig,
    PartialMouseConfig, PartialNotificationConfig, PartialPaneClipboardWriteConfig,
    PartialPaneConfig, PartialScrollbackConfig, PartialTerminalConfig, PartialUpdateConfig,
};
use crate::parser::{
    find_newer_version_error, find_section_block, format_unknown_key, parse_boolean_kdl_value,
    parse_integer_kdl_value, parse_kdl, parse_nonempty_string_kdl_value,
    parse_ranged_integer_kdl_value, parse_string_kdl_value, parse_supported_schema_version,
    set_parsed_field, ParseProblems,
};
use crate::types::{
    ToastPosition, WheelScroll, HIGHEST_MAX_LOG_FILE_SIZE_IN_MB, HIGHEST_TOAST_DURATION_MS,
    LOWEST_MAX_LOG_FILE_SIZE_IN_MB, LOWEST_TOAST_DURATION_MS, SCHEMA_VERSION,
};

/// The top-level node names. Each may appear at most once; an unknown name is
/// matched against these for the `did you mean` hint.
const APP_CONFIG_SECTION_NAMES: &[&str] = &[
    "version",
    "update",
    "theme",
    "pane",
    "scrollback",
    "layout",
    "mouse",
    "copy",
    "notifications",
    "terminal",
    "logging",
    "image-support",
    "reduced-motion",
    "stay-in-pane-placement-mode-after-placement",
    "remote-reconnect",
    "remote-link-timeout-ms",
    "max-frames-per-second",
    "allow-beta-features",
    "allow-other-users",
    "remote-listen",
    "shared-sessions-dir",
    "auto-close-session",
    "pane-clipboard-writes",
];

/// A parsed `koshi.kdl`.
///
/// The theme name is kept **out** of [`layer`](Self::layer): `layer.theme` is
/// always `None`, and the name from the `theme` line is in
/// [`theme_name`](Self::theme_name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfigFile {
    /// The schema version the `version` line declares, from 1 through
    /// [`SCHEMA_VERSION`]: `version 1` gives `1`.
    pub schema_version: u32,
    /// The settings this file overrides, to fold onto the built-in defaults.
    pub layer: PartialKoshiConfig,
    /// The name from the `theme "<name>"` line, trimmed of surrounding
    /// whitespace; `themes/<name>.kdl` supplies the colors. `None` when the
    /// file names no theme.
    pub theme_name: Option<String>,
    /// One warning per skipped field, skipped duplicate section, and unknown
    /// key, in file order. Each starts with the line and column of its node,
    /// such as ``line 2, column 8: ignored `pane.gap`: expected an integer``.
    pub parse_warnings: Vec<String>,
}

/// Parses the `koshi.kdl` text `config_source_text` into its schema version,
/// its override layer, the theme it names, and one warning per skipped field,
/// skipped duplicate section, and unknown key.
///
/// # Errors
/// Returns [`ConfigError::Parse`] when `config_source_text` is not valid KDL.
///
/// Returns [`ConfigError::Validation`] when `version` is missing, repeated,
/// carries a `{ … }` block, is not a single integer from `0` to `4294967295`,
/// is `0`, or is newer than this build supports; when `update` is repeated; or
/// when an `update` field is not a single value of the right type and range.
/// The error holds every problem of the file in file order, the warnings
/// included. A first `version` node that declares a newer version gives that
/// one problem, and no other node is read. Each problem starts with the line
/// and column of the node or value at fault, such as ``line 3, column 5:
/// invalid config key `update.auto-check`: expected a boolean (#true or
/// #false)``. A missing `version` names the position where the first node
/// starts, `line 1, column 1` in an empty file.
pub fn parse_app_config(
    config_path: &Path,
    config_source_text: &str,
) -> Result<AppConfigFile, ConfigError> {
    let config_document = parse_kdl(config_path, config_source_text)?;
    parse_app_config_from_document(&config_document, config_source_text)
}

/// [`parse_app_config`] on `config_document`, the KDL document already parsed
/// from `config_source_text`.
///
/// # Errors
/// Returns the [`ConfigError::Validation`] errors of [`parse_app_config`].
pub(crate) fn parse_app_config_from_document(
    config_document: &KdlDocument,
    config_source_text: &str,
) -> Result<AppConfigFile, ConfigError> {
    if let Some(newer_version_error) = find_newer_version_error(config_document, config_source_text)
    {
        return Err(newer_version_error);
    }
    let mut schema_version = None;
    let mut partial_koshi_config = PartialKoshiConfig::default();
    let mut theme_name = None;
    let mut parse_problems = ParseProblems::default();
    let mut seen_config_section_names: BTreeSet<&str> = BTreeSet::new();
    for config_node in config_document.nodes() {
        let config_section_name = config_node.name().value();
        // Each section may appear once. A repeated `version` or `update` is a
        // refusal; a repeated field-partial section is a warning. The repeat
        // is not read, and the first stands.
        if APP_CONFIG_SECTION_NAMES.contains(&config_section_name)
            && !seen_config_section_names.insert(config_section_name)
        {
            if config_section_name == "version" || config_section_name == "update" {
                parse_problems.record_refusal(
                    config_node.span(),
                    format!("`{config_section_name}` is declared more than once"),
                );
            } else {
                parse_problems.record_warning(
                    config_node.span(),
                    format!("ignored duplicate `{config_section_name}` section"),
                );
            }
            continue;
        }
        match config_section_name {
            "version" => match parse_supported_schema_version(config_node, SCHEMA_VERSION) {
                Ok(declared_schema_version) => schema_version = Some(declared_schema_version),
                Err((version_error_span, version_error_detail)) => {
                    parse_problems.record_refusal(version_error_span, version_error_detail);
                }
            },
            // `theme` names which `themes/<name>.kdl` supplies the colors; the
            // colors themselves are never spelled here.
            "theme" => set_top_level_field(
                &mut theme_name,
                parse_nonempty_string_kdl_value(config_node),
                config_node,
                &mut parse_problems,
            ),
            "update" => {
                partial_koshi_config.update =
                    Some(parse_update_config(config_node, &mut parse_problems));
            }
            "pane" => {
                partial_koshi_config.pane =
                    Some(parse_pane_config(config_node, &mut parse_problems));
            }
            "scrollback" => {
                partial_koshi_config.scrollback =
                    Some(parse_scrollback_config(config_node, &mut parse_problems));
            }
            "layout" => {
                partial_koshi_config.layout =
                    Some(parse_layout_defaults(config_node, &mut parse_problems));
            }
            "mouse" => {
                partial_koshi_config.mouse =
                    Some(parse_mouse_config(config_node, &mut parse_problems));
            }
            "copy" => {
                partial_koshi_config.copy =
                    Some(parse_copy_config(config_node, &mut parse_problems));
            }
            "notifications" => {
                partial_koshi_config.notifications =
                    Some(parse_notification_config(config_node, &mut parse_problems));
            }
            "terminal" => {
                partial_koshi_config.terminal =
                    Some(parse_terminal_config(config_node, &mut parse_problems));
            }
            "logging" => {
                partial_koshi_config.logging =
                    Some(parse_logging_config(config_node, &mut parse_problems));
            }
            "image-support" => set_top_level_field(
                &mut partial_koshi_config.supports_image_protocols,
                parse_boolean_kdl_value(config_node),
                config_node,
                &mut parse_problems,
            ),
            "reduced-motion" => set_top_level_field(
                &mut partial_koshi_config.should_reduce_motion,
                parse_boolean_kdl_value(config_node),
                config_node,
                &mut parse_problems,
            ),
            "stay-in-pane-placement-mode-after-placement" => set_top_level_field(
                &mut partial_koshi_config.should_stay_in_pane_placement_mode_after_placement,
                parse_boolean_kdl_value(config_node),
                config_node,
                &mut parse_problems,
            ),
            "remote-reconnect" => set_top_level_field(
                &mut partial_koshi_config.should_reconnect_remote_session,
                parse_boolean_kdl_value(config_node),
                config_node,
                &mut parse_problems,
            ),
            "remote-link-timeout-ms" => set_top_level_field(
                &mut partial_koshi_config.remote_link_timeout_ms,
                parse_remote_link_timeout_ms(config_node),
                config_node,
                &mut parse_problems,
            ),
            "max-frames-per-second" => set_top_level_field(
                &mut partial_koshi_config.max_frames_per_second,
                parse_ranged_integer_kdl_value(
                    config_node,
                    LOWEST_MAX_FRAMES_PER_SECOND..=HIGHEST_MAX_FRAMES_PER_SECOND,
                ),
                config_node,
                &mut parse_problems,
            ),
            "allow-beta-features" => set_top_level_field(
                &mut partial_koshi_config.should_allow_beta_features,
                parse_boolean_kdl_value(config_node),
                config_node,
                &mut parse_problems,
            ),
            "allow-other-users" => set_top_level_field(
                &mut partial_koshi_config.should_allow_other_users,
                parse_boolean_kdl_value(config_node),
                config_node,
                &mut parse_problems,
            ),
            // `remote-listen` is `Option<Option<SocketAddr>>`: the outer
            // layer marks the field set, the inner carries the IP address and
            // port. No host name is looked up.
            "remote-listen" => set_top_level_field(
                &mut partial_koshi_config.remote_listen_address,
                parse_nonempty_string_kdl_value(config_node).and_then(|remote_listen_text| {
                    remote_listen_text
                        .parse::<SocketAddr>()
                        .map(Some)
                        .map_err(|_| {
                            format!(
                                "`{remote_listen_text}` is not an IP address and port, such as \
                                 `192.0.2.20:7654`"
                            )
                        })
                }),
                config_node,
                &mut parse_problems,
            ),
            // `shared-sessions-dir` is `Option<Option<PathBuf>>`: the outer
            // layer marks the field set, the inner carries the directory.
            "shared-sessions-dir" => set_top_level_field(
                &mut partial_koshi_config.shared_sessions_directory,
                parse_nonempty_string_kdl_value(config_node)
                    .map(|directory_path| Some(PathBuf::from(directory_path))),
                config_node,
                &mut parse_problems,
            ),
            "auto-close-session" => set_top_level_field(
                &mut partial_koshi_config.should_auto_close_session,
                parse_boolean_kdl_value(config_node),
                config_node,
                &mut parse_problems,
            ),
            "pane-clipboard-writes" => {
                partial_koshi_config.pane_clipboard_writes = Some(
                    parse_pane_clipboard_write_config(config_node, &mut parse_problems),
                );
            }
            unknown_section_name => parse_problems.record_warning(
                config_node.span(),
                format!(
                    "ignored {}",
                    format_unknown_key(unknown_section_name, APP_CONFIG_SECTION_NAMES)
                ),
            ),
        }
    }
    if !seen_config_section_names.contains("version") {
        parse_problems.record_refusal(config_document.span(), "file must declare `version`");
    }
    let parse_warnings = parse_problems.resolve_file_problems(config_source_text)?;
    Ok(AppConfigFile {
        schema_version: schema_version
            .expect("a file with no refusal has one `version` line that parsed"),
        layer: partial_koshi_config,
        theme_name,
        parse_warnings,
    })
}

/// Stores a parsed top-level field in `field_value_slot`. On `Err`, leaves
/// `field_value_slot` untouched and records one warning at `field_node`,
/// naming the field and the reason.
///
/// `field_node` is the top-level node (`remote-listen`). A `field_parse_result`
/// of `Err("must not be empty")` records ``ignored `remote-listen`: must not
/// be empty``.
fn set_top_level_field<FieldValue>(
    field_value_slot: &mut Option<FieldValue>,
    field_parse_result: Result<FieldValue, String>,
    field_node: &KdlNode,
    parse_problems: &mut ParseProblems,
) {
    match field_parse_result {
        Ok(parsed_field_value) => *field_value_slot = Some(parsed_field_value),
        Err(parse_error_detail) => parse_problems.record_warning(
            field_node.span(),
            format!(
                "ignored `{}`: {parse_error_detail}",
                field_node.name().value()
            ),
        ),
    }
}

/// Reads the strict `update { … }` block. A field whose value is the wrong
/// kind is recorded as a refusal; an unknown field is dropped with a warning.
fn parse_update_config(
    config_node: &KdlNode,
    parse_problems: &mut ParseProblems,
) -> PartialUpdateConfig {
    let mut partial_update_config = PartialUpdateConfig::default();
    let Some(section_children) = find_section_block(config_node, parse_problems) else {
        return partial_update_config;
    };
    for field_node in section_children.nodes() {
        let field_name = field_node.name().value();
        match field_name {
            "auto-check" => set_required_update_field(
                &mut partial_update_config.should_auto_check_for_updates,
                parse_boolean_kdl_value(field_node),
                field_node,
                parse_problems,
            ),
            "check-interval-days" => set_required_update_field(
                &mut partial_update_config.check_interval_days,
                parse_ranged_integer_kdl_value(field_node, 0..=u32::MAX),
                field_node,
                parse_problems,
            ),
            "allow-prerelease" => set_required_update_field(
                &mut partial_update_config.should_allow_prerelease_updates,
                parse_boolean_kdl_value(field_node),
                field_node,
                parse_problems,
            ),
            unknown_field_name => parse_problems.record_warning(
                field_node.span(),
                format!(
                    "ignored {}",
                    format_unknown_key(
                        &format!("update.{unknown_field_name}"),
                        &[
                            "update.auto-check",
                            "update.check-interval-days",
                            "update.allow-prerelease",
                        ],
                    )
                ),
            ),
        }
    }
    partial_update_config
}

/// Reads the `pane { … }` block.
fn parse_pane_config(
    config_node: &KdlNode,
    parse_problems: &mut ParseProblems,
) -> PartialPaneConfig {
    let mut partial_pane_config = PartialPaneConfig::default();
    let Some(section_children) = find_section_block(config_node, parse_problems) else {
        return partial_pane_config;
    };
    for field_node in section_children.nodes() {
        let field_name = field_node.name().value();
        match field_name {
            "min-cols" => set_parsed_field(
                &mut partial_pane_config.minimum_column_count,
                parse_ranged_integer_kdl_value(field_node, 0..=u16::MAX),
                "pane",
                field_node,
                parse_problems,
            ),
            "min-rows" => set_parsed_field(
                &mut partial_pane_config.minimum_row_count,
                parse_ranged_integer_kdl_value(field_node, 0..=u16::MAX),
                "pane",
                field_node,
                parse_problems,
            ),
            "gap" => set_parsed_field(
                &mut partial_pane_config.gap_cell_count,
                parse_ranged_integer_kdl_value(field_node, 0..=u16::MAX),
                "pane",
                field_node,
                parse_problems,
            ),
            unknown_field_name => parse_problems.record_warning(
                field_node.span(),
                format!(
                    "ignored {}",
                    format_unknown_key(
                        &format!("pane.{unknown_field_name}"),
                        &["pane.min-cols", "pane.min-rows", "pane.gap"],
                    )
                ),
            ),
        }
    }
    partial_pane_config
}

/// Reads the `scrollback { … }` block.
fn parse_scrollback_config(
    config_node: &KdlNode,
    parse_problems: &mut ParseProblems,
) -> PartialScrollbackConfig {
    let mut partial_scrollback_config = PartialScrollbackConfig::default();
    let Some(section_children) = find_section_block(config_node, parse_problems) else {
        return partial_scrollback_config;
    };
    for field_node in section_children.nodes() {
        let field_name = field_node.name().value();
        match field_name {
            "max-lines" => set_parsed_field(
                &mut partial_scrollback_config.maximum_line_count,
                parse_scrollback_limit(field_node),
                "scrollback",
                field_node,
                parse_problems,
            ),
            "max-bytes" => set_parsed_field(
                &mut partial_scrollback_config.maximum_byte_count,
                parse_scrollback_limit(field_node),
                "scrollback",
                field_node,
                parse_problems,
            ),
            "scroll-on-input" => set_parsed_field(
                &mut partial_scrollback_config.should_scroll_to_input,
                parse_boolean_kdl_value(field_node),
                "scrollback",
                field_node,
                parse_problems,
            ),
            unknown_field_name => parse_problems.record_warning(
                field_node.span(),
                format!(
                    "ignored {}",
                    format_unknown_key(
                        &format!("scrollback.{unknown_field_name}"),
                        &[
                            "scrollback.max-lines",
                            "scrollback.max-bytes",
                            "scrollback.scroll-on-input",
                        ],
                    )
                ),
            ),
        }
    }
    partial_scrollback_config
}

/// Reads the `layout { … }` block of default-layout settings.
fn parse_layout_defaults(
    config_node: &KdlNode,
    parse_problems: &mut ParseProblems,
) -> PartialLayoutDefaults {
    let mut partial_layout_defaults = PartialLayoutDefaults::default();
    let Some(section_children) = find_section_block(config_node, parse_problems) else {
        return partial_layout_defaults;
    };
    for field_node in section_children.nodes() {
        let field_name = field_node.name().value();
        match field_name {
            "new-pane-direction" => set_parsed_field(
                &mut partial_layout_defaults.new_pane_direction,
                parse_direction(field_node),
                "layout",
                field_node,
                parse_problems,
            ),
            unknown_field_name => parse_problems.record_warning(
                field_node.span(),
                format!(
                    "ignored {}",
                    format_unknown_key(
                        &format!("layout.{unknown_field_name}"),
                        &["layout.new-pane-direction"],
                    )
                ),
            ),
        }
    }
    partial_layout_defaults
}

/// Reads the `mouse { … }` block.
fn parse_mouse_config(
    config_node: &KdlNode,
    parse_problems: &mut ParseProblems,
) -> PartialMouseConfig {
    let mut partial_mouse_config = PartialMouseConfig::default();
    let Some(section_children) = find_section_block(config_node, parse_problems) else {
        return partial_mouse_config;
    };
    for field_node in section_children.nodes() {
        let field_name = field_node.name().value();
        match field_name {
            "border-resize" => set_parsed_field(
                &mut partial_mouse_config.can_resize_pane_border,
                parse_boolean_kdl_value(field_node),
                "mouse",
                field_node,
                parse_problems,
            ),
            "scroll-lines" => set_parsed_field(
                &mut partial_mouse_config.scroll_line_count,
                parse_ranged_integer_kdl_value(field_node, 0..=u16::MAX),
                "mouse",
                field_node,
                parse_problems,
            ),
            "wheel" => set_parsed_field(
                &mut partial_mouse_config.wheel_scroll,
                parse_wheel_scroll(field_node),
                "mouse",
                field_node,
                parse_problems,
            ),
            unknown_field_name => parse_problems.record_warning(
                field_node.span(),
                format!(
                    "ignored {}",
                    format_unknown_key(
                        &format!("mouse.{unknown_field_name}"),
                        &["mouse.border-resize", "mouse.scroll-lines", "mouse.wheel",],
                    )
                ),
            ),
        }
    }
    partial_mouse_config
}

/// Reads the `copy { … }` block.
fn parse_copy_config(
    config_node: &KdlNode,
    parse_problems: &mut ParseProblems,
) -> PartialCopyConfig {
    let mut partial_copy_config = PartialCopyConfig::default();
    let Some(section_children) = find_section_block(config_node, parse_problems) else {
        return partial_copy_config;
    };
    for field_node in section_children.nodes() {
        let field_name = field_node.name().value();
        match field_name {
            "trim-trailing-whitespace" => set_parsed_field(
                &mut partial_copy_config.should_trim_trailing_whitespace,
                parse_boolean_kdl_value(field_node),
                "copy",
                field_node,
                parse_problems,
            ),
            unknown_field_name => parse_problems.record_warning(
                field_node.span(),
                format!(
                    "ignored {}",
                    format_unknown_key(
                        &format!("copy.{unknown_field_name}"),
                        &["copy.trim-trailing-whitespace"],
                    )
                ),
            ),
        }
    }
    partial_copy_config
}

/// Reads the `notifications { … }` block.
fn parse_notification_config(
    config_node: &KdlNode,
    parse_problems: &mut ParseProblems,
) -> PartialNotificationConfig {
    let mut partial_notification_config = PartialNotificationConfig::default();
    let Some(section_children) = find_section_block(config_node, parse_problems) else {
        return partial_notification_config;
    };
    for field_node in section_children.nodes() {
        let field_name = field_node.name().value();
        match field_name {
            "toast" => set_parsed_field(
                &mut partial_notification_config.should_show_toasts,
                parse_boolean_kdl_value(field_node),
                "notifications",
                field_node,
                parse_problems,
            ),
            "toast-position" => set_parsed_field(
                &mut partial_notification_config.toast_position,
                parse_toast_position(field_node),
                "notifications",
                field_node,
                parse_problems,
            ),
            "toast-duration-ms" => set_parsed_field(
                &mut partial_notification_config.toast_duration_ms,
                parse_ranged_integer_kdl_value(
                    field_node,
                    LOWEST_TOAST_DURATION_MS..=HIGHEST_TOAST_DURATION_MS,
                ),
                "notifications",
                field_node,
                parse_problems,
            ),
            unknown_field_name => parse_problems.record_warning(
                field_node.span(),
                format!(
                    "ignored {}",
                    format_unknown_key(
                        &format!("notifications.{unknown_field_name}"),
                        &[
                            "notifications.toast",
                            "notifications.toast-position",
                            "notifications.toast-duration-ms",
                        ],
                    )
                ),
            ),
        }
    }
    partial_notification_config
}

/// Reads the node's single value as a [`ToastPosition`]: `"top-right"`,
/// `"top-left"`, `"bottom-right"` or `"bottom-left"`.
fn parse_toast_position(field_node: &KdlNode) -> Result<ToastPosition, String> {
    match parse_string_kdl_value(field_node)? {
        "top-right" => Ok(ToastPosition::TopRight),
        "top-left" => Ok(ToastPosition::TopLeft),
        "bottom-right" => Ok(ToastPosition::BottomRight),
        "bottom-left" => Ok(ToastPosition::BottomLeft),
        _ => {
            Err(r#"expected "top-right", "top-left", "bottom-right", or "bottom-left""#.to_string())
        }
    }
}

/// Reads the `terminal { … }` block.
fn parse_terminal_config(
    config_node: &KdlNode,
    parse_problems: &mut ParseProblems,
) -> PartialTerminalConfig {
    let mut partial_terminal_config = PartialTerminalConfig::default();
    let Some(section_children) = find_section_block(config_node, parse_problems) else {
        return partial_terminal_config;
    };
    for field_node in section_children.nodes() {
        let field_name = field_node.name().value();
        match field_name {
            // A blank or whitespace-only `term`/`colorterm` is dropped with a
            // warning.
            "term" => set_parsed_field(
                &mut partial_terminal_config.term,
                parse_nonempty_string_kdl_value(field_node),
                "terminal",
                field_node,
                parse_problems,
            ),
            "colorterm" => set_parsed_field(
                &mut partial_terminal_config.colorterm,
                parse_nonempty_string_kdl_value(field_node),
                "terminal",
                field_node,
                parse_problems,
            ),
            // `default-shell` is `Option<Option<String>>`: the outer layer marks
            // it set, the inner is the shell. The file can only name a shell;
            // it cannot unset one. A blank value is dropped with a warning.
            "default-shell" => set_parsed_field(
                &mut partial_terminal_config.default_shell,
                parse_nonempty_string_kdl_value(field_node).map(Some),
                "terminal",
                field_node,
                parse_problems,
            ),
            // `extended-keys` selects how a pane receives keys whose legacy
            // bytes another key also owns.
            "extended-keys" => set_parsed_field(
                &mut partial_terminal_config.extended_keys_mode,
                parse_extended_keys_mode(field_node),
                "terminal",
                field_node,
                parse_problems,
            ),
            unknown_field_name => parse_problems.record_warning(
                field_node.span(),
                format!(
                    "ignored {}",
                    format_unknown_key(
                        &format!("terminal.{unknown_field_name}"),
                        &[
                            "terminal.term",
                            "terminal.colorterm",
                            "terminal.default-shell",
                            "terminal.extended-keys",
                        ],
                    )
                ),
            ),
        }
    }
    partial_terminal_config
}

/// Reads the `logging { … }` block.
fn parse_logging_config(
    config_node: &KdlNode,
    parse_problems: &mut ParseProblems,
) -> PartialLoggingConfig {
    let mut partial_logging_config = PartialLoggingConfig::default();
    let Some(section_children) = find_section_block(config_node, parse_problems) else {
        return partial_logging_config;
    };
    for field_node in section_children.nodes() {
        let field_name = field_node.name().value();
        match field_name {
            "enabled" => set_parsed_field(
                &mut partial_logging_config.is_enabled,
                parse_boolean_kdl_value(field_node),
                "logging",
                field_node,
                parse_problems,
            ),
            "level" => set_parsed_field(
                &mut partial_logging_config.log_level,
                parse_log_level(field_node),
                "logging",
                field_node,
                parse_problems,
            ),
            "format" => set_parsed_field(
                &mut partial_logging_config.log_format,
                parse_log_format(field_node),
                "logging",
                field_node,
                parse_problems,
            ),
            "max-log-file-size-in-mb" => set_parsed_field(
                &mut partial_logging_config.max_log_file_size_in_mb,
                parse_ranged_integer_kdl_value(
                    field_node,
                    LOWEST_MAX_LOG_FILE_SIZE_IN_MB..=HIGHEST_MAX_LOG_FILE_SIZE_IN_MB,
                ),
                "logging",
                field_node,
                parse_problems,
            ),
            unknown_field_name => parse_problems.record_warning(
                field_node.span(),
                format!(
                    "ignored {}",
                    format_unknown_key(
                        &format!("logging.{unknown_field_name}"),
                        &[
                            "logging.enabled",
                            "logging.level",
                            "logging.format",
                            "logging.max-log-file-size-in-mb",
                        ],
                    )
                ),
            ),
        }
    }
    partial_logging_config
}

/// Reads the `pane-clipboard-writes { … }` block.
fn parse_pane_clipboard_write_config(
    config_node: &KdlNode,
    parse_problems: &mut ParseProblems,
) -> PartialPaneClipboardWriteConfig {
    let mut partial_pane_clipboard_write_config = PartialPaneClipboardWriteConfig::default();
    let Some(section_children) = find_section_block(config_node, parse_problems) else {
        return partial_pane_clipboard_write_config;
    };
    for field_node in section_children.nodes() {
        let field_name = field_node.name().value();
        match field_name {
            "allow" => set_parsed_field(
                &mut partial_pane_clipboard_write_config.should_allow_writes,
                parse_boolean_kdl_value(field_node),
                "pane-clipboard-writes",
                field_node,
                parse_problems,
            ),
            "max-copy-bytes" => set_parsed_field(
                &mut partial_pane_clipboard_write_config.maximum_copy_byte_count,
                parse_maximum_copy_byte_count(field_node),
                "pane-clipboard-writes",
                field_node,
                parse_problems,
            ),
            unknown_field_name => parse_problems.record_warning(
                field_node.span(),
                format!(
                    "ignored {}",
                    format_unknown_key(
                        &format!("pane-clipboard-writes.{unknown_field_name}"),
                        &[
                            "pane-clipboard-writes.allow",
                            "pane-clipboard-writes.max-copy-bytes",
                        ],
                    )
                ),
            ),
        }
    }
    partial_pane_clipboard_write_config
}

/// Reads the largest copy a pane may write, in decoded bytes. A value below
/// `1` gives `must be at least 1`. A value above `usize::MAX` becomes
/// `usize::MAX`. `max-copy-bytes 0` is an error, and `max-copy-bytes 1` yields
/// `1`.
fn parse_maximum_copy_byte_count(field_node: &KdlNode) -> Result<usize, String> {
    let maximum_copy_byte_count = parse_integer_kdl_value(field_node)?;
    if maximum_copy_byte_count < 1 {
        return Err("must be at least 1".to_string());
    }
    Ok(usize::try_from(maximum_copy_byte_count).unwrap_or(usize::MAX))
}

/// Reads the remote link timeout in milliseconds. A value below
/// [`MIN_LINK_TIMEOUT_MS`], `1000`, gives `must be at least 1000`, and a value
/// above `4294967295` gives `must be at most 4294967295`.
/// `remote-link-timeout-ms 999` is an error, and `remote-link-timeout-ms 1000`
/// yields `1000`.
fn parse_remote_link_timeout_ms(config_node: &KdlNode) -> Result<u32, String> {
    let remote_link_timeout_ms = parse_integer_kdl_value(config_node)?;
    if remote_link_timeout_ms < i128::from(MIN_LINK_TIMEOUT_MS) {
        return Err(format!("must be at least {MIN_LINK_TIMEOUT_MS}"));
    }
    u32::try_from(remote_link_timeout_ms).map_err(|_| "must be at most 4294967295".to_string())
}

/// Reads a scrollback cap. A negative value becomes `0` (no scrollback); a
/// value above `usize::MAX` becomes `usize::MAX`. `max-lines -5` yields `0`.
fn parse_scrollback_limit(field_node: &KdlNode) -> Result<usize, String> {
    Ok(parse_integer_kdl_value(field_node)?.clamp(0, usize::MAX as i128) as usize)
}

/// Reads the node's single value as a split [`Direction`].
fn parse_direction(field_node: &KdlNode) -> Result<Direction, String> {
    match parse_string_kdl_value(field_node)? {
        "left" => Ok(Direction::Left),
        "right" => Ok(Direction::Right),
        "up" => Ok(Direction::Up),
        "down" => Ok(Direction::Down),
        _ => Err(r#"expected "left", "right", "up", or "down""#.to_string()),
    }
}

/// Reads the node's single value as a [`LogLevel`], the lowest severity that
/// is written to the log file.
fn parse_log_level(field_node: &KdlNode) -> Result<LogLevel, String> {
    match parse_string_kdl_value(field_node)? {
        "info" => Ok(LogLevel::Info),
        "warning" => Ok(LogLevel::Warning),
        "error" => Ok(LogLevel::Error),
        _ => Err(r#"expected "info", "warning", or "error""#.to_string()),
    }
}

/// Reads the node's single value as a [`LogFormat`], the shape of each written
/// log line.
fn parse_log_format(field_node: &KdlNode) -> Result<LogFormat, String> {
    match parse_string_kdl_value(field_node)? {
        "pretty" => Ok(LogFormat::Pretty),
        "json" => Ok(LogFormat::Json),
        _ => Err(r#"expected "pretty" or "json""#.to_string()),
    }
}

/// Reads the node's single value as an [`ExtendedKeysMode`].
fn parse_extended_keys_mode(field_node: &KdlNode) -> Result<ExtendedKeysMode, String> {
    match parse_string_kdl_value(field_node)? {
        "on-request" => Ok(ExtendedKeysMode::OnRequest),
        "always" => Ok(ExtendedKeysMode::Always),
        _ => Err(r#"expected "on-request" or "always""#.to_string()),
    }
}

/// Reads the node's single value as a [`WheelScroll`] behavior.
fn parse_wheel_scroll(field_node: &KdlNode) -> Result<WheelScroll, String> {
    match parse_string_kdl_value(field_node)? {
        "scroll-scrollback" => Ok(WheelScroll::ScrollScrollback),
        "ignore" => Ok(WheelScroll::Ignore),
        _ => Err(r#"expected "scroll-scrollback" or "ignore""#.to_string()),
    }
}

/// Stores a parsed field of the strict `update` section in
/// `field_value_slot`. On `Err`, leaves `field_value_slot` untouched and
/// records one refusal at `field_node`, naming the field and the reason.
///
/// A `field_parse_result` of `Err("expected a boolean (#true or #false)")`
/// for `auto-check` records ``invalid config key `update.auto-check`:
/// expected a boolean (#true or #false)``.
fn set_required_update_field<FieldValue>(
    field_value_slot: &mut Option<FieldValue>,
    field_parse_result: Result<FieldValue, String>,
    field_node: &KdlNode,
    parse_problems: &mut ParseProblems,
) {
    match field_parse_result {
        Ok(parsed_field_value) => *field_value_slot = Some(parsed_field_value),
        Err(parse_error_detail) => parse_problems.record_refusal(
            field_node.span(),
            format!(
                "invalid config key `update.{}`: {parse_error_detail}",
                field_node.name().value()
            ),
        ),
    }
}

#[cfg(test)]
mod tests;
