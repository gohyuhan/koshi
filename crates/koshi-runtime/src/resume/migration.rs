//! Ordered conversion of released resume bodies into the current shape.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;
use serde_json::{Map, Number, Value};

use koshi_core::ids::{PaneId, SessionId};
use koshi_core::process::PtySize;
use koshi_session::session::state::Session;
use koshi_storage::error::StorageError;
use koshi_terminal::state::TerminalState;

use super::{CarriedPaneState, CarriedQuit, ResumeBody, RESUME_FORMAT};

struct ParsedJsonObject<'a> {
    json_fields: BTreeMap<String, &'a RawValue>,
    repeated_json_field_names: BTreeSet<String>,
}

impl<'de> Deserialize<'de> for ParsedJsonObject<'de> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ParsedJsonObjectVisitor;

        impl<'de> Visitor<'de> for ParsedJsonObjectVisitor {
            type Value = ParsedJsonObject<'de>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                mut json_map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut json_fields = BTreeMap::new();
                let mut repeated_json_field_names = BTreeSet::new();
                while let Some((field_name, raw_field)) =
                    json_map.next_entry::<String, &'de RawValue>()?
                {
                    if json_fields.insert(field_name.clone(), raw_field).is_some() {
                        repeated_json_field_names.insert(field_name);
                    }
                }
                Ok(ParsedJsonObject {
                    json_fields,
                    repeated_json_field_names,
                })
            }
        }

        deserializer.deserialize_map(ParsedJsonObjectVisitor)
    }
}

struct OpaqueByteArraySeed<'storage, 'de> {
    opaque_byte_arrays: &'storage mut Vec<&'de RawValue>,
}

impl<'de> DeserializeSeed<'de> for OpaqueByteArraySeed<'_, 'de> {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OpaqueByteArrayVisitor<'storage, 'de> {
            opaque_byte_arrays: &'storage mut Vec<&'de RawValue>,
        }

        impl<'de> Visitor<'de> for OpaqueByteArrayVisitor<'_, 'de> {
            type Value = Value;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("JSON with unique object fields")
            }

            fn visit_bool<E: de::Error>(self, boolean: bool) -> Result<Value, E> {
                Ok(Value::Bool(boolean))
            }

            fn visit_i64<E: de::Error>(self, number: i64) -> Result<Value, E> {
                Ok(Value::Number(number.into()))
            }

            fn visit_u64<E: de::Error>(self, number: u64) -> Result<Value, E> {
                Ok(Value::Number(number.into()))
            }

            fn visit_f64<E: de::Error>(self, number: f64) -> Result<Value, E> {
                Number::from_f64(number)
                    .map(Value::Number)
                    .ok_or_else(|| E::custom("JSON number is not finite"))
            }

            fn visit_str<E: de::Error>(self, string: &str) -> Result<Value, E> {
                Ok(Value::String(string.to_string()))
            }

            fn visit_string<E: de::Error>(self, string: String) -> Result<Value, E> {
                Ok(Value::String(string))
            }

            fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
                let mut json_values = Vec::new();
                while let Some(json_value) = sequence.next_element_seed(OpaqueByteArraySeed {
                    opaque_byte_arrays: self.opaque_byte_arrays,
                })? {
                    json_values.push(json_value);
                }
                Ok(Value::Array(json_values))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut json_map: A) -> Result<Value, A::Error> {
                let mut json_fields = Map::new();
                while let Some(field_name) = json_map.next_key::<String>()? {
                    let json_field = if matches!(
                        field_name.as_str(),
                        "rgba" | "rgba_bytes" | "indices" | "pixel_register_indices"
                    ) {
                        let raw_field = json_map.next_value::<&'de RawValue>()?;
                        if raw_field.get().trim_start().starts_with('[') {
                            let array_index = self.opaque_byte_arrays.len();
                            self.opaque_byte_arrays.push(raw_field);
                            Value::Array(vec![Value::Number(Number::from(array_index as u64))])
                        } else {
                            parse_json_fragment(raw_field.get(), self.opaque_byte_arrays)
                                .map_err(de::Error::custom)?
                        }
                    } else {
                        json_map.next_value_seed(OpaqueByteArraySeed {
                            opaque_byte_arrays: self.opaque_byte_arrays,
                        })?
                    };
                    if json_fields.insert(field_name.clone(), json_field).is_some() {
                        return Err(de::Error::custom(format!(
                            "duplicate JSON field {field_name}"
                        )));
                    }
                }
                Ok(Value::Object(json_fields))
            }
        }

        deserializer.deserialize_any(OpaqueByteArrayVisitor {
            opaque_byte_arrays: self.opaque_byte_arrays,
        })
    }
}

struct JsonWithOpaqueByteArrays<'a, 'de> {
    json_value: &'a Value,
    opaque_byte_arrays: &'a [&'de RawValue],
}

impl Serialize for JsonWithOpaqueByteArrays<'_, '_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.json_value {
            Value::Array(json_elements) => {
                let mut sequence = serializer.serialize_seq(Some(json_elements.len()))?;
                for json_element in json_elements {
                    sequence.serialize_element(&Self {
                        json_value: json_element,
                        opaque_byte_arrays: self.opaque_byte_arrays,
                    })?;
                }
                sequence.end()
            }
            Value::Object(json_fields) => {
                let mut json_map = serializer.serialize_map(Some(json_fields.len()))?;
                for (field_name, json_field) in json_fields {
                    let opaque_array = if matches!(
                        field_name.as_str(),
                        "rgba" | "rgba_bytes" | "indices" | "pixel_register_indices"
                    ) {
                        json_field
                            .as_array()
                            .filter(|array| array.len() == 1)
                            .and_then(|array| array[0].as_u64())
                            .and_then(|array_index| {
                                self.opaque_byte_arrays.get(array_index as usize)
                            })
                    } else {
                        None
                    };
                    if let Some(raw_array) = opaque_array {
                        json_map.serialize_entry(field_name, raw_array)?;
                    } else {
                        json_map.serialize_entry(
                            field_name,
                            &Self {
                                json_value: json_field,
                                opaque_byte_arrays: self.opaque_byte_arrays,
                            },
                        )?;
                    }
                }
                json_map.end()
            }
            json_scalar => json_scalar.serialize(serializer),
        }
    }
}

fn parse_json_fragment<'de>(
    json_text: &'de str,
    opaque_byte_arrays: &mut Vec<&'de RawValue>,
) -> Result<Value, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(json_text);
    let json_value = OpaqueByteArraySeed { opaque_byte_arrays }.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(json_value)
}

fn build_invalid_resume_error(error_detail: impl Into<String>) -> StorageError {
    StorageError::Corrupt {
        detail: error_detail.into(),
    }
}

fn get_json_object<'a>(
    json_value: &'a mut Value,
    json_path: &str,
) -> Result<&'a mut Map<String, Value>, StorageError> {
    json_value
        .as_object_mut()
        .ok_or_else(|| build_invalid_resume_error(format!("{json_path} must be an object")))
}

