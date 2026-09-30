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
                "index_name": "PRIMARY",
                "columns": [column],
                "is_unique": true,
                "is_primary": true,
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
                    "index_name": format!("idx_{name}"),
                    "columns": [name],
                    "is_unique": false,
                    "is_primary": false,
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

pub fn get_schema_snapshot(id: Value, _params: &Value) -> Value {
    ok_response(id, json!([]))
}

pub fn get_all_columns_batch(id: Value, _params: &Value) -> Value {
    ok_response(id, json!({}))
}

pub fn get_all_foreign_keys_batch(id: Value, _params: &Value) -> Value {
    ok_response(id, json!({}))
}

fn client(params: &Value) -> Result<Client, PluginError> {
    Client::connect(ConnectionParams::from_value(inner_params(params)))
}

fn database(params: &Value) -> Result<String, PluginError> {
    inner_params(params)
        .get("database")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|database| !database.is_empty())
        .map(str::to_string)
        .ok_or_else(|| PluginError::invalid_params("Harper database is required"))
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

fn database_names(value: Value) -> Result<Value, PluginError> {
    let databases = value
        .get("databases")
        .and_then(Value::as_object)
        .or_else(|| value.as_object())
        .ok_or_else(|| {
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
    value
        .get("tables")
        .and_then(Value::as_object)
        .or_else(|| value.as_object())
}

fn columns_from_description(description: Value) -> Result<Value, PluginError> {
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
        .get("is_nullable")
        .and_then(Value::as_bool)
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
        .cloned()
        .unwrap_or(Value::Null);
    let comment = attribute.get("description").cloned().unwrap_or(Value::Null);

    Some(json!({
        "name": name,
        "data_type": data_type(attribute),
        "is_nullable": is_nullable,
        "default_value": default_value,
        "is_pk": is_primary,
        "is_auto_increment": false,
        "comment": comment,
    }))
}

fn primary_key(description: &Value) -> Option<String> {
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
        "INT" | "NUMBER" => "INTEGER".to_string(),
        "OBJECT" | "ARRAY" => "JSON".to_string(),
        _ => data_type,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{columns_from_description, database_names, indexes_from_description, table_list};

    #[test]
    fn extracts_database_names_in_response_order() {
        let result = database_names(json!({ "data": {}, "system": {} })).unwrap();
        assert_eq!(result, json!(["data", "system"]));
    }

    #[test]
    fn supports_wrapped_table_maps() {
        let result = table_list(json!({
            "tables": {
                "dog": { "description": "Dogs" },
                "owner": {}
            }
        }))
        .unwrap();
        assert_eq!(result[0]["name"], "dog");
        assert_eq!(result[0]["comment"], "Dogs");
        assert_eq!(result[1]["name"], "owner");
    }

    #[test]
    fn supports_v5_primary_key_and_typed_attributes() {
        let result = columns_from_description(json!({
            "primary_key": "id",
            "attributes": [
                { "attribute": "id", "type": "integer" },
                { "attribute": "profile", "type": "object", "required": true }
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
        assert_eq!(result[0]["index_name"], "PRIMARY");
        assert_eq!(result[1]["index_name"], "idx_email");
        assert_eq!(result[1]["is_unique"], false);
    }
}
