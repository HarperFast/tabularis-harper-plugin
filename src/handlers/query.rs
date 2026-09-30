use std::time::Instant;

use serde_json::{json, Map, Value};

use crate::client::Client;
use crate::error::PluginError;
use crate::models::{inner_params, ConnectionParams};
use crate::rpc::{not_implemented, result_response};

pub fn test_connection(id: Value, params: &Value) -> Value {
    let result = client(params)
        .and_then(|client| client.describe_all())
        .map(|_| json!({ "success": true }));
    result_response(id, result)
}

pub fn ping(id: Value, params: &Value) -> Value {
    let result = client(params)
        .and_then(|client| client.health())
        .map(|_| Value::Null);
    result_response(id, result)
}

pub fn execute_query(id: Value, params: &Value) -> Value {
    let result = execute_query_inner(params);
    result_response(id, result)
}

pub fn explain_query(id: Value, _params: &Value) -> Value {
    not_implemented(id, "explain_query")
}

fn execute_query_inner(params: &Value) -> Result<Value, PluginError> {
    let query = params
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .ok_or_else(|| PluginError::invalid_params("query is required"))?;
    if !is_single_select(query) {
        return Err(PluginError::invalid_params(
            "this read-only driver accepts one SELECT statement at a time",
        ));
    }

    let started = Instant::now();
    let response = client(params)?.sql(query)?;
    let elapsed = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    let page = params
        .get("page")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);
    let page_size = params
        .get("page_size")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 1_000);

    Ok(tabularis_query_result(response, page, page_size, elapsed))
}

fn client(params: &Value) -> Result<Client, PluginError> {
    Client::connect(ConnectionParams::from_value(inner_params(params)))
}

fn is_single_select(query: &str) -> bool {
    let query = query.trim();
    let query = query.strip_suffix(';').unwrap_or(query).trim_end();
    !query.contains(';')
        && query
            .split_ascii_whitespace()
            .next()
            .is_some_and(|keyword| keyword.eq_ignore_ascii_case("select"))
}

fn tabularis_query_result(response: Value, page: u64, page_size: u64, elapsed: u64) -> Value {
    let values = match response {
        Value::Array(values) => values,
        Value::Null => Vec::new(),
        value => vec![value],
    };
    let columns = collect_columns(&values);
    let total_count = values.len();
    let start = (page - 1).saturating_mul(page_size) as usize;
    let rows = values
        .into_iter()
        .skip(start)
        .take(page_size as usize)
        .map(|value| row_values(value, &columns))
        .collect::<Vec<_>>();

    json!({
        "columns": columns,
        "rows": rows,
        "total_count": total_count,
        "execution_time_ms": elapsed,
    })
}

fn collect_columns(values: &[Value]) -> Vec<String> {
    let mut columns = Vec::new();
    for value in values {
        match value {
            Value::Object(object) => {
                for column in object.keys() {
                    if !columns.iter().any(|existing| existing == column) {
                        columns.push(column.clone());
                    }
                }
            }
            _ if !columns.iter().any(|column| column == "value") => {
                columns.push("value".to_string());
            }
            _ => {}
        }
    }
    columns
}

fn row_values(value: Value, columns: &[String]) -> Vec<Value> {
    match value {
        Value::Object(mut object) => columns
            .iter()
            .map(|column| object.remove(column).unwrap_or(Value::Null))
            .collect(),
        value => scalar_row(value, columns),
    }
}

fn scalar_row(value: Value, columns: &[String]) -> Vec<Value> {
    let mut object = Map::new();
    object.insert("value".to_string(), value);
    columns
        .iter()
        .map(|column| object.remove(column).unwrap_or(Value::Null))
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{is_single_select, tabularis_query_result};

    #[test]
    fn read_only_guard_allows_one_select() {
        assert!(is_single_select(" SELECT * FROM data.dog; "));
        assert!(!is_single_select("DELETE FROM data.dog"));
        assert!(!is_single_select(
            "SELECT * FROM data.dog; DELETE FROM data.dog"
        ));
        assert!(!is_single_select("-- comment\nSELECT * FROM data.dog"));
    }

    #[test]
    fn query_result_preserves_first_seen_columns_and_missing_values() {
        let response = json!([
            { "name": "Ada", "id": 1 },
            { "id": 2, "active": true }
        ]);
        let result = tabularis_query_result(response, 1, 100, 12);

        assert_eq!(result["columns"], json!(["name", "id", "active"]));
        assert_eq!(result["rows"], json!([["Ada", 1, null], [null, 2, true]]));
        assert_eq!(result["total_count"], 2);
        assert_eq!(result["execution_time_ms"], 12);
    }

    #[test]
    fn query_result_paginates_without_rewriting_sql() {
        let response = json!([{ "id": 1 }, { "id": 2 }, { "id": 3 }]);
        let result = tabularis_query_result(response, 2, 2, 0);

        assert_eq!(result["rows"], json!([[3]]));
        assert_eq!(result["total_count"], 3);
    }

    #[test]
    fn object_responses_are_displayed_as_one_row() {
        let result = tabularis_query_result(json!({ "message": "ok", "count": 1 }), 1, 100, 0);
        assert_eq!(result["columns"], json!(["message", "count"]));
        assert_eq!(result["rows"], json!([["ok", 1]]));
    }
}