fn get_required_json_field<'a>(
    json_fields: &'a mut Map<String, Value>,
    field_name: &str,
    json_path: &str,
) -> Result<&'a mut Value, StorageError> {
    json_fields
        .get_mut(field_name)
        .ok_or_else(|| build_invalid_resume_error(format!("{json_path}.{field_name} is missing")))
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

fn rename_json_fields(json_fields: &mut Map<String, Value>, field_names: &[(&str, &str)]) {
    for &(previous_field_name, current_field_name) in field_names {
        rename_json_field(json_fields, previous_field_name, current_field_name);
    }
}

fn migrate_json_object_members(
    json_value: &mut Value,
    json_path: &str,
    mut migrate_child: impl FnMut(&mut Value, &str) -> Result<(), StorageError>,
) -> Result<(), StorageError> {
    for (field_name, json_child) in get_json_object(json_value, json_path)? {
        migrate_child(json_child, &format!("{json_path}.{field_name}"))?;
    }
    Ok(())
}

fn migrate_json_array_elements(
    json_value: &mut Value,
    json_path: &str,
    mut migrate_child: impl FnMut(&mut Value, &str) -> Result<(), StorageError>,
) -> Result<(), StorageError> {
    let json_children = json_value
        .as_array_mut()
        .ok_or_else(|| build_invalid_resume_error(format!("{json_path} must be an array")))?;
    for (child_index, json_child) in json_children.iter_mut().enumerate() {
        migrate_child(json_child, &format!("{json_path}[{child_index}]"))?;
    }
    Ok(())
}

pub(super) fn migrate_resume_body(
    source_resume_format: u32,
    raw_resume_body: &str,
) -> Result<ResumeBody, StorageError> {
    if !(1..RESUME_FORMAT).contains(&source_resume_format) {
        return Err(build_invalid_resume_error(format!(
            "resume body format {source_resume_format} is outside the 1 to {} range this build migrates",
            RESUME_FORMAT - 1
        )));
    }
    let root_fields = parse_unique_json_object(raw_resume_body, "body")?;
    let raw_sessions = root_fields
        .get("sessions")
        .ok_or_else(|| build_invalid_resume_error("body.sessions is missing"))?;
    let sessions = parse_unique_json_object(raw_sessions.get(), "body.sessions")?;
    let raw_terminal_engines = root_fields
        .get("engines")
        .ok_or_else(|| build_invalid_resume_error("body.engines is missing"))?;
    let terminal_engines = parse_json_object_fields(raw_terminal_engines.get(), "body.engines")?;
    let mut ancillary_pane_fields = BTreeMap::new();
    let mut repeated_pane_keys = terminal_engines.repeated_json_field_names;
    for field_name in [
        "undecoded",
        "graphics_events",
        "graphics_transport",
        "synchronized_output",
    ] {
        if let Some(raw_fields) = root_fields.get(field_name) {
            let ancillary_fields_for_panes =
                parse_json_object_fields(raw_fields.get(), field_name)?;
            repeated_pane_keys.extend(ancillary_fields_for_panes.repeated_json_field_names);
            ancillary_pane_fields.insert(field_name, ancillary_fields_for_panes.json_fields);
        }
    }
    let carried_quit = root_fields
        .get("quit")
        .map(|raw_quit| serde_json::from_str::<Option<CarriedQuit>>(raw_quit.get()))
        .transpose()
        .map_err(|parse_error| {
            build_invalid_resume_error(format!("resume body is unreadable: {parse_error}"))
        })?
        .flatten();

    let mut session_by_id = HashMap::with_capacity(sessions.len());
    for (session_key, raw_session) in sessions {
        let mut opaque_byte_arrays = Vec::new();
        let session = parse_json_fragment(raw_session.get(), &mut opaque_byte_arrays).map_err(
            |parse_error| {
                build_invalid_resume_error(format!("resume body is unreadable: {parse_error}"))
            },
        )?;
        let mut single_session_by_id = Map::new();
        single_session_by_id.insert(session_key.clone(), session);
        let mut body_fields = Map::new();
        body_fields.insert("sessions".to_string(), Value::Object(single_session_by_id));
        body_fields.insert("engines".to_string(), Value::Object(Map::new()));
        let mut single_session_body = Value::Object(body_fields);
        apply_resume_migrations(source_resume_format, &mut single_session_body)?;
        let migrated_session = get_required_json_field(
            get_json_object(&mut single_session_body, "body")?,
            "session_by_id",
            "body",
        )?
        .get(&session_key)
        .ok_or_else(|| {
            build_invalid_resume_error(format!("body.session_by_id.{session_key} is missing"))
        })?;
        let session_id = serde_json::from_value::<SessionId>(Value::String(session_key.clone()))
            .map_err(|parse_error| {
                build_invalid_resume_error(format!(
                    "resume body has invalid session id {session_key}: {parse_error}"
                ))
            })?;
        let mut migrated_session_bytes = Vec::new();
        write_migrated_json(
            &mut migrated_session_bytes,
            &JsonWithOpaqueByteArrays {
                json_value: migrated_session,
                opaque_byte_arrays: &opaque_byte_arrays,
            },
        )?;
        let migrated_session: Session =
            serde_json::from_slice(&migrated_session_bytes).map_err(|parse_error| {
                build_invalid_resume_error(format!(
                    "migrated session {session_id} is unreadable: {parse_error}"
                ))
            })?;
        if session_by_id.insert(session_id, migrated_session).is_some() {
            return Err(build_invalid_resume_error(format!(
                "resume body has duplicate session id {session_id}"
            )));
        }
    }

    let mut seen_pane_ids = HashSet::new();
    let mut repeated_pane_ids = HashSet::new();
    for pane_key in terminal_engines.json_fields.keys() {
        if let Ok(pane_id) = serde_json::from_value::<PaneId>(Value::String(pane_key.clone())) {
            if !seen_pane_ids.insert(pane_id) {
                repeated_pane_ids.insert(pane_id);
            }
        }
    }
    for pane_key in repeated_pane_keys {
        if let Ok(pane_id) = serde_json::from_value::<PaneId>(Value::String(pane_key)) {
            repeated_pane_ids.insert(pane_id);
        }
    }
    let mut carried_pane_state_by_pane_id =
        HashMap::with_capacity(terminal_engines.json_fields.len());
    for (pane_key, raw_terminal_engine) in terminal_engines.json_fields {
        let pane_id = match serde_json::from_value::<PaneId>(Value::String(pane_key.clone())) {
            Ok(pane_id) => pane_id,
            Err(_) => {
                tracing::warn!(
                    pane_key = %pane_key,
                    "a carried pane state is keyed by no pane id or by one named twice; that pane comes back with a blank screen"
                );
                continue;
            }
        };
        if repeated_pane_ids.contains(&pane_id) {
            tracing::warn!(
                pane_key = %pane_key,
                "a carried pane state is keyed by no pane id or by one named twice; that pane comes back with a blank screen"
            );
            continue;
        }
        match migrate_previous_pane_state(
            source_resume_format,
            &pane_key,
            raw_terminal_engine,
            &ancillary_pane_fields,
        ) {
            Ok(carried_pane_state) => {
                carried_pane_state_by_pane_id.insert(pane_id, carried_pane_state);
            }
            Err(migration_error) => tracing::warn!(
                %pane_id,
                %migration_error,
                "a carried pane state could not be read; that pane comes back with a blank screen"
            ),
        }
    }
    Ok(ResumeBody {
        session_by_id,
        carried_pane_state_by_pane_id,
        carried_quit,
    })
}

