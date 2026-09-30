use serde_json::{Map, Value};

use crate::client::Client;
use crate::error::PluginError;
use crate::handlers::metadata::{database, primary_key};
use crate::models::{inner_params, ConnectionParams};
use crate::rpc::result_response;

pub fn insert_record(id: Value, params: &Value) -> Value {
    result_response(id, insert_record_inner(params).map(Value::from))
}

pub fn update_record(id: Value, params: &Value) -> Value {
    result_response(id, update_record_inner(params).map(Value::from))
}

pub fn delete_record(id: Value, params: &Value) -> Value {
    result_response(id, delete_record_inner(params).map(Value::from))
}

fn insert_record_inner(params: &Value) -> Result<u64, PluginError> {
    let database = database(params)?;
    let table = required_string(params, "table")?;
    let data = params
        .get("data")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| PluginError::invalid_params("data must be an object"))?;
    reject_tabularis_wire_values(&Value::Object(data.clone()))?;
    let response = client(params)?.insert(&database, table, Value::Object(data))?;
    exactly_one_written(&response, "inserted_hashes", "insert")
}

fn update_record_inner(params: &Value) -> Result<u64, PluginError> {
    let database = database(params)?;
    let table = required_string(params, "table")?;
    let column = required_string(params, "col_name")?;
    let new_value = params.get("new_val").cloned().unwrap_or(Value::Null);
    reject_tabularis_wire_values(&new_value)?;
    let client = client(params)?;
    let (primary_key, key) = checked_primary_key(&client, params, &database, table)?;
    if column == primary_key {
        return Err(PluginError::invalid_params(
            "Harper primary-key values cannot be edited in place",
        ));
    }

    let mut record = Map::new();
    record.insert(primary_key, key);
    record.insert(column.to_string(), new_value);
    let response = client.update(&database, table, Value::Object(record))?;
    exactly_one_written(&response, "update_hashes", "update")
}

fn reject_tabularis_wire_values(value: &Value) -> Result<(), PluginError> {
    match value {
        Value::String(value)
            if value == "__USE_DEFAULT__"
                || value.starts_with("BLOB:")
                || value.starts_with("BLOB_FILE_REF:") =>
        {
            Err(PluginError::invalid_params(
                "Tabularis default and binary upload markers are not supported by the Harper driver; enter a JSON value instead",
            ))
        }
        Value::Array(values) => values.iter().try_for_each(reject_tabularis_wire_values),
        Value::Object(values) => values.values().try_for_each(reject_tabularis_wire_values),
        _ => Ok(()),
    }
}

fn delete_record_inner(params: &Value) -> Result<u64, PluginError> {
    let database = database(params)?;
    let table = required_string(params, "table")?;
    let client = client(params)?;
    let (_, key) = checked_primary_key(&client, params, &database, table)?;
    let response = client.delete(&database, table, key)?;
    exactly_one_written(&response, "deleted_hashes", "delete")
}

fn checked_primary_key(
    client: &Client,
    params: &Value,
    database: &str,
    table: &str,
) -> Result<(String, Value), PluginError> {
    let description = client.describe_table(database, table)?;
    let primary_key = primary_key(&description).ok_or_else(|| {
        PluginError::invalid_params(format!(
            "Harper table '{database}.{table}' has no discoverable primary key"
        ))
    })?;
    let key = validate_primary_key_map(params, &primary_key)?;
    Ok((primary_key, key))
}

fn validate_primary_key_map(params: &Value, primary_key: &str) -> Result<Value, PluginError> {
    let key_map = params
        .get("pk_map")
        .and_then(Value::as_object)
        .ok_or_else(|| PluginError::invalid_params("pk_map must be an object"))?;
    if key_map.len() != 1 || !key_map.contains_key(primary_key) {
        return Err(PluginError::invalid_params(format!(
            "row identity must contain only Harper primary key '{primary_key}'"
        )));
    }
    let key = key_map.get(primary_key).cloned().unwrap_or(Value::Null);
    if key.is_null() || key.as_str().is_some_and(|key| key.trim().is_empty()) {
        return Err(PluginError::invalid_params(
            "Harper primary-key value cannot be null or empty",
        ));
    }
    Ok(key)
}

pub(crate) fn exactly_one_written(
    response: &Value,
    field: &str,
    operation: &str,
) -> Result<u64, PluginError> {
    let written = response
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| {
            PluginError::connection(format!(
                "Harper {operation} response did not include '{field}'"
            ))
        })?;
    let skipped = response
        .get("skipped_hashes")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if written.len() == 1 && skipped == 0 {
        Ok(1)
    } else {
        Err(PluginError::connection(format!(
            "Harper {operation} did not affect exactly one record (written {}, skipped {skipped})",
            written.len()
        )))
    }
}

fn required_string<'a>(params: &'a Value, field: &str) -> Result<&'a str, PluginError> {
    params
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PluginError::invalid_params(format!("{field} is required")))
}

fn client(params: &Value) -> Result<Client, PluginError> {
    Client::connect(ConnectionParams::from_value(inner_params(params)))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{exactly_one_written, reject_tabularis_wire_values, validate_primary_key_map};

    #[test]
    fn single_row_writes_require_one_written_hash_and_no_skips() {
        assert_eq!(
            exactly_one_written(
                &json!({ "inserted_hashes": ["a"], "skipped_hashes": [] }),
                "inserted_hashes",
                "insert"
            )
            .unwrap(),
            1
        );
        assert!(exactly_one_written(
            &json!({ "inserted_hashes": [], "skipped_hashes": ["a"] }),
            "inserted_hashes",
            "insert"
        )
        .is_err());
        assert!(
            exactly_one_written(&json!({ "message": "ok" }), "update_hashes", "update").is_err()
        );
    }

    #[test]
    fn row_identity_must_be_the_non_null_harper_primary_key() {
        assert_eq!(
            validate_primary_key_map(&json!({ "pk_map": { "id": 7 } }), "id").unwrap(),
            7
        );
        assert!(validate_primary_key_map(&json!({ "pk_map": { "owner_id": 7 } }), "id").is_err());
        assert!(validate_primary_key_map(&json!({ "pk_map": { "id": null } }), "id").is_err());
        assert!(validate_primary_key_map(&json!({ "pk_map": { "id": "" } }), "id").is_err());
    }

    #[test]
    fn tabularis_default_and_blob_markers_are_never_stored_as_strings() {
        for value in [
            json!("__USE_DEFAULT__"),
            json!("BLOB:3:text/plain:YWJj"),
            json!("BLOB_FILE_REF:/private/tmp/upload"),
            json!({ "nested": ["BLOB_FILE_REF:/private/tmp/upload"] }),
        ] {
            assert!(reject_tabularis_wire_values(&value).is_err());
        }
        assert!(reject_tabularis_wire_values(&json!("ordinary text")).is_ok());
    }
}
