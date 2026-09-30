use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::client::Client;
use crate::error::PluginError;
use crate::handlers::metadata::{database, primary_key};
use crate::models::{inner_params, ConnectionParams};
use crate::rpc::result_response;

const PRIMARY_KEY_CACHE_TTL: Duration = Duration::from_secs(1);
const MAX_PRIMARY_KEY_CACHE_ENTRIES: usize = 1_024;

thread_local! {
    static PRIMARY_KEY_CACHE: RefCell<HashMap<PrimaryKeyCacheKey, CachedPrimaryKey>> =
        RefCell::new(HashMap::new());
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PrimaryKeyCacheKey {
    host: Option<String>,
    port: Option<u16>,
    username: Option<String>,
    ssl_mode: Option<String>,
    ssl_ca: Option<String>,
    ssl_cert: Option<String>,
    ssl_key: Option<String>,
    database: String,
    table: String,
}

struct CachedPrimaryKey {
    name: String,
    loaded_at: Instant,
}

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
        .ok_or_else(|| PluginError::invalid_params("data must be an object"))?;
    reject_tabularis_wire_values(data)?;
    let data = data
        .as_object()
        .cloned()
        .ok_or_else(|| PluginError::invalid_params("data must be an object"))?;
    let response = client(params)?.insert(&database, table, Value::Object(data))?;
    exactly_one_written(&response, "inserted_hashes", "insert")
}

fn update_record_inner(params: &Value) -> Result<u64, PluginError> {
    let database = database(params)?;
    let table = required_string(params, "table")?;
    let column = required_string(params, "col_name")?;
    let new_value = params.get("new_val").cloned().unwrap_or(Value::Null);
    reject_tabularis_wire_values(&new_value)?;
    let (identity_name, key) = row_identity(params)?;
    if column == identity_name {
        return Err(PluginError::invalid_params(
            "Harper primary-key values cannot be edited in place",
        ));
    }
    let client = client(params)?;
    let primary_key = checked_primary_key(&client, params, &database, table)?;
    if identity_name != primary_key {
        return Err(PluginError::invalid_params(format!(
            "row identity must contain only Harper primary key '{primary_key}'"
        )));
    }

    let mut record = Map::new();
    record.insert(primary_key, key.clone());
    record.insert(column.to_string(), new_value);
    let response = client.update(&database, table, Value::Object(record))?;
    exactly_expected_written(&response, "update_hashes", "update", &key)
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
    let (identity_name, key) = row_identity(params)?;
    let client = client(params)?;
    let primary_key = checked_primary_key(&client, params, &database, table)?;
    if identity_name != primary_key {
        return Err(PluginError::invalid_params(format!(
            "row identity must contain only Harper primary key '{primary_key}'"
        )));
    }
    let response = client.delete(&database, table, key)?;
    exactly_one_written(&response, "deleted_hashes", "delete")
}

fn checked_primary_key(
    client: &Client,
    params: &Value,
    database: &str,
    table: &str,
) -> Result<String, PluginError> {
    let cache_key = primary_key_cache_key(params, database, table);
    if let Some(primary_key) = cached_primary_key(&cache_key) {
        return Ok(primary_key);
    }
    let description = client.describe_table(database, table)?;
    let primary_key = primary_key(&description).ok_or_else(|| {
        PluginError::invalid_params(format!(
            "Harper table '{database}.{table}' has no discoverable primary key"
        ))
    })?;
    PRIMARY_KEY_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= MAX_PRIMARY_KEY_CACHE_ENTRIES {
            cache.retain(|_, cached| cached.loaded_at.elapsed() <= primary_key_cache_ttl());
            if cache.len() >= MAX_PRIMARY_KEY_CACHE_ENTRIES {
                cache.clear();
            }
        }
        cache.insert(
            cache_key,
            CachedPrimaryKey {
                name: primary_key.clone(),
                loaded_at: Instant::now(),
            },
        )
    });
    Ok(primary_key)
}

fn row_identity(params: &Value) -> Result<(String, Value), PluginError> {
    let key_map = params
        .get("pk_map")
        .and_then(Value::as_object)
        .ok_or_else(|| PluginError::invalid_params("pk_map must be an object"))?;
    if key_map.len() != 1 {
        return Err(PluginError::invalid_params(
            "row identity must contain exactly one Harper primary-key field",
        ));
    }
    let (name, key) = key_map.iter().next().expect("map length was checked");
    if !matches!(key, Value::Number(_))
        && !matches!(key, Value::String(value) if !value.trim().is_empty())
    {
        return Err(PluginError::invalid_params(
            "Harper primary-key value must be a non-empty string or number; composite keys are not supported",
        ));
    }
    Ok((name.clone(), key.clone()))
}