fn migrate_previous_pane_state(
    source_resume_format: u32,
    pane_key: &str,
    raw_terminal_engine: &RawValue,
    ancillary_pane_fields: &BTreeMap<&str, BTreeMap<String, &RawValue>>,
) -> Result<CarriedPaneState, StorageError> {
    let mut opaque_byte_arrays = Vec::new();
    let terminal_engine = parse_json_fragment(raw_terminal_engine.get(), &mut opaque_byte_arrays)
        .map_err(|parse_error| {
        build_invalid_resume_error(format!("pane {pane_key} is unreadable: {parse_error}"))
    })?;
    let mut single_engine_by_pane_id = Map::new();
    single_engine_by_pane_id.insert(pane_key.to_string(), terminal_engine);
    let mut body_fields = Map::new();
    body_fields.insert("sessions".to_string(), Value::Object(Map::new()));
    body_fields.insert(
        "engines".to_string(),
        Value::Object(single_engine_by_pane_id),
    );
    for (field_name, pane_fields) in ancillary_pane_fields {
        if let Some(raw_pane_field) = pane_fields.get(pane_key) {
            let pane_field = parse_json_fragment(raw_pane_field.get(), &mut opaque_byte_arrays)
                .map_err(|parse_error| {
                    build_invalid_resume_error(format!(
                        "pane {pane_key} {field_name} is unreadable: {parse_error}"
                    ))
                })?;
            let mut single_pane_field = Map::new();
            single_pane_field.insert(pane_key.to_string(), pane_field);
            body_fields.insert((*field_name).to_string(), Value::Object(single_pane_field));
        }
    }
    let mut single_pane_body = Value::Object(body_fields);
    apply_resume_migrations(source_resume_format, &mut single_pane_body)?;
    let migrated_pane = get_required_json_field(
        get_json_object(&mut single_pane_body, "body")?,
        "carried_pane_state_by_pane_id",
        "body",
    )?
    .get(pane_key)
    .ok_or_else(|| {
        build_invalid_resume_error(format!(
            "body.carried_pane_state_by_pane_id.{pane_key} is missing"
        ))
    })?;
    let mut migrated_pane_bytes = Vec::new();
    write_migrated_json(
        &mut migrated_pane_bytes,
        &JsonWithOpaqueByteArrays {
            json_value: migrated_pane,
            opaque_byte_arrays: &opaque_byte_arrays,
        },
    )?;
    serde_json::from_slice(&migrated_pane_bytes).map_err(|parse_error| {
        build_invalid_resume_error(format!(
            "migrated pane {pane_key} is unreadable: {parse_error}"
        ))
    })
}

fn parse_json_object_fields<'a>(
    json_text: &'a str,
    json_path: &str,
) -> Result<ParsedJsonObject<'a>, StorageError> {
    serde_json::from_str(json_text).map_err(|parse_error| {
        build_invalid_resume_error(format!(
            "resume body is unreadable at {json_path}: {parse_error}"
        ))
    })
}

fn parse_unique_json_object<'a>(
    json_text: &'a str,
    json_path: &str,
) -> Result<BTreeMap<String, &'a RawValue>, StorageError> {
    let parsed_json_object = parse_json_object_fields(json_text, json_path)?;
    if let Some(field_name) = parsed_json_object.repeated_json_field_names.first() {
        return Err(build_invalid_resume_error(format!(
            "resume body has duplicate field {json_path}.{field_name}"
        )));
    }
    Ok(parsed_json_object.json_fields)
}

fn write_migrated_json(
    migrated_body_bytes: &mut Vec<u8>,
    json_value: &impl Serialize,
) -> Result<(), StorageError> {
    serde_json::to_writer(migrated_body_bytes, json_value).map_err(|encode_error| {
        build_invalid_resume_error(format!("encode migrated resume body: {encode_error}"))
    })
}

fn apply_resume_migrations(
    source_resume_format: u32,
    resume_body: &mut Value,
) -> Result<(), StorageError> {
    for migration_format in source_resume_format..RESUME_FORMAT {
        match migration_format {
            1 => migrate_resume_one_to_two(resume_body)?,
            2 => migrate_resume_two_to_three(resume_body)?,
            3 => migrate_resume_three_to_four(resume_body)?,
            _ => {
                return Err(build_invalid_resume_error(format!(
                    "no resume migration from format {migration_format}"
                )))
            }
        }
    }
    Ok(())
}

fn migrate_resume_one_to_two(resume_body: &mut Value) -> Result<(), StorageError> {
    let sessions =
        get_required_json_field(get_json_object(resume_body, "body")?, "sessions", "body")?;
    migrate_json_object_members(sessions, "body.sessions", |session, session_path| {
        let clients = get_required_json_field(
            get_json_object(session, session_path)?,
            "clients",
            session_path,
        )?;
        let client_records_by_id =
            get_required_json_field(get_json_object(clients, "clients")?, "records", "clients")?;
        migrate_json_object_members(
            client_records_by_id,
            "clients.records",
            |client, client_path| {
                get_json_object(client, client_path)?.remove("tier");
                Ok(())
            },
        )
    })
}

