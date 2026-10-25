//! KDL parsing entry point, and the field readers the file parsers share.
//!
//! [`parse_kdl`] wraps the `kdl` crate's document parser and attaches the
//! config file path to any syntax error as a [`ConfigParseDiagnostic`].
//! [`ParsedConfigFile`] keeps one file's text with what [`parse_kdl`] gave for
//! it, and every reader of that file reads the one document. The
//! `parse_*_kdl_value` readers turn one field node into a typed field value,
//! `set_parsed_field` stores one field value or records the skip in
//! `ParseProblems`, and [`format_unknown_key`] names the nearest allowed key
//! for an unrecognized one.

use std::path::{Path, PathBuf};

use kdl::{KdlDiagnostic, KdlDocument, KdlError, KdlNode, KdlValue};
use koshi_layout::template::ProfileTemplate;
use miette::SourceSpan;

use crate::app_config::{parse_app_config_from_document, AppConfigFile};
use crate::error::{
    format_problem_at_line_and_column, format_problems_at_line_and_column,
    validate_config_schema_version, ConfigError, ConfigParseDiagnostic, ConfigVersionDiagnostic,
};
use crate::profile::{parse_profile_from_document, ProfileError};
use crate::types::SCHEMA_VERSION;

#[cfg(test)]
mod tests;

/// The deepest `{ … }` nesting [`parse_kdl`] reads. Text that opens level 25
/// is refused before the `kdl` parser reads it. Koshi's own files nest at most
/// three levels deep.
pub(crate) const MAX_BLOCK_DEPTH: usize = 24;

/// The byte offset in `source_text` of the `{` that opens level
/// `MAX_BLOCK_DEPTH + 1`, or `None` when nothing nests that deep.
///
/// A `{` inside a line comment, a block comment, a quoted string or a raw
/// string opens no level. A `}` with no open level is ignored.
fn find_first_brace_past_depth_limit(source_text: &str) -> Option<usize> {
    let source_bytes = source_text.as_bytes();
    let mut depth: usize = 0;
    let mut block_comment_depth: usize = 0;
    let mut byte_index = 0;
    while byte_index < source_bytes.len() {
        let source_byte = source_bytes[byte_index];
        let next_source_byte = source_bytes.get(byte_index + 1).copied();
        if block_comment_depth > 0 {
            match (source_byte, next_source_byte) {
                (b'*', Some(b'/')) => {
                    block_comment_depth -= 1;
                    byte_index += 2;
                }
                (b'/', Some(b'*')) => {
                    block_comment_depth += 1;
                    byte_index += 2;
                }
                _ => byte_index += 1,
            }
            continue;
        }
        match source_byte {
            b'/' if next_source_byte == Some(b'/') => {
                byte_index += 2;
                while byte_index < source_bytes.len() && source_bytes[byte_index] != b'\n' {
                    byte_index += 1;
                }
            }
            b'/' if next_source_byte == Some(b'*') => {
                block_comment_depth = 1;
                byte_index += 2;
            }
            b'#' | b'"' => byte_index = find_string_end_offset(source_bytes, byte_index),
            b'{' => {
                depth += 1;
                if depth > MAX_BLOCK_DEPTH {
                    return Some(byte_index);
                }
                byte_index += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                byte_index += 1;
            }
            _ => byte_index += 1,
        }
    }
    None
}