fn cached_primary_key(cache_key: &PrimaryKeyCacheKey) -> Option<String> {
    PRIMARY_KEY_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let cached = cache
            .get(cache_key)
            .filter(|cached| cached.loaded_at.elapsed() <= primary_key_cache_ttl())
            .map(|cached| cached.name.clone());
        if cached.is_none() {
            cache.remove(cache_key);
        }
        cached
    })
}

fn primary_key_cache_key(params: &Value, database: &str, table: &str) -> PrimaryKeyCacheKey {
    let connection = ConnectionParams::from_value(inner_params(params));
    PrimaryKeyCacheKey {
        host: connection.host,
        port: connection.port,
        username: connection.username,
        ssl_mode: connection.ssl_mode,
        ssl_ca: connection.ssl_ca,
        ssl_cert: connection.ssl_cert,
        ssl_key: connection.ssl_key,
        database: database.to_string(),
        table: table.to_string(),
    }
}

pub(crate) fn invalidate_primary_key_cache(params: &Value, database: &str, table: &str) {
    let cache_key = primary_key_cache_key(params, database, table);
    PRIMARY_KEY_CACHE.with(|cache| cache.borrow_mut().remove(&cache_key));
}

#[cfg(not(test))]
fn primary_key_cache_ttl() -> Duration {
    PRIMARY_KEY_CACHE_TTL
}

#[cfg(test)]
fn primary_key_cache_ttl() -> Duration {
    TEST_PRIMARY_KEY_CACHE_TTL.with(std::cell::Cell::get)
}

#[cfg(test)]
thread_local! {
    static TEST_PRIMARY_KEY_CACHE_TTL: std::cell::Cell<Duration> =
        const { std::cell::Cell::new(PRIMARY_KEY_CACHE_TTL) };
}

#[cfg(test)]
pub(crate) struct PrimaryKeyCacheTtlGuard(Duration);

#[cfg(test)]
impl Drop for PrimaryKeyCacheTtlGuard {
    fn drop(&mut self) {
        TEST_PRIMARY_KEY_CACHE_TTL.with(|ttl| ttl.set(self.0));
    }
}

#[cfg(test)]
pub(crate) fn set_primary_key_cache_ttl_for_test(ttl: Duration) -> PrimaryKeyCacheTtlGuard {
    let previous = TEST_PRIMARY_KEY_CACHE_TTL.with(|current| current.replace(ttl));
    PrimaryKeyCacheTtlGuard(previous)
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

fn exactly_expected_written(
    response: &Value,
    field: &str,
    operation: &str,
    expected_key: &Value,
) -> Result<u64, PluginError> {
    exactly_one_written(response, field, operation)?;
    let written_key = &response[field][0];
    if written_key == expected_key {
        Ok(1)
    } else {
        Err(PluginError::connection(format!(
            "Harper committed the {operation} to record key {written_key}, not requested key {expected_key}; verify the database state before retrying"
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

    use super::{
        exactly_expected_written, exactly_one_written, primary_key_cache_key,
        reject_tabularis_wire_values, row_identity,
    };

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
            row_identity(&json!({ "pk_map": { "id": 7 } })).unwrap(),
            ("id".to_string(), json!(7))
        );
        assert!(row_identity(&json!({ "pk_map": { "id": 7, "other": 8 } })).is_err());
        assert!(row_identity(&json!({ "pk_map": { "id": null } })).is_err());
        assert!(row_identity(&json!({ "pk_map": { "id": "" } })).is_err());
        assert!(row_identity(&json!({ "pk_map": { "id": {} } })).is_err());
        assert!(row_identity(&json!({ "pk_map": { "id": [] } })).is_err());
        assert!(row_identity(&json!({ "pk_map": { "id": true } })).is_err());
    }

    #[test]
    fn primary_key_cache_keys_preserve_field_boundaries() {
        let params = json!({ "params": { "host": "localhost", "username": "tester" } });

        assert_ne!(
            primary_key_cache_key(&params, "a|b", "c"),
            primary_key_cache_key(&params, "a", "b|c")
        );
    }

    #[test]
    fn update_response_must_name_the_requested_record() {
        assert!(exactly_expected_written(
            &json!({ "update_hashes": [7], "skipped_hashes": [] }),
            "update_hashes",
            "update",
            &json!(7)
        )
        .is_ok());
        let error = exactly_expected_written(
            &json!({ "update_hashes": ["7"], "skipped_hashes": [] }),
            "update_hashes",
            "update",
            &json!(7),
        )
        .unwrap_err();
        assert!(error.message.contains("committed"));
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