fn migrate_resume_two_to_three(resume_body: &mut Value) -> Result<(), StorageError> {
    let default_terminal = serde_json::to_value(TerminalState::from_pty_size(PtySize {
        row_count: 1,
        column_count: 1,
    }))
    .map_err(|encode_error| {
        build_invalid_resume_error(format!("encode terminal defaults: {encode_error}"))
    })?;
    let default_terminal_fields = default_terminal
        .as_object()
        .ok_or_else(|| build_invalid_resume_error("terminal defaults must be an object"))?;
    let body_fields = get_json_object(resume_body, "body")?;
    let sessions = get_required_json_field(body_fields, "sessions", "body")?;
    migrate_json_object_members(sessions, "body.sessions", |session, session_path| {
        let tabs = get_required_json_field(
            get_json_object(session, session_path)?,
            "tabs",
            session_path,
        )?;
        migrate_json_object_members(tabs, "session.tabs", |tab, tab_path| {
            let layout =
                get_required_json_field(get_json_object(tab, tab_path)?, "layout", tab_path)?;
            unwrap_layout_children(layout)
        })
    })?;
    let terminal_engines = get_required_json_field(body_fields, "engines", "body")?;
    migrate_json_object_members(
        terminal_engines,
        "body.engines",
        |terminal_engine, engine_path| {
            let engine_fields = get_json_object(terminal_engine, engine_path)?;
            for field_name in [
                "cell_size",
                "primary_image_placements",
                "primary_image_history",
                "alternate_image_placements",
                "kitty_images",
                "image_contents",
                "next_image_content_id",
                "next_image_placement_id",
                "sixel_palette",
                "shell_integration_state",
                "shell_integration_facts",
            ] {
                if !engine_fields.contains_key(field_name) {
                    engine_fields.insert(
                        field_name.to_string(),
                        default_terminal_fields
                            .get(field_name)
                            .expect("terminal default is serialized")
                            .clone(),
                    );
                }
            }
            engine_fields
                .entry("native_image_coverage")
                .or_insert(Value::Bool(false));
            let modes = get_json_object(
                get_required_json_field(engine_fields, "modes", engine_path)?,
                "terminal.modes",
            )?;
            for mode_name in [
                "sixel_scrolling",
                "sixel_private_color_registers",
                "sixel_cursor_right",
            ] {
                modes.entry(mode_name).or_insert(Value::Bool(false));
            }
            for screen_name in ["primary", "alternate"] {
                let screen = get_required_json_field(engine_fields, screen_name, engine_path)?;
                let screen_fields = get_json_object(screen, screen_name)?;
                if let Some(row_ends) = screen_fields.remove("row_ends") {
                    let ends = row_ends.as_array().ok_or_else(|| {
                        build_invalid_resume_error(format!(
                            "{screen_name}.row_ends must be an array"
                        ))
                    })?;
                    let rows = get_required_json_field(screen_fields, "rows", screen_name)?
                        .as_array()
                        .ok_or_else(|| {
                            build_invalid_resume_error(format!(
                                "{screen_name}.rows must be an array"
                            ))
                        })?;
                    if ends.len() != rows.len() {
                        return Err(build_invalid_resume_error(format!(
                            "{screen_name} has {} row ends for {} rows",
                            ends.len(),
                            rows.len()
                        )));
                    }
                    screen_fields.insert(
                        "row_meta".to_string(),
                        Value::Array(
                            ends.iter()
                                .map(|row_end| serde_json::json!({"end": row_end, "prompt": false}))
                                .collect(),
                        ),
                    );
                }
                if let Some(row_metadata) = screen_fields.get_mut("row_meta") {
                    migrate_json_array_elements(row_metadata, "row_meta", |row, row_path| {
                        get_json_object(row, row_path)?
                            .entry("prompt")
                            .or_insert(Value::Bool(false));
                        Ok(())
                    })?;
                }
            }
            let scrollback = get_required_json_field(engine_fields, "scrollback", engine_path)?;
            let retained_lines = get_required_json_field(
                get_json_object(scrollback, "scrollback")?,
                "lines",
                "scrollback",
            )?;
            migrate_json_array_elements(retained_lines, "scrollback.lines", |line, line_path| {
                let scrollback_line_parts = line.as_array_mut().ok_or_else(|| {
                    build_invalid_resume_error(format!("{line_path} must be an array"))
                })?;
                if scrollback_line_parts.len() != 2 {
                    return Err(build_invalid_resume_error(format!(
                        "{line_path} must have cells and row end"
                    )));
                }
                if scrollback_line_parts[1].is_string() {
                    scrollback_line_parts[1] =
                        serde_json::json!({"end": scrollback_line_parts[1], "prompt": false});
                }
                Ok(())
            })?;
            Ok(())
        },
    )
}

fn unwrap_layout_children(layout: &mut Value) -> Result<(), StorageError> {
    let layout_fields = get_json_object(layout, "layout")?;
    if let Some(split) = layout_fields.get_mut("Split") {
        let layout_children = get_required_json_field(
            get_json_object(split, "layout.Split")?,
            "children",
            "layout.Split",
        )?;
        migrate_json_array_elements(
            layout_children,
            "layout.Split.children",
            |json_child, child_path| {
                let wrapper = get_json_object(json_child, child_path)?;
                if let Some(node) = wrapper.remove("node") {
                    *json_child = node;
                }
                unwrap_layout_children(json_child)
            },
        )?;
    }
    Ok(())
}

fn migrate_resume_three_to_four(resume_body: &mut Value) -> Result<(), StorageError> {
    let body_fields = get_json_object(resume_body, "body")?;
    rename_json_fields(
        body_fields,
        &[("sessions", "session_by_id"), ("quit", "carried_quit")],
    );
    let sessions = get_required_json_field(body_fields, "session_by_id", "body")?;
    migrate_json_object_members(sessions, "body.session_by_id", migrate_session)?;

    let terminal_engines = body_fields
        .remove("engines")
        .ok_or_else(|| build_invalid_resume_error("body.engines is missing"))?;
    let mut pane_states = Map::new();
    let Value::Object(terminal_engines) = terminal_engines else {
        return Err(build_invalid_resume_error("body.engines must be an object"));
    };
    for (pane_id, mut terminal_state) in terminal_engines {
        migrate_terminal_state(&mut terminal_state, &format!("body.engines.{pane_id}"))?;
        let mut carried_pane_state = Map::new();
        carried_pane_state.insert("terminal_state".to_string(), terminal_state);
        for (previous_field_name, current_field_name) in [
            ("undecoded", "undecoded_bytes"),
            ("graphics_events", "graphics_events"),
            ("graphics_transport", "graphics_transport"),
            ("synchronized_output", "synchronized_output"),
        ] {
            let mut pane_field = body_fields
                .get(previous_field_name)
                .and_then(Value::as_object)
                .and_then(|pane_fields| pane_fields.get(&pane_id))
                .cloned()
                .unwrap_or_else(|| match previous_field_name {
                    "undecoded" | "graphics_events" => Value::Array(Vec::new()),
                    _ => Value::Null,
                });
            if previous_field_name == "graphics_transport" && !pane_field.is_null() {
                migrate_graphics_transport(&mut pane_field, 0)?;
            }
            if previous_field_name == "graphics_events" {
                migrate_graphics_events(&mut pane_field)?;
            }
            if previous_field_name == "synchronized_output" && !pane_field.is_null() {
                migrate_synchronized_output(&mut pane_field)?;
            }
            carried_pane_state.insert(current_field_name.to_string(), pane_field);
        }
        pane_states.insert(pane_id, Value::Object(carried_pane_state));
    }
    body_fields.insert(
        "carried_pane_state_by_pane_id".to_string(),
        Value::Object(pane_states),
    );
    for previous_field_name in [
        "undecoded",
        "graphics_undecoded",
        "graphics_screen_continuation",
        "graphics_screen_wrapper_active",
        "graphics_tmux_continuation",
        "graphics_tmux_wrapper_active",
        "graphics_events",
        "graphics_transport",
        "synchronized_output",
    ] {
        body_fields.remove(previous_field_name);
    }
    Ok(())
}