/// The byte offset just past the string that starts at `start_byte_index`, where
/// `source_bytes[start_byte_index]` is `"` or `#`.
///
/// A string opens with `"` or `"""`, after any number of `#`. It closes at the
/// same quote run followed by the same `#` count. A `\` skips the next byte in
/// a string with no `#`. `#` runs that no `"` follows — `#true`, `#null` — are
/// not strings, and the answer is `start_byte_index + 1`. A string that never
/// closes ends at `source_bytes.len()`.
///
/// - `"a\"b" rest` from offset 0 → 6, the offset of the space
/// - `##"a"#b"## rest` from offset 0 → 10
/// - `"""`, a newline, `a " b`, a newline, `""" rest` from offset 0 → 13
/// - `#true` from offset 0 → 1
fn find_string_end_offset(source_bytes: &[u8], start_byte_index: usize) -> usize {
    let mut byte_index = start_byte_index;
    while source_bytes.get(byte_index) == Some(&b'#') {
        byte_index += 1;
    }
    let hash_character_count = byte_index - start_byte_index;
    if source_bytes.get(byte_index) != Some(&b'"') {
        return start_byte_index + 1;
    }
    let quote_character_count = if source_bytes[byte_index..].starts_with(b"\"\"\"") {
        3
    } else {
        1
    };
    byte_index += quote_character_count;
    while byte_index < source_bytes.len() {
        if hash_character_count == 0 && source_bytes[byte_index] == b'\\' {
            byte_index += 2;
            continue;
        }
        let delimiter_end_offset = byte_index + quote_character_count + hash_character_count;
        if let Some(delimiter_bytes) = source_bytes.get(byte_index..delimiter_end_offset) {
            let (quote_bytes, hash_bytes) = delimiter_bytes.split_at(quote_character_count);
            if quote_bytes.iter().all(|source_byte| *source_byte == b'"')
                && hash_bytes.iter().all(|source_byte| *source_byte == b'#')
            {
                return delimiter_end_offset;
            }
        }
        byte_index += 1;
    }
    source_bytes.len()
}

/// The refusal for source text that nests past [`MAX_BLOCK_DEPTH`], pointing at
/// the `{` at `depth_limit_byte_offset`.
fn build_nested_depth_diagnostic(
    config_path: &Path,
    config_source_text: &str,
    depth_limit_byte_offset: usize,
) -> ConfigParseDiagnostic {
    let shared_config_source_text = std::sync::Arc::new(config_source_text.to_string());
    ConfigParseDiagnostic::from_kdl_error(
        config_path,
        KdlError {
            input: shared_config_source_text.clone(),
            diagnostics: vec![KdlDiagnostic {
                input: shared_config_source_text,
                span: (depth_limit_byte_offset, 1).into(),
                message: Some(format!(
                    "blocks nest more than {MAX_BLOCK_DEPTH} levels deep"
                )),
                label: Some(format!("this block opens level {}", MAX_BLOCK_DEPTH + 1)),
                help: Some("flatten the nesting".to_string()),
                severity: miette::Severity::Error,
            }],
        },
    )
}

/// Parses `source_text` — the already-read contents of the config file at `path` —
/// into a [`KdlDocument`]. Does no file I/O: discovery and reading happen in
/// the caller.
///
/// # Errors
/// Returns a [`ConfigParseDiagnostic`] carrying `path` and the span-tagged
/// KDL error for pretty rendering when `source_text` is not valid KDL syntax, or
/// when a block nests more than 24 levels deep.
pub fn parse_kdl(
    config_path: &Path,
    config_source_text: &str,
) -> Result<KdlDocument, ConfigParseDiagnostic> {
    if let Some(depth_limit_byte_offset) = find_first_brace_past_depth_limit(config_source_text) {
        return Err(build_nested_depth_diagnostic(
            config_path,
            config_source_text,
            depth_limit_byte_offset,
        ));
    }
    config_source_text
        .parse::<KdlDocument>()
        .map_err(|parse_error| ConfigParseDiagnostic::from_kdl_error(config_path, parse_error))
}

/// One config file's path, its text, and what [`parse_kdl`] gave for that
/// text: the KDL document, or the diagnostic of text that is not valid KDL.
///
/// [`ParsedConfigFile::parse`] runs [`parse_kdl`] once. Every reader of the
/// file reads that one result: [`migrate_config`],
/// [`ParsedConfigFile::parse_app_config`] and
/// [`ParsedConfigFile::parse_profile`].
///
/// [`migrate_config`]: crate::migration::migrate_config
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedConfigFile {
    /// The path of the file the text was read from.
    pub(crate) config_path: PathBuf,
    /// The text read.
    pub(crate) config_source_text: String,
    /// What [`parse_kdl`] gave for `config_source_text`.
    pub(crate) config_document: Result<KdlDocument, ConfigParseDiagnostic>,
}

