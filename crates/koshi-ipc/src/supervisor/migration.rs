//! Translate a running version 1 pane supervisor's frames during an image swap.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use super::{IncomingSupervisorMessage, SupervisorRequest};

/// The supervisor-link protocol version of a supervisor that koshi 0.3.0 or
/// 0.4.0 started. [`serialize_previous_supervisor_request`] writes, and
/// [`deserialize_previous_supervisor_message`] reads, the frames of this version.
pub const PREVIOUS_SUPERVISOR_PROTOCOL_VERSION: u32 = 1;

/// One frame from a supervisor that koshi 0.3.0 or 0.4.0 started, read as
/// [`deserialize_previous_supervisor_message`] reads it.
pub struct PreviousSupervisorMessage {
    /// The frame, with each field under the name this build reads.
    pub supervisor_message: IncomingSupervisorMessage,
}

impl<'de> Deserialize<'de> for PreviousSupervisorMessage {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        let message_json = Value::deserialize(deserializer)?;
        deserialize_previous_supervisor_message(message_json)
            .map(|supervisor_message| PreviousSupervisorMessage { supervisor_message })
            .map_err(DeserializerType::Error::custom)
    }
}

fn rename_json_field(
    json_fields: &mut Map<String, Value>,
    previous_field_name: &str,
    current_field_name: &str,
) {
    if let Some(json_field) = json_fields.remove(previous_field_name) {
        json_fields.insert(current_field_name.to_string(), json_field);
    }
}

fn get_json_object<'a>(
    json_value: &'a mut Value,
    object_name: &str,
) -> Result<&'a mut Map<String, Value>, String> {
    json_value
        .as_object_mut()
        .ok_or_else(|| format!("{object_name} must be an object"))
}

fn get_required_json_field<'a>(
    json_fields: &'a mut Map<String, Value>,
    field_name: &str,
) -> Result<&'a mut Value, String> {
    json_fields
        .get_mut(field_name)
        .ok_or_else(|| format!("{field_name} is missing"))
}

fn rename_pty_size_fields_to_previous(pty_size: &mut Value) -> Result<(), String> {
    let size_fields = get_json_object(pty_size, "pty_size")?;
    rename_json_field(size_fields, "column_count", "cols");
    rename_json_field(size_fields, "row_count", "rows");
    Ok(())
}

fn rename_pty_size_fields_to_current(pty_size: &mut Value) -> Result<(), String> {
    let size_fields = get_json_object(pty_size, "pty_size")?;
    rename_json_field(size_fields, "cols", "column_count");
    rename_json_field(size_fields, "rows", "row_count");
    Ok(())
}

/// Serialize `request` as the frame that a supervisor of koshi 0.3.0 or 0.4.0
/// reads: the Hello names protocol [`PREVIOUS_SUPERVISOR_PROTOCOL_VERSION`] only,
/// and each field carries the name of that release.
///
/// # Errors
/// Returns a message when `request` cannot be serialized, or when its JSON
/// lacks a field this function renames.
pub fn serialize_previous_supervisor_request(request: &SupervisorRequest) -> Result<Value, String> {
    let mut request_json = serde_json::to_value(request)
        .map_err(|serialize_error| format!("encode supervisor request: {serialize_error}"))?;
    let request_fields = get_json_object(&mut request_json, "supervisor request")?;
    rename_json_field(request_fields, "request_kind", "kind");
    let request_kind_json = get_required_json_field(request_fields, "kind")?;
    if request_kind_json.is_string() {
        return Ok(request_json);
    }
    let request_kinds = get_json_object(request_kind_json, "request kind")?;
    if let Some(hello_request) = request_kinds.get_mut("Hello") {
        let hello_fields = get_json_object(hello_request, "Hello")?;
        hello_fields.insert(
            "min_protocol_version".to_string(),
            Value::from(PREVIOUS_SUPERVISOR_PROTOCOL_VERSION),
        );
        hello_fields.insert(
            "max_protocol_version".to_string(),
            Value::from(PREVIOUS_SUPERVISOR_PROTOCOL_VERSION),
        );
        hello_fields.remove("minimum_protocol_version");
        hello_fields.remove("maximum_protocol_version");
        rename_json_field(hello_fields, "connection_token", "token");
    }
    if let Some(spawn_request) = request_kinds.get_mut("Spawn") {
        let spawn_fields = get_json_object(spawn_request, "Spawn")?;
        rename_json_field(spawn_fields, "spawn_spec", "spec");
        rename_json_field(spawn_fields, "pty_size", "size");
        let spec_fields =
            get_json_object(get_required_json_field(spawn_fields, "spec")?, "Spawn.spec")?;
        rename_json_field(spec_fields, "arguments", "args");
        rename_json_field(spec_fields, "working_directory", "cwd");
        rename_json_field(spec_fields, "environment_variables", "env");
        rename_pty_size_fields_to_previous(get_required_json_field(spawn_fields, "size")?)?;
    }
    if let Some(resize_request) = request_kinds.get_mut("Resize") {
        let resize_fields = get_json_object(resize_request, "Resize")?;
        rename_json_field(resize_fields, "pty_size", "size");
        rename_pty_size_fields_to_previous(get_required_json_field(resize_fields, "size")?)?;
    }
    if let Some(write_request) = request_kinds.get_mut("Write") {
        rename_json_field(
            get_json_object(write_request, "Write")?,
            "input_bytes",
            "bytes",
        );
    }
    if let Some(kill_request) = request_kinds.get_mut("Kill") {
        let kill_fields = get_json_object(kill_request, "Kill")?;
        let kill_policy = get_required_json_field(kill_fields, "kill_policy")?;
        if let Some(policy_fields) = kill_policy.as_object_mut() {
            for policy_name in ["Graceful", "GracefulTree"] {
                if let Some(graceful) = policy_fields.get_mut(policy_name) {
                    rename_json_field(
                        get_json_object(graceful, policy_name)?,
                        "timeout_duration",
                        "timeout",
                    );
                }
            }
        }
    }
    Ok(request_json)
}