fn migrate_session(session: &mut Value, json_path: &str) -> Result<(), StorageError> {
    let json_fields = get_json_object(session, json_path)?;
    rename_json_fields(
        json_fields,
        &[
            ("id", "session_id"),
            ("name", "session_name"),
            ("start_locked", "should_start_locked"),
        ],
    );
    let tabs = get_required_json_field(json_fields, "tabs", json_path)?;
    migrate_json_object_members(tabs, "session.tabs", migrate_tab)?;
    let panes = get_required_json_field(json_fields, "panes", json_path)?;
    let pane_registry = get_json_object(panes, "session.panes")?;
    rename_json_field(pane_registry, "records", "pane_record_by_id");
    let pane_records_by_id =
        get_required_json_field(pane_registry, "pane_record_by_id", "session.panes")?;
    migrate_json_object_members(
        pane_records_by_id,
        "session.panes.pane_record_by_id",
        migrate_pane,
    )?;
    let clients = get_required_json_field(json_fields, "clients", json_path)?;
    let client_registry = get_json_object(clients, "session.clients")?;
    rename_json_field(client_registry, "records", "client_by_id");
    let client_records_by_id =
        get_required_json_field(client_registry, "client_by_id", "session.clients")?;
    migrate_json_object_members(
        client_records_by_id,
        "session.clients.client_by_id",
        migrate_client,
    )?;
    Ok(())
}

fn migrate_tab(tab: &mut Value, json_path: &str) -> Result<(), StorageError> {
    let json_fields = get_json_object(tab, json_path)?;
    rename_json_fields(
        json_fields,
        &[
            ("id", "tab_id"),
            ("name", "tab_name"),
            ("index", "tab_index"),
        ],
    );
    json_fields.remove("lifecycle");
    migrate_layout(get_required_json_field(json_fields, "layout", json_path)?)
}

fn migrate_layout(layout: &mut Value) -> Result<(), StorageError> {
    let json_fields = get_json_object(layout, "layout")?;
    if let Some(split) = json_fields.get_mut("Split") {
        let split_fields = get_json_object(split, "layout.Split")?;
        rename_json_field(split_fields, "active", "active_child_index");
        let weights = get_required_json_field(split_fields, "weights", "layout.Split")?;
        migrate_json_array_elements(weights, "layout.Split.weights", |weight, weight_path| {
            rename_json_fields(
                get_json_object(weight, weight_path)?,
                &[
                    ("primary", "primary_constraint"),
                    ("min", "minimum_cell_count"),
                    ("preferred", "preferred_cell_count"),
                ],
            );
            Ok(())
        })?;
        let layout_children = get_required_json_field(split_fields, "children", "layout.Split")?;
        migrate_json_array_elements(layout_children, "layout.Split.children", |json_child, _| {
            migrate_layout(json_child)
        })?;
    }
    Ok(())
}

fn migrate_pane(pane: &mut Value, json_path: &str) -> Result<(), StorageError> {
    let json_fields = get_json_object(pane, json_path)?;
    rename_json_fields(
        json_fields,
        &[
            ("id", "pane_id"),
            ("command", "spawn_spec"),
            ("cwd", "working_directory"),
        ],
    );
    json_fields.remove("kind");
    json_fields.remove("exit_policy");
    json_fields.remove("created_at");
    if let Some(spawn_spec) = json_fields
        .get_mut("spawn_spec")
        .filter(|spawn_spec| !spawn_spec.is_null())
    {
        rename_json_fields(
            get_json_object(spawn_spec, "pane.spawn_spec")?,
            &[
                ("args", "arguments"),
                ("cwd", "working_directory"),
                ("env", "environment_variables"),
            ],
        );
    }
    if let Some(close_policy) = json_fields.get_mut("close_policy") {
        let policy_fields = get_json_object(close_policy, "pane.close_policy")?;
        if let Some(graceful) = policy_fields.get_mut("Graceful") {
            rename_json_field(
                get_json_object(graceful, "pane.close_policy.Graceful")?,
                "timeout",
                "timeout_duration",
            );
        }
    }
    Ok(())
}

fn migrate_client(client: &mut Value, json_path: &str) -> Result<(), StorageError> {
    let json_fields = get_json_object(client, json_path)?;
    rename_json_fields(
        json_fields,
        &[
            ("id", "client_id"),
            ("viewport", "viewport_size"),
            ("active_tab", "active_tab_id"),
            ("colour", "color_index"),
            ("focus_by_tab", "focused_pane_id_by_tab_id"),
            ("mouse_select", "is_mouse_selection_enabled"),
            ("scroll_by_pane", "scroll_offset_by_pane_id"),
            ("selection_by_pane", "selection_by_pane_id"),
            ("zoom_by_tab", "zoomed_pane_id_by_tab_id"),
        ],
    );
    json_fields.entry("cell_size").or_insert(Value::Null);
    json_fields.entry("pane_area").or_insert(Value::Null);
    migrate_size(get_required_json_field(
        json_fields,
        "viewport_size",
        json_path,
    )?)?;
    if let Some(pane_area) = json_fields
        .get_mut("pane_area")
        .filter(|pane_area| !pane_area.is_null())
    {
        migrate_pane_area(pane_area)?;
    }
    let selections = get_required_json_field(json_fields, "selection_by_pane_id", json_path)?;
    migrate_json_object_members(
        selections,
        "client.selection_by_pane_id",
        |selection, selection_path| {
            let selection_fields = get_json_object(selection, selection_path)?;
            rename_json_field(selection_fields, "kind", "selection_kind");
            for coordinate_name in ["anchor", "cursor"] {
                rename_json_fields(
                    get_json_object(
                        get_required_json_field(selection_fields, coordinate_name, selection_path)?,
                        coordinate_name,
                    )?,
                    &[("row", "row_index"), ("col", "column_index")],
                );
            }
            Ok(())
        },
    )
}

fn migrate_size(size: &mut Value) -> Result<(), StorageError> {
    rename_json_fields(
        get_json_object(size, "size")?,
        &[("cols", "column_count"), ("rows", "row_count")],
    );
    Ok(())
}

fn migrate_pane_area(pane_area: &mut Value) -> Result<(), StorageError> {
    if pane_area == "Starving" {
        return Ok(());
    }
    let pane_area_variants = get_json_object(pane_area, "client.pane_area")?;
    if pane_area_variants.len() != 1 || !pane_area_variants.contains_key("Reported") {
        return Err(build_invalid_resume_error(
            "client.pane_area must be Reported or Starving",
        ));
    }
    migrate_size(get_required_json_field(
        pane_area_variants,
        "Reported",
        "client.pane_area",
    )?)
}