impl ParsedConfigFile {
    /// Parses `config_source_text`, the text of the file at `config_path`,
    /// with [`parse_kdl`].
    #[must_use]
    pub fn parse(config_path: &Path, config_source_text: String) -> Self {
        let config_document = parse_kdl(config_path, &config_source_text);
        Self {
            config_path: config_path.to_path_buf(),
            config_source_text,
            config_document,
        }
    }

    /// The path of the file the text was read from.
    #[must_use]
    pub fn get_config_path(&self) -> &Path {
        &self.config_path
    }

    /// The text read.
    #[must_use]
    pub fn get_source_text(&self) -> &str {
        &self.config_source_text
    }

    /// What [`parse_app_config`] gives for this file's text, read from the
    /// document parsed once.
    ///
    /// # Errors
    /// The errors [`parse_app_config`] states.
    ///
    /// [`parse_app_config`]: crate::app_config::parse_app_config
    pub fn parse_app_config(&self) -> Result<AppConfigFile, ConfigError> {
        match &self.config_document {
            Ok(config_document) => {
                parse_app_config_from_document(config_document, &self.config_source_text)
            }
            Err(parse_diagnostic) => Err(parse_diagnostic.build_config_error()),
        }
    }

    /// What [`parse_profile`] gives for this file's text, read from the
    /// document parsed once.
    ///
    /// # Errors
    /// The errors [`parse_profile`] states.
    ///
    /// [`parse_profile`]: crate::profile::parse_profile
    pub fn parse_profile(&self) -> Result<ProfileTemplate, ProfileError> {
        match &self.config_document {
            Ok(config_document) => parse_profile_from_document(
                &self.config_path,
                &self.config_source_text,
                config_document,
            ),
            Err(parse_diagnostic) => Err(ProfileError::Syntax(parse_diagnostic.clone())),
        }
    }
}

// Field-value readers shared by the `koshi.kdl` and theme-file parsers. Each
// takes one field node (`key value`) and returns the typed field value, or a plain-words
// reason it could not be read.

/// The problems of one field-partial file, `koshi.kdl` or a theme, each
/// recorded at the node it names. A warning skips one setting, and the rest of
/// the file still applies. A refusal refuses the whole file.
#[derive(Debug, Default)]
pub(crate) struct ParseProblems {
    /// `(byte offset where the node starts, message)` per problem, warnings
    /// and refusals alike, in the order recorded.
    problem_messages_by_offset: Vec<(usize, String)>,
    /// Whether at least one refusal is recorded.
    has_refusal: bool,
}

impl ParseProblems {
    /// Records `warning_message`, a skipped setting, at the start of
    /// `warning_span`.
    pub(crate) fn record_warning(
        &mut self,
        warning_span: SourceSpan,
        warning_message: impl Into<String>,
    ) {
        self.problem_messages_by_offset
            .push((warning_span.offset(), warning_message.into()));
    }

    /// Records `refusal_message`, a problem that refuses the whole file, at
    /// the start of `refusal_span`.
    pub(crate) fn record_refusal(
        &mut self,
        refusal_span: SourceSpan,
        refusal_message: impl Into<String>,
    ) {
        self.problem_messages_by_offset
            .push((refusal_span.offset(), refusal_message.into()));
        self.has_refusal = true;
    }