/// Deserialize `message_json`, one frame from a supervisor that koshi 0.3.0 or
/// 0.4.0 started, once each field carries the name this build reads.
///
/// # Errors
/// Returns a message when the frame is malformed or its known payload is invalid.
pub fn deserialize_previous_supervisor_message(
    mut message_json: Value,
) -> Result<IncomingSupervisorMessage, String> {
    let message_fields = get_json_object(&mut message_json, "supervisor message")?;
    if let Some(supervisor_response) = message_fields.get_mut("Response") {
        let response_fields = get_json_object(supervisor_response, "Response")?;
        rename_json_field(response_fields, "result", "answer_result");
        let answer_result_json = get_required_json_field(response_fields, "answer_result")?;
        if answer_result_json.is_string() {
            return deserialize_migrated_frame(&message_json);
        }
        let answer_variants = get_json_object(answer_result_json, "answer result")?;
        if let Some(spawned_pane) = answer_variants.get_mut("Spawned") {
            rename_json_field(
                get_json_object(spawned_pane, "Spawned")?,
                "pid",
                "process_id",
            );
        }
        if let Some(supervisor_panes) = answer_variants.get_mut("Panes") {
            let pane_records = supervisor_panes
                .as_array_mut()
                .ok_or_else(|| "Panes must be an array".to_string())?;
            for supervisor_pane in pane_records {
                let pane_fields = get_json_object(supervisor_pane, "Panes member")?;
                rename_json_field(pane_fields, "pid", "process_id");
                rename_json_field(pane_fields, "size", "pty_size");
                rename_pty_size_fields_to_current(get_required_json_field(
                    pane_fields,
                    "pty_size",
                )?)?;
            }
        }
        if let Some(supervisor_error) = answer_variants.get_mut("Error") {
            let error_fields = get_json_object(supervisor_error, "Error")?;
            if let Some(error_code) = error_fields.get_mut("code") {
                if let Some(previous_code) = error_code.as_str() {
                    let current_code = match previous_code {
                        "bad_token" => "BadToken",
                        "unsupported_version" => "UnsupportedVersion",
                        "unsupported_kind" => "UnsupportedKind",
                        "malformed_request" => "MalformedRequest",
                        "not_found" => "NotFound",
                        "hello_required" => "HelloRequired",
                        "other_users_off" => "OtherUsersOff",
                        _ => "Unknown",
                    };
                    *error_code = Value::String(current_code.to_string());
                }
            }
        }
    }
    if let Some(supervisor_event) = message_fields.get_mut("Event") {
        let event_fields = get_json_object(supervisor_event, "Event")?;
        if let Some(output_event) = event_fields.get_mut("Output") {
            rename_json_field(
                get_json_object(output_event, "Output")?,
                "bytes",
                "output_bytes",
            );
        }
        if let Some(exit_event) = event_fields.get_mut("Exited") {
            rename_json_field(
                get_json_object(exit_event, "Exited")?,
                "status",
                "exit_status",
            );
        }
    }
    deserialize_migrated_frame(&message_json)
}

/// Serialize `message_json`, a supervisor frame whose fields carry the names
/// this build reads, to JSON text, and deserialize that text as an
/// [`IncomingSupervisorMessage`].
///
/// # Errors
/// Returns `encode migrated supervisor frame: <failure>` when the JSON cannot
/// be serialized, and `decode previous supervisor frame: <failure>` when the
/// text is no supervisor message.
fn deserialize_migrated_frame(message_json: &Value) -> Result<IncomingSupervisorMessage, String> {
    let migrated_frame = serde_json::to_string(message_json).map_err(|serialize_error| {
        format!("encode migrated supervisor frame: {serialize_error}")
    })?;
    serde_json::from_str(&migrated_frame).map_err(|deserialize_error| {
        format!("decode previous supervisor frame: {deserialize_error}")
    })
}

#[cfg(test)]
mod tests;