fn migrate_terminal_state(terminal_state: &mut Value, json_path: &str) -> Result<(), StorageError> {
    let json_fields = get_json_object(terminal_state, json_path)?;
    if !matches!(
        json_fields.remove("native_image_coverage"),
        Some(Value::Bool(_))
    ) {
        return Err(build_invalid_resume_error(format!(
            "{json_path}.native_image_coverage must be a boolean"
        )));
    }
    rename_json_fields(
        json_fields,
        &[
            ("active", "active_screen"),
            ("reported_cwd", "reported_working_directory"),
            ("replies", "device_query_replies"),
        ],
    );
    for screen_name in ["primary", "alternate"] {
        migrate_grid(get_required_json_field(
            json_fields,
            screen_name,
            json_path,
        )?)?;
        migrate_cursor(get_required_json_field(
            json_fields,
            &format!("{screen_name}_cursor"),
            json_path,
        )?)?;
        migrate_render(get_required_json_field(
            json_fields,
            &format!("{screen_name}_render"),
            json_path,
        )?)?;
        json_fields.insert(format!("{screen_name}_horizontal_margins"), Value::Null);
        json_fields.insert(
            format!("{screen_name}_keyboard_stack"),
            Value::Array(Vec::new()),
        );
    }
    let modes = get_json_object(
        get_required_json_field(json_fields, "modes", json_path)?,
        "terminal.modes",
    )?;
    rename_json_fields(
        modes,
        &[
            ("bracketed_paste", "is_bracketed_paste_enabled"),
            ("alt_scroll", "is_alternate_scroll_enabled"),
            ("autowrap", "is_autowrap_enabled"),
            ("app_cursor_keys", "is_application_cursor_keys_enabled"),
            ("reverse_video", "is_reverse_video_enabled"),
            ("cursor_blink", "is_cursor_blink_enabled"),
            ("sixel_scrolling", "is_sixel_scrolling_enabled"),
            (
                "sixel_private_color_registers",
                "is_sixel_private_color_registers_enabled",
            ),
            ("sixel_cursor_right", "is_sixel_cursor_right_enabled"),
        ],
    );
    modes.insert(
        "is_left_right_margin_mode_enabled".to_string(),
        Value::Bool(false),
    );
    migrate_scrollback(get_required_json_field(
        json_fields,
        "scrollback",
        json_path,
    )?)?;
    migrate_terminal_images(json_fields)?;
    Ok(())
}

fn migrate_grid(grid: &mut Value) -> Result<(), StorageError> {
    let json_fields = get_json_object(grid, "grid")?;
    rename_json_field(json_fields, "row_meta", "row_metadata");
    let rows = get_required_json_field(json_fields, "rows", "grid")?;
    migrate_json_array_elements(rows, "grid.rows", migrate_cells)?;
    let metadata = get_required_json_field(json_fields, "row_metadata", "grid")?;
    migrate_json_array_elements(metadata, "grid.row_metadata", migrate_row_metadata)
}

fn migrate_cells(cells: &mut Value, json_path: &str) -> Result<(), StorageError> {
    migrate_json_array_elements(cells, json_path, |cell, cell_path| {
        let cell_fields = get_json_object(cell, cell_path)?;
        rename_json_fields(
            cell_fields,
            &[("ch", "character"), ("width", "display_width")],
        );
        migrate_style(get_required_json_field(cell_fields, "style", cell_path)?)
    })
}

fn migrate_row_metadata(metadata: &mut Value, json_path: &str) -> Result<(), StorageError> {
    rename_json_fields(
        get_json_object(metadata, json_path)?,
        &[("end", "row_end"), ("prompt", "has_prompt_mark")],
    );
    Ok(())
}

fn migrate_cursor(cursor: &mut Value) -> Result<(), StorageError> {
    let json_fields = get_json_object(cursor, "cursor")?;
    rename_json_fields(
        json_fields,
        &[("col", "column"), ("pending_wrap", "is_wrap_pending")],
    );
    json_fields.insert("is_origin_mode_enabled".to_string(), Value::Bool(false));
    if let Some(saved) = json_fields
        .get_mut("saved")
        .filter(|saved| !saved.is_null())
    {
        let saved_fields = get_json_object(saved, "cursor.saved")?;
        rename_json_field(saved_fields, "col", "column");
    }
    Ok(())
}

fn migrate_render(render: &mut Value) -> Result<(), StorageError> {
    let json_fields = get_json_object(render, "render")?;
    migrate_style(get_required_json_field(json_fields, "style", "render")?)
}

fn migrate_style(style: &mut Value) -> Result<(), StorageError> {
    rename_json_fields(
        get_json_object(style, "style")?,
        &[
            ("fg", "foreground_color"),
            ("bg", "background_color"),
            ("attrs", "attributes"),
        ],
    );
    Ok(())
}

fn migrate_scrollback(scrollback: &mut Value) -> Result<(), StorageError> {
    let json_fields = get_json_object(scrollback, "scrollback")?;
    rename_json_fields(
        json_fields,
        &[
            ("lines", "retained_lines"),
            ("max_lines", "maximum_line_count"),
            ("max_bytes", "maximum_byte_count"),
            ("byte_total", "retained_byte_count"),
            ("total_pushed", "total_pushed_line_count"),
        ],
    );
    json_fields.remove("dropped_lines");
    json_fields.remove("dropped_bytes");
    let retained_lines = get_required_json_field(json_fields, "retained_lines", "scrollback")?;
    migrate_json_array_elements(
        retained_lines,
        "scrollback.retained_lines",
        |line, line_path| {
            let scrollback_line_parts = line.as_array_mut().ok_or_else(|| {
                build_invalid_resume_error(format!("{line_path} must be an array"))
            })?;
            if scrollback_line_parts.len() != 2 {
                return Err(build_invalid_resume_error(format!(
                    "{line_path} must have cells and row metadata"
                )));
            }
            migrate_cells(&mut scrollback_line_parts[0], line_path)?;
            migrate_row_metadata(&mut scrollback_line_parts[1], line_path)
        },
    )?;
    Ok(())
}

fn migrate_graphics_transport(
    transport: &mut Value,
    wrapper_depth: usize,
) -> Result<(), StorageError> {
    if wrapper_depth > 16 {
        return Err(build_invalid_resume_error(
            "graphics wrapper nesting exceeds the supported limit",
        ));
    }
    let json_fields = get_json_object(transport, "graphics_transport")?;
    rename_json_fields(
        json_fields,
        &[
            ("carry", "carry_bytes"),
            ("carryable", "is_carryable"),
            ("abandonment", "graphics_abandonment"),
            ("screen_continuation", "is_screen_continuation"),
            ("screen_inner", "screen_inner_transport"),
            ("tmux_continuation", "is_tmux_continuation"),
            ("tmux_inner", "tmux_inner_transport"),
        ],
    );
    json_fields.remove("screen_wrapper_active");
    json_fields.remove("tmux_wrapper_active");
    for child_name in ["screen_inner_transport", "tmux_inner_transport"] {
        if let Some(json_child) = json_fields
            .get_mut(child_name)
            .filter(|json_child| !json_child.is_null())
        {
            migrate_graphics_transport(json_child, wrapper_depth + 1)?;
        }
    }
    Ok(())
}