    /// Every recorded problem as `line <L>, column <C>: <message>`, the line
    /// and column of its node in `config_source_text`, in file order:
    /// ascending node start, and the order recorded for one start. With no
    /// refusal recorded, returns the warnings.
    ///
    /// Example — `version 2\npane { gap "x" }` gives ``line 2, column 8:
    /// ignored `pane.gap`: expected an integer``.
    ///
    /// # Errors
    /// Returns [`ConfigError::Validation`] that holds every problem, warnings
    /// included, when a refusal is recorded.
    pub(crate) fn resolve_file_problems(
        mut self,
        config_source_text: &str,
    ) -> Result<Vec<String>, ConfigError> {
        self.problem_messages_by_offset
            .sort_by_key(|(byte_offset, _)| *byte_offset);
        let problem_messages =
            format_problems_at_line_and_column(config_source_text, self.problem_messages_by_offset);
        if self.has_refusal {
            return Err(ConfigError::Validation { problem_messages });
        }
        Ok(problem_messages)
    }
}

/// The `{ … }` block of a section node, warning about a value written on the
/// section line, which no section reads.
///
/// `scrollback 5000` gives `None` and the warning ``ignored `scrollback`
/// value: a section takes a `{ … }` block`` at the `scrollback` node.
/// `scrollback { max-lines 5000 }` gives the block and no warning.
pub(crate) fn find_section_block<'a>(
    section_node: &'a KdlNode,
    parse_problems: &mut ParseProblems,
) -> Option<&'a KdlDocument> {
    if !section_node.entries().is_empty() {
        parse_problems.record_warning(
            section_node.span(),
            format!(
                "ignored `{}` value: a section takes a `{{ … }}` block",
                section_node.name().value()
            ),
        );
    }
    section_node.children()
}

/// The node's single unnamed argument.
///
/// Returns `takes no children` when the node carries a `{ … }` child block:
/// `theme "midnight" { foo }` is an error. Returns `expected exactly one
/// value` when the node holds anything other than one unnamed argument.
pub(crate) fn parse_single_kdl_value(field_node: &KdlNode) -> Result<&KdlValue, String> {
    if field_node.children().is_some() {
        return Err("takes no children".to_string());
    }
    match field_node.entries() {
        [argument_entry] if argument_entry.name().is_none() => Ok(argument_entry.value()),
        _ => Err("expected exactly one value".to_string()),
    }
}

/// Reads the node's single value as a boolean, or
/// `expected a boolean (#true or #false)`.
pub(crate) fn parse_boolean_kdl_value(field_node: &KdlNode) -> Result<bool, String> {
    parse_single_kdl_value(field_node)?
        .as_bool()
        .ok_or_else(|| "expected a boolean (#true or #false)".to_string())
}

/// Reads the node's single value as a string, borrowed from the node, or
/// `expected a string`.
pub(crate) fn parse_string_kdl_value(field_node: &KdlNode) -> Result<&str, String> {
    parse_single_kdl_value(field_node)?
        .as_string()
        .ok_or_else(|| "expected a string".to_string())
}

/// Reads the node's single value as a string, **trimmed** of surrounding
/// whitespace. A value that is empty or whitespace-only is rejected with
/// `must not be empty`.
///
/// `term " xterm-256color "` yields `xterm-256color`; `theme " midnight "`
/// yields `midnight`; `term "   "` and `term ""` are both errors.
pub(crate) fn parse_nonempty_string_kdl_value(field_node: &KdlNode) -> Result<String, String> {
    let trimmed_text = parse_string_kdl_value(field_node)?.trim();
    if trimmed_text.is_empty() {
        Err("must not be empty".to_string())
    } else {
        Ok(trimmed_text.to_string())
    }
}

/// Reads the node's single value as an integer, or `expected an integer`.
pub(crate) fn parse_integer_kdl_value(field_node: &KdlNode) -> Result<i128, String> {
    parse_single_kdl_value(field_node)?
        .as_integer()
        .ok_or_else(|| "expected an integer".to_string())
}

/// Reads the node's single value as a `u16`. A value outside `0..=65535`
/// gives `must be between 0 and 65535`.
pub(crate) fn parse_u16_kdl_value(field_node: &KdlNode) -> Result<u16, String> {
    u16::try_from(parse_integer_kdl_value(field_node)?)
        .map_err(|_| "must be between 0 and 65535".to_string())
}

