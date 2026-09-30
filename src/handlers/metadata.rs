use serde_json::{json, Map, Value};

use crate::client::Client;
use crate::error::PluginError;
use crate::models::{inner_params, ConnectionParams};
use crate::rpc::{ok_response, result_response};

pub fn get_databases(id: Value, params: &Value) -> Value {
    let result = client(params)
        .and_then(|client| client.describe_all())
        .and_then(database_names);
    result_response(id, result)
}

pub fn get_schemas(id: Value, _params: &Value) -> Value {
    ok_response(id, json!([]))
}

pub fn get_tables(id: Value, params: &Value) -> Value {
    let result = database(params).and_then(|database| {
        client(params)?
            .describe_database(&database)
            .and_then(table_list)
    });
    result_response(id, result)
}

pub fn get_columns(id: Value, params: &Value) -> Value {
    let result = table_description(params).and_then(columns_from_description);
    result_response(id, result)
}

pub fn get_foreign_keys(id: Value, _params: &Value) -> Value {
    ok_response(id, json!([]))
}

pub fn get_indexes(id: Value, params: &Value) -> Value {
    let result = table_description(params).map(indexes_from_description);
    result_response(id, result)
}

fn indexes_from_description(description: Value) -> Value {
    let primary_key = primary_key(&description);
    let mut indexes = primary_key
        .as_ref()
        .map(|column| {
            vec![json!({
                "name": "PRIMARY",
                "column_name": column,
                "is_unique": true,
                "is_primary": true,
                "seq_in_index": 1,
                "is_expression": false,
            })]
        })
        .unwrap_or_default();

    if let Some(attributes) = description.get("attributes").and_then(Value::as_array) {
        for attribute in attributes {
            let Some(name) = attribute
                .get("attribute")
                .or_else(|| attribute.get("name"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            if attribute.get("indexed").and_then(Value::as_bool) == Some(true)
                && primary_key.as_deref() != Some(name)
            {
                indexes.push(json!({
                    "name": format!("idx_{name}"),
                    "column_name": name,
                    "is_unique": false,
                    "is_primary": false,
                    "seq_in_index": 1,
                    "is_expression": false,
                }));
            }
        }
    }

    Value::Array(indexes)
}

pub fn get_views(id: Value, _params: &Value) -> Value {
    ok_response(id, json!([]))
}

pub fn get_view_definition(id: Value, _params: &Value) -> Value {
    ok_response(id, Value::String(String::new()))
}

pub fn get_view_columns(id: Value, _params: &Value) -> Value {
    ok_response(id, json!([]))
}

pub fn get_routines(id: Value, _params: &Value) -> Value {
    ok_response(id, json!([]))
}

pub fn get_routine_parameters(id: Value, _params: &Value) -> Value {
    ok_response(id, json!([]))
}

pub fn get_routine_definition(id: Value, _params: &Value) -> Value {
    ok_response(id, Value::String(String::new()))
}

pub fn get_schema_snapshot(id: Value, params: &Value) -> Value {
    let result = database_metadata(params).and_then(|description| {
        let descriptions = table_descriptions(&description)?;
        let tables = selected_table_names(params, descriptions);
        tables
            .into_iter()
            .map(|name| {
                let table = descriptions.get(&name).ok_or_else(|| {
                    PluginError::connection(format!("Harper did not describe table '{name}'"))
                })?;
                Ok(json!({
                    "name": name,
                    "columns": columns_from_description(table.clone())?,
                    "foreign_keys": [],
                }))
            })
            .collect::<Result<Vec<_>, PluginError>>()
            .map(Value::Array)
    });
    result_response(id, result)
}

pub fn get_all_columns_batch(id: Value, params: &Value) -> Value {
    let result = database_metadata(params).and_then(|description| {
        let descriptions = table_descriptions(&description)?;
        let mut columns = Map::new();
        for name in selected_table_names(params, descriptions) {
            let table = descriptions.get(&name).ok_or_else(|| {
                PluginError::connection(format!("Harper did not describe table '{name}'"))
            })?;
            columns.insert(name, columns_from_description(table.clone())?);
        }
        Ok(Value::Object(columns))
    });
    result_response(id, result)
}

pub fn get_all_foreign_keys_batch(id: Value, params: &Value) -> Value {
    let result = database_metadata(params).and_then(|description| {
        let descriptions = table_descriptions(&description)?;
        let foreign_keys = selected_table_names(params, descriptions)
            .into_iter()
            .map(|name| (name, json!([])))
            .collect();
        Ok(Value::Object(foreign_keys))
    });
    result_response(id, result)
}

fn client(params: &Value) -> Result<Client, PluginError> {
    Client::connect(ConnectionParams::from_value(inner_params(params)))
}

pub(crate) fn database(params: &Value) -> Result<String, PluginError> {
    if let Some(database) = params.get("schema").and_then(non_empty_string) {
        return Ok(database.to_string());
    }
    let database = inner_params(params).get("database");
    if let Some(database) = database.and_then(non_empty_string) {
        return Ok(database.to_string());
    }
    let databases = database
        .and_then(Value::as_array)
        .map(|databases| {
            databases
                .iter()
                .filter_map(non_empty_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    match databases.as_slice() {
        [database] => Ok((*database).to_string()),
        [] => Err(PluginError::invalid_params("Harper database is required")),
        _ => Err(PluginError::invalid_params(
            "select exactly one Harper database for this operation",
        )),
    }
}

pub(crate) fn non_empty_string(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn table(params: &Value) -> Result<String, PluginError> {
    params
        .get("table")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|table| !table.is_empty())
        .map(str::to_string)
        .ok_or_else(|| PluginError::invalid_params("table is required"))
}

fn table_description(params: &Value) -> Result<Value, PluginError> {
    let database = database(params)?;
    let table = table(params)?;
    client(params)?.describe_table(&database, &table)
}

fn database_metadata(params: &Value) -> Result<Value, PluginError> {
    let database = database(params)?;
    client(params)?.describe_database(&database)
}

fn table_descriptions(description: &Value) -> Result<&Map<String, Value>, PluginError> {
    description
        .as_object()
        .ok_or_else(|| PluginError::connection("Harper returned an invalid database description"))
}

fn selected_table_names(params: &Value, available: &Map<String, Value>) -> Vec<String> {
    let requested = params.get("tables").and_then(Value::as_array);
    match requested {
        Some(requested) => requested
            .iter()
            .filter_map(non_empty_string)
            .filter(|name| available.contains_key(*name))
            .map(str::to_string)
            .collect(),
        None => available.keys().cloned().collect(),
    }
}

fn database_names(value: Value) -> Result<Value, PluginError> {
    let databases = value.as_object().ok_or_else(|| {
        PluginError::connection("Harper returned an invalid database description")
    })?;
    Ok(Value::Array(
        databases.keys().cloned().map(Value::String).collect(),
    ))
}

fn table_list(value: Value) -> Result<Value, PluginError> {
    let tables = table_map(&value)
        .ok_or_else(|| PluginError::connection("Harper returned an invalid table description"))?;
    let tables = tables
        .iter()
        .map(|(name, description)| {
            json!({
                "name": name,
                "schema": null,
                "comment": description.get("description").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    Ok(Value::Array(tables))
}

fn table_map(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

pub(crate) fn columns_from_description(description: Value) -> Result<Value, PluginError> {
    let attributes = description
        .get("attributes")
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::connection("Harper table description has no attributes"))?;
    let primary_key = primary_key(&description);
    let columns = attributes
        .iter()
        .filter_map(|attribute| column_from_attribute(attribute, primary_key.as_deref()))
        .collect();
    Ok(Value::Array(columns))
}

fn column_from_attribute(attribute: &Value, primary_key: Option<&str>) -> Option<Value> {
    let name = attribute
        .as_str()
        .or_else(|| attribute.get("attribute").and_then(Value::as_str))
        .or_else(|| attribute.get("name").and_then(Value::as_str))?;
    let is_primary = attribute
        .get("is_primary_key")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || primary_key == Some(name);
    let is_nullable = attribute
        .get("nullable")
        .and_then(Value::as_bool)
        .or_else(|| attribute.get("is_nullable").and_then(Value::as_bool))
        .or_else(|| {
            attribute
                .get("required")
                .and_then(Value::as_bool)
                .map(|required| !required)
        })
        .unwrap_or(!is_primary);
    let default_value = attribute
        .get("default_value")
        .or_else(|| attribute.get("default"))
        .and_then(string_value)
        .map(Value::String)
        .unwrap_or(Value::Null);
    let comment = attribute
        .get("description")
        .and_then(string_value)
        .map(Value::String)
        .unwrap_or(Value::Null);
    let data_type = data_type(attribute);
    let is_auto_increment =
        is_primary && matches!(data_type.as_str(), "INTEGER" | "LONG" | "FLOAT" | "BIGINT");

    Some(json!({
        "name": name,
        "data_type": data_type,
        "is_nullable": is_nullable,
        "default_value": default_value,
        "is_pk": is_primary,
        "is_auto_increment": is_auto_increment,
        "is_generated": false,
        "character_maximum_length": null,
        "comment": comment,
    }))
}

fn string_value(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(value) => Some(value.clone()),
        value => Some(value.to_string()),
    }
}

pub(crate) fn primary_key(description: &Value) -> Option<String> {
    ["primary_key", "hash_attribute"]
        .into_iter()
        .find_map(|field| description.get(field))
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("attribute").and_then(Value::as_str))
                .or_else(|| {
                    value
                        .as_array()
                        .and_then(|values| values.first())
                        .and_then(Value::as_str)
                })
        })
        .map(str::to_string)
}

fn data_type(attribute: &Value) -> String {
    let data_type = attribute
        .get("data_type")
        .or_else(|| attribute.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("ANY")
        .to_ascii_uppercase();
    match data_type.as_str() {
        "STRING" => "TEXT".to_string(),
        "INT" => "INTEGER".to_string(),
        "NUMBER" => "FLOAT".to_string(),
        "OBJECT" | "ARRAY" => "JSON".to_string(),
        _ => data_type,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        columns_from_description, database, database_names, indexes_from_description, table_list,
    };

    #[test]
    fn uses_schema_as_database_for_tabularis_multi_database_requests() {
        let result = database(&json!({
            "params": { "database": "" },
            "schema": "data"
        }))
        .unwrap();

        assert_eq!(result, "data");
    }

    #[test]
    fn uses_single_connection_database_without_a_per_call_selection() {
        let result = database(&json!({
            "params": { "database": ["data"] },
            "schema": null
        }))
        .unwrap();

        assert_eq!(result, "data");
    }

    #[test]
    fn rejects_an_ambiguous_multi_database_request() {
        let error = database(&json!({
            "params": { "database": ["data", "packages"] },
            "schema": null
        }))
        .unwrap_err();

        assert!(error.message.contains("exactly one Harper database"));
    }

    #[test]
    fn ignores_empty_schema_and_database_candidates() {
        let result = database(&json!({
            "params": { "database": ["", "packages"] },
            "schema": "  "
        }))
        .unwrap();

        assert_eq!(result, "packages");
    }

    #[test]
    fn extracts_database_names_in_response_order() {
        let result = database_names(json!({ "data": {}, "system": {} })).unwrap();
        assert_eq!(result, json!(["data", "system"]));
    }

    #[test]
    fn reads_top_level_table_maps_even_when_a_table_is_named_tables() {
        let result = table_list(json!({
            "tables": { "description": "A real table" },
            "owner": {}
        }))
        .unwrap();
        assert_eq!(result[0]["name"], "tables");
        assert_eq!(result[0]["comment"], "A real table");
        assert_eq!(result[1]["name"], "owner");
    }

    #[test]
    fn reads_top_level_database_maps_even_when_a_database_is_named_databases() {
        let result = database_names(json!({ "databases": {}, "data": {} })).unwrap();
        assert_eq!(result, json!(["databases", "data"]));
    }

    #[test]
    fn supports_v5_primary_key_and_typed_attributes() {
        let result = columns_from_description(json!({
            "primary_key": "id",
            "attributes": [
                { "attribute": "id", "type": "integer" },
                { "attribute": "profile", "type": "object", "nullable": false }
            ]
        }))
        .unwrap();
        assert_eq!(result[0]["data_type"], "INTEGER");
        assert_eq!(result[0]["is_pk"], true);
        assert_eq!(result[0]["is_nullable"], false);
        assert_eq!(result[1]["data_type"], "JSON");
        assert_eq!(result[1]["is_nullable"], false);
    }

    #[test]
    fn supports_v4_hash_attribute_and_string_attributes() {
        let result = columns_from_description(json!({
            "hash_attribute": "id",
            "attributes": ["id", "name"]
        }))
        .unwrap();
        assert_eq!(result[0]["name"], "id");
        assert_eq!(result[0]["is_pk"], true);
        assert_eq!(result[1]["data_type"], "ANY");
    }

    #[test]
    fn serializes_non_string_defaults_and_comments_for_tabularis() {
        let result = columns_from_description(json!({
            "primary_key": "id",
            "attributes": [
                { "attribute": "id", "type": "Int", "default": 7, "description": { "source": "generated" } }
            ]
        }))
        .unwrap();

        assert_eq!(result[0]["default_value"], "7");
        assert_eq!(result[0]["comment"], r#"{"source":"generated"}"#);
        assert_eq!(result[0]["is_auto_increment"], true);
    }

    #[test]
    fn reports_primary_and_secondary_indexes() {
        let result = indexes_from_description(json!({
            "primary_key": "id",
            "attributes": [
                { "attribute": "id", "is_primary_key": true, "indexed": true },
                { "attribute": "email", "indexed": true },
                { "attribute": "name" }
            ]
        }));

        assert_eq!(result.as_array().unwrap().len(), 2);
        assert_eq!(
            result[0],
            json!({
                "name": "PRIMARY",
                "column_name": "id",
                "is_unique": true,
                "is_primary": true,
                "seq_in_index": 1,
                "is_expression": false,
            })
        );
        assert_eq!(result[1]["name"], "idx_email");
        assert_eq!(result[1]["column_name"], "email");
        assert_eq!(result[1]["is_unique"], false);
        assert_eq!(result[1]["seq_in_index"], 1);
    }
}