fn migrate_synchronized_output(transport: &mut Value) -> Result<(), StorageError> {
    let json_fields = get_json_object(transport, "synchronized_output")?;
    rename_json_field(json_fields, "bytes", "normalized_bytes");
    let terminal_input =
        get_required_json_field(json_fields, "terminal_input", "synchronized_output")?;
    rename_json_fields(
        get_json_object(terminal_input, "terminal_input")?,
        &[
            ("state", "input_state"),
            ("utf8_continuations", "remaining_utf8_continuation_count"),
            ("tail", "trailing_bytes"),
            ("tail_len", "trailing_byte_count"),
            ("tail_next", "trailing_start_index"),
        ],
    );
    Ok(())
}

fn migrate_graphics_events(graphics_events: &mut Value) -> Result<(), StorageError> {
    migrate_json_array_elements(
        graphics_events,
        "graphics_events",
        |graphics_event, event_path| {
            let variants = get_json_object(graphics_event, event_path)?;
            if let Some(image_record) = variants.get_mut("Ok") {
                migrate_image_record(image_record, true)?;
            }
            if let Some(graphics_error) = variants.get_mut("Err") {
                migrate_graphics_error(graphics_error)?;
            }
            Ok(())
        },
    )
}

fn migrate_graphics_error(graphics_error: &mut Value) -> Result<(), StorageError> {
    let variants = get_json_object(graphics_error, "graphics_error")?;
    if let Some(placement_rejected) = variants.get_mut("PlacementRejected") {
        let rejection_fields =
            get_json_object(placement_rejected, "graphics_error.PlacementRejected")?;
        rename_json_field(rejection_fields, "reason", "placement_error");
        migrate_image_placement_error(get_required_json_field(
            rejection_fields,
            "placement_error",
            "graphics_error.PlacementRejected",
        )?)?;
    }
    if let Some(queue_full) = variants.get_mut("QueueFull") {
        rename_json_field(
            get_json_object(queue_full, "graphics_error.QueueFull")?,
            "dropped",
            "dropped_event_count",
        );
    }
    if let Some(unsupported_media) = variants.get_mut("UnsupportedMedia") {
        rename_json_field(
            get_json_object(unsupported_media, "graphics_error.UnsupportedMedia")?,
            "format",
            "media_format",
        );
    }
    if let Some(size_mismatch) = variants.get_mut("DeclaredSizeMismatch") {
        rename_json_fields(
            get_json_object(size_mismatch, "graphics_error.DeclaredSizeMismatch")?,
            &[
                ("expected", "expected_byte_count"),
                ("actual", "actual_byte_count"),
            ],
        );
    }
    Ok(())
}

fn migrate_image_placement_error(placement_error: &mut Value) -> Result<(), StorageError> {
    if let Value::String(variant_name) = placement_error {
        *variant_name = match variant_name.as_str() {
            "NoParent" => "ParentNotFound",
            "AnimationDataInvalid" => "InvalidAnimationData",
            other_variant => other_variant,
        }
        .to_string();
        return Ok(());
    }
    let error_variants = get_json_object(placement_error, "image_placement_error")?;
    if error_variants.len() != 1 {
        return Err(build_invalid_resume_error(
            "image_placement_error must have one variant",
        ));
    }
    let (variant_name, variant_fields) = error_variants
        .iter_mut()
        .next()
        .expect("one image placement error variant");
    let field_names: &[(&str, &str)] = match variant_name.as_str() {
        "RelativeOffsetOutOfBounds" => &[("row", "resolved_row"), ("column", "resolved_column")],
        "AnimationFrameNotFound" => &[("frame", "frame_index")],
        "ImageNotFound" => &[("id", "image_id"), ("number", "image_number")],
        "MissingCellDimensions" | "UnsupportedCellDimensions" => {
            &[("width", "requested_width"), ("height", "requested_height")]
        }
        "ZeroSize" | "DimensionsTooLarge" => &[("columns", "column_count"), ("rows", "row_count")],
        "SourceOutOfBounds" => &[
            ("x", "source_x"),
            ("y", "source_y"),
            ("width", "source_pixel_width"),
            ("height", "source_pixel_height"),
            ("image_width", "image_pixel_width"),
            ("image_height", "image_pixel_height"),
        ],
        "OutOfBounds" | "HistoryOutOfBounds" => &[
            ("row", "anchor_row"),
            ("column", "anchor_column"),
            ("columns", "column_count"),
            ("rows", "row_count"),
        ],
        "HistoryWidthOutOfBounds" => &[
            ("row", "anchor_row"),
            ("column", "anchor_column"),
            ("columns", "column_count"),
        ],
        "HistoryRangeOverflow" => &[("total_pushed", "total_pushed_row_count")],
        "HistoryRowsExceedCounter" => &[
            ("retained_rows", "retained_row_count"),
            ("total_pushed", "total_pushed_row_count"),
        ],
        "TooManyPlacements" => &[("count", "placement_count"), ("limit", "placement_limit")],
        "StorageLimit" => &[
            ("used_bytes", "used_byte_count"),
            ("requested_bytes", "requested_byte_count"),
            ("limit_bytes", "byte_limit"),
        ],
        _ => {
            return Err(build_invalid_resume_error(format!(
                "unknown image placement error {variant_name}"
            )))
        }
    };
    rename_json_fields(
        get_json_object(variant_fields, "image_placement_error variant")?,
        field_names,
    );
    Ok(())
}

fn migrate_image_record(image_record: &mut Value, has_image: bool) -> Result<(), StorageError> {
    let json_fields = get_json_object(image_record, "image_record")?;
    migrate_image_display(get_required_json_field(
        json_fields,
        "display",
        "image_record",
    )?)?;
    if has_image {
        migrate_decoded_image(get_required_json_field(
            json_fields,
            "image",
            "image_record",
        )?)?;
        if let Some(animation) = json_fields
            .get_mut("animation")
            .filter(|animation| !animation.is_null())
        {
            migrate_animation(animation)?;
        }
    } else {
        for previous_field_name in ["image", "animation"] {
            if json_fields
                .get(previous_field_name)
                .is_some_and(|field| !field.is_null())
            {
                return Err(build_invalid_resume_error(format!(
                    "stored image record has inline {previous_field_name}"
                )));
            }
            json_fields.remove(previous_field_name);
        }
    }
    Ok(())
}