/// Reads the node's single value as a `u32`. A value outside `0..=4294967295`
/// gives `must be between 0 and 4294967295`.
pub(crate) fn parse_u32_kdl_value(field_node: &KdlNode) -> Result<u32, String> {
    u32::try_from(parse_integer_kdl_value(field_node)?)
        .map_err(|_| "must be between 0 and 4294967295".to_string())
}

/// Reads the schema number a `version` node declares. Every config file that
/// carries a `version` node reads it through this function, by way of
/// [`parse_supported_schema_version`]: `koshi.kdl`, a theme, `keybinding.kdl`,
/// a profile, and migration.
///
/// `version 3` gives `Ok(3)`.
///
/// # Errors
/// Each error carries the span a caret points at and the reason:
///
/// - the whole node and ``` `version` takes no children ``` when the node
///   carries a `{ … }` block;
/// - the whole node and ``` `version` takes exactly one integer argument ```
///   when the node's arguments are not exactly one unnamed value;
/// - the argument and ``` `version` must be an integer from 1 to 4294967295 ```
///   when that value is not an integer or falls outside `0..=4294967295`.
///
/// A declared `0` is returned as `Ok(0)`;
/// [`validate_config_schema_version`] rejects it.
pub(crate) fn parse_version_argument(
    version_node: &KdlNode,
) -> Result<u32, (SourceSpan, &'static str)> {
    if version_node.children().is_some() {
        return Err((version_node.span(), "`version` takes no children"));
    }
    let version_argument = match version_node.entries() {
        [version_argument] if version_argument.name().is_none() => version_argument,
        _ => {
            return Err((
                version_node.span(),
                "`version` takes exactly one integer argument",
            ))
        }
    };
    version_argument
        .value()
        .as_integer()
        .and_then(|schema_version_value| u32::try_from(schema_version_value).ok())
        .ok_or((
            version_argument.span(),
            "`version` must be an integer from 1 to 4294967295",
        ))
}

/// The schema number `version_node` declares, from 1 through
/// `supported_schema_version`.
///
/// `version 2` with a `supported_schema_version` of 2 gives `Ok(2)`.
///
/// # Errors
/// Returns the span and the reason [`parse_version_argument`] gives for a node
/// that holds no integer from 0 to 4294967295. Returns the whole node and the
/// [`ConfigVersionDiagnostic`] message from [`validate_config_schema_version`]
/// for `version 0` and for a version newer than `supported_schema_version`.
pub(crate) fn parse_supported_schema_version(
    version_node: &KdlNode,
    supported_schema_version: u32,
) -> Result<u32, (SourceSpan, String)> {
    let declared_schema_version = parse_version_argument(version_node).map_err(
        |(version_error_span, version_error_detail)| {
            (version_error_span, version_error_detail.to_string())
        },
    )?;
    validate_config_schema_version(declared_schema_version, supported_schema_version)
        .map_err(|version_diagnostic| (version_node.span(), version_diagnostic.to_string()))?;
    Ok(declared_schema_version)
}

/// The problem of a document whose first `version` node declares a schema
/// newer than [`SCHEMA_VERSION`]: that node's span and the
/// [`ConfigVersionDiagnostic::TooNew`] message. `None` for every other
/// document, one with no `version` node or a malformed one included.
///
/// A file parser that gets `Some` reports that one problem and reads no other
/// node: `version 3` then `frobnicate 1` gives only `config schema version 3
/// is newer than this koshi supports (2)` at the `version` node.
pub(crate) fn find_newer_version_problem(
    config_document: &KdlDocument,
) -> Option<(SourceSpan, String)> {
    let version_node = config_document
        .nodes()
        .iter()
        .find(|kdl_node| kdl_node.name().value() == "version")?;
    let declared_schema_version = parse_version_argument(version_node).ok()?;
    if declared_schema_version <= SCHEMA_VERSION {
        return None;
    }
    let newer_version_diagnostic = ConfigVersionDiagnostic::TooNew {
        declared_schema_version,
        supported_schema_version: SCHEMA_VERSION,
    };
    Some((version_node.span(), newer_version_diagnostic.to_string()))
}

