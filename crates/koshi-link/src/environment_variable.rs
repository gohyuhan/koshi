//! Reading the `KOSHI_*` environment variables a koshi program was started
//! with: [`read_environment_variable`] reads one variable, and
//! [`parse_environment_variable_id`] reads an id out of its value.

use koshi_core::ids::parse_prefixed_uuid;
use uuid::Uuid;

/// The value of `environment_variable_name` in this process's environment, or
/// `None` when it is not set. A value that is not UTF-8 is read with each bad
/// sequence replaced by `U+FFFD`.
pub(crate) fn read_environment_variable(environment_variable_name: &str) -> Option<String> {
    std::env::var_os(environment_variable_name)
        .map(|environment_variable_text| environment_variable_text.to_string_lossy().into_owned())
}

/// Parse `environment_variable_text`, the value of `environment_variable_name`,
/// as `<identifier_prefix>-<uuid>` or a bare UUID.
///
/// # Errors
/// The sentence that names the variable and its value, such as
/// `` `KOSHI_PANE_ID` is `pane-1`: expected `pane-<uuid>` or a bare UUID ``.
pub(crate) fn parse_environment_variable_id(
    environment_variable_name: &str,
    environment_variable_text: &str,
    identifier_prefix: &str,
) -> Result<Uuid, String> {
    parse_prefixed_uuid(environment_variable_text, identifier_prefix).map_err(|expected_format| {
        format!("`{environment_variable_name}` is `{environment_variable_text}`: {expected_format}")
    })
}