fn migrate_image_display(display: &mut Value) -> Result<(), StorageError> {
    rename_json_fields(
        get_json_object(display, "image_display")?,
        &[
            ("width", "requested_width"),
            ("height", "requested_height"),
            ("preserve_aspect_ratio", "is_aspect_ratio_preserved"),
            ("unicode_placeholder", "is_unicode_placeholder"),
            ("relative_offset_x", "relative_column_offset"),
            ("relative_offset_y", "relative_row_offset"),
            ("cell_columns", "requested_column_count"),
            ("cell_rows", "requested_row_count"),
            ("source_offset_x", "source_pixel_offset_x"),
            ("source_offset_y", "source_pixel_offset_y"),
            ("cell_offset_x", "cell_pixel_offset_x"),
            ("cell_offset_y", "cell_pixel_offset_y"),
            ("move_cursor", "should_move_cursor"),
            ("quiet", "response_suppression_level"),
        ],
    );
    Ok(())
}

fn migrate_decoded_image(image: &mut Value) -> Result<(), StorageError> {
    rename_json_fields(
        get_json_object(image, "decoded_image")?,
        &[
            ("width", "pixel_width"),
            ("height", "pixel_height"),
            ("rgba", "rgba_bytes"),
        ],
    );
    Ok(())
}

fn migrate_terminal_images(json_fields: &mut Map<String, Value>) -> Result<(), StorageError> {
    let contents = get_required_json_field(json_fields, "image_contents", "terminal")?;
    migrate_json_array_elements(
        contents,
        "terminal.image_contents",
        |content, content_path| {
            let content_fields = get_json_object(content, content_path)?;
            rename_json_fields(
                content_fields,
                &[
                    ("id", "image_content_id"),
                    ("image", "decoded_image"),
                    ("animation_frame", "animation_frame_index"),
                    ("animation_loops", "animation_loop_count"),
                    ("animation_running", "is_animation_running"),
                    ("animation_loading", "is_animation_loading"),
                ],
            );
            migrate_decoded_image(get_required_json_field(
                content_fields,
                "decoded_image",
                content_path,
            )?)?;
            if let Some(animation) = content_fields
                .get_mut("animation")
                .filter(|animation| !animation.is_null())
            {
                migrate_animation(animation)?;
            }
            if let Some(sixel) = content_fields
                .get_mut("sixel")
                .filter(|sixel| !sixel.is_null())
            {
                migrate_sixel_source(sixel)?;
            }
            Ok(())
        },
    )?;
    for placement_name in [
        "primary_image_placements",
        "primary_image_history",
        "alternate_image_placements",
    ] {
        let placements = get_required_json_field(json_fields, placement_name, "terminal")?;
        migrate_json_array_elements(placements, placement_name, migrate_image_placement)?;
    }
    let kitty_images = get_required_json_field(json_fields, "kitty_images", "terminal")?;
    migrate_json_array_elements(
        kitty_images,
        "terminal.kitty_images",
        |kitty_image, kitty_path| {
            let kitty_fields = get_json_object(kitty_image, kitty_path)?;
            rename_json_fields(
                kitty_fields,
                &[
                    ("content_id", "image_content_id"),
                    ("virtual_placement", "is_virtual_placement"),
                ],
            );
            if kitty_fields
                .get("image_content_id")
                .is_none_or(Value::is_null)
            {
                return Err(build_invalid_resume_error(format!(
                    "{kitty_path}.image_content_id is missing"
                )));
            }
            migrate_image_record(kitty_image, false)
        },
    )
}

fn migrate_image_placement(placement: &mut Value, json_path: &str) -> Result<(), StorageError> {
    let json_fields = get_json_object(placement, json_path)?;
    rename_json_fields(
        json_fields,
        &[
            ("id", "image_placement_id"),
            ("record", "image_record"),
            ("content_id", "image_content_id"),
            ("columns", "column_count"),
            ("rows", "row_count"),
        ],
    );
    for unused_field in ["geometry", "raster"] {
        if json_fields
            .get(unused_field)
            .is_some_and(|field| !field.is_null())
        {
            return Err(build_invalid_resume_error(format!(
                "{json_path}.{unused_field} cannot be migrated"
            )));
        }
        json_fields.remove(unused_field);
    }
    if json_fields
        .get("image_content_id")
        .is_none_or(Value::is_null)
    {
        return Err(build_invalid_resume_error(format!(
            "{json_path}.image_content_id is missing"
        )));
    }
    migrate_image_record(
        get_required_json_field(json_fields, "image_record", json_path)?,
        false,
    )?;
    let plan = get_required_json_field(json_fields, "plan", json_path)?;
    if plan.is_null() {
        return Err(build_invalid_resume_error(format!(
            "{json_path}.plan is missing"
        )));
    }
    migrate_raster_plan(plan)
}

fn migrate_raster_plan(plan: &mut Value) -> Result<(), StorageError> {
    let json_fields = get_json_object(plan, "raster_plan")?;
    rename_json_fields(
        json_fields,
        &[
            ("source", "source_rect"),
            ("target", "target_size"),
            ("canvas", "canvas_size"),
        ],
    );
    json_fields.insert("needs_raster".to_string(), Value::Bool(true));
    let geometry = get_json_object(
        get_required_json_field(json_fields, "geometry", "raster_plan")?,
        "image_geometry",
    )?;
    rename_json_field(geometry, "offset", "cell_offset");
    migrate_size(get_required_json_field(
        geometry,
        "full_size",
        "image_geometry",
    )?)?;
    let offset = get_json_object(
        get_required_json_field(geometry, "cell_offset", "image_geometry")?,
        "image_offset",
    )?;
    rename_json_fields(offset, &[("x", "column"), ("y", "row")]);
    Ok(())
}

fn migrate_animation(animation: &mut Value) -> Result<(), StorageError> {
    let json_fields = get_json_object(animation, "animation")?;
    if let Some(frames) = json_fields.get_mut("frames") {
        migrate_json_array_elements(frames, "animation.frames", |frame, frame_path| {
            let frame_fields = get_json_object(frame, frame_path)?;
            rename_json_fields(
                frame_fields,
                &[
                    ("image", "decoded_image"),
                    ("delay", "frame_delay"),
                    ("gapless", "is_gapless"),
                ],
            );
            migrate_decoded_image(get_required_json_field(
                frame_fields,
                "decoded_image",
                frame_path,
            )?)
        })?;
    }
    Ok(())
}

fn migrate_sixel_source(sixel: &mut Value) -> Result<(), StorageError> {
    let sixel_fields = get_json_object(sixel, "sixel")?;
    rename_json_fields(
        sixel_fields,
        &[
            ("indexed", "indexed_image"),
            ("palette", "sixel_palette"),
            ("shared_palette", "is_shared_palette"),
        ],
    );
    rename_json_fields(
        get_json_object(
            get_required_json_field(sixel_fields, "indexed_image", "sixel")?,
            "sixel.indexed_image",
        )?,
        &[
            ("width", "width_pixels"),
            ("height", "height_pixels"),
            ("indices", "pixel_register_indices"),
            ("aspect_vertical", "pixel_aspect_vertical"),
            ("aspect_horizontal", "pixel_aspect_horizontal"),
        ],
    );
    Ok(())
}

#[cfg(test)]
mod tests;