/// The [`ConfigError::Validation`] that holds the one problem
/// [`find_newer_version_problem`] finds in `config_document`, the document
/// parsed from `config_source_text`, at its line and column. `None` for every
/// other document.
pub(crate) fn find_newer_version_error(
    config_document: &KdlDocument,
    config_source_text: &str,
) -> Option<ConfigError> {
    let (newer_version_span, newer_version_detail) = find_newer_version_problem(config_document)?;
    Some(ConfigError::Validation {
        problem_messages: vec![format_problem_at_line_and_column(
            config_source_text,
            newer_version_span,
            newer_version_detail,
        )],
    })
}

/// The first problem in `version_node`, with the span a caret points at, or
/// `None` when the node declares a version from 1 through [`SCHEMA_VERSION`].
/// The problem is the error [`parse_supported_schema_version`] gives.
pub(crate) fn find_version_node_problem(version_node: &KdlNode) -> Option<(SourceSpan, String)> {
    parse_supported_schema_version(version_node, SCHEMA_VERSION).err()
}

/// Stores a parsed field value in `field_value_slot`. On `Err`, leaves the
/// field value untouched and records one warning at `field_node`, naming the
/// field and reason.
///
/// `section_name` is the enclosing block (`pane`), and `field_node` the field
/// node (`min-cols`). A `field_parse_result` of `Err("expected an integer")`
/// records ``ignored `pane.min-cols`: expected an integer``.
pub(crate) fn set_parsed_field<FieldValue>(
    field_value_slot: &mut Option<FieldValue>,
    field_parse_result: Result<FieldValue, String>,
    section_name: &str,
    field_node: &KdlNode,
    parse_problems: &mut ParseProblems,
) {
    match field_parse_result {
        Ok(parsed_field_value) => *field_value_slot = Some(parsed_field_value),
        Err(parse_error_message) => parse_problems.record_warning(
            field_node.span(),
            format!(
                "ignored `{section_name}.{}`: {parse_error_message}",
                field_node.name().value()
            ),
        ),
    }
}

/// Names the nearest allowed key for an unknown config key, measured by
/// Levenshtein edit distance in characters. A tie goes to the earliest key in
/// `allowed_key_names`.
///
/// `format_unknown_key("pane.min-col", &["pane.min-cols", "pane.min-rows"])` gives
/// ``unknown key `pane.min-col`; did you mean `pane.min-cols`?``.
///
/// # Panics
/// Panics when `allowed_key_names` is empty.
#[must_use]
pub fn format_unknown_key(unknown_key_name: &str, allowed_key_names: &[&str]) -> String {
    let nearest_key_name = allowed_key_names
        .iter()
        .min_by_key(|candidate_key_name| {
            compute_edit_distance(unknown_key_name, candidate_key_name)
        })
        .expect("every config key set is non-empty");
    format!("unknown key `{unknown_key_name}`; did you mean `{nearest_key_name}`?")
}

/// The Levenshtein edit distance between `left_text` and `right_text`, counted in
/// characters. `"colors.acent"` against `"colors.accent"` is `1`.
fn compute_edit_distance(left_text: &str, right_text: &str) -> usize {
    let right_character_count = right_text.chars().count();
    let mut previous_row: Vec<usize> = (0..=right_character_count).collect();
    let mut current_row = vec![0; previous_row.len()];
    for (left_character_index, left_character) in left_text.chars().enumerate() {
        current_row[0] = left_character_index + 1;
        for (right_character_index, right_character) in right_text.chars().enumerate() {
            current_row[right_character_index + 1] = if left_character == right_character {
                previous_row[right_character_index]
            } else {
                1 + previous_row[right_character_index]
                    .min(current_row[right_character_index])
                    .min(previous_row[right_character_index + 1])
            };
        }
        std::mem::swap(&mut previous_row, &mut current_row);
    }
    previous_row[right_character_count]
}
