use std::collections::HashSet;
use std::time::Instant;

use serde_json::{json, Map, Value};

use crate::client::Client;
use crate::error::PluginError;
use crate::models::{inner_params, ConnectionParams};
use crate::rpc::{not_implemented, result_response};

pub fn test_connection(id: Value, params: &Value) -> Value {
    let result = client(params)
        .and_then(|client| client.user_info())
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
    let query = normalized_select(query).ok_or_else(|| {
        PluginError::invalid_params("this read-only driver accepts one SELECT statement at a time")
    })?;
    if !is_single_select(&query) {
        return Err(PluginError::invalid_params(
            "this read-only driver accepts one SELECT statement at a time",
        ));
    }

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
    let offset = (page - 1).saturating_mul(page_size);
    let server_paged = ["limit", "offset", "fetch", "top"]
        .into_iter()
        .all(|keyword| !contains_top_level_keyword(&query, keyword));
    let executed_query = if server_paged {
        format!("{query} LIMIT {} OFFSET {offset}", page_size + 1)
    } else {
        query
    };

    let started = Instant::now();
    let response = client(params)?.sql(&executed_query)?;
    let elapsed = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);

    Ok(tabularis_query_result(
        response,
        page,
        page_size,
        elapsed,
        server_paged,
    ))
}

fn client(params: &Value) -> Result<Client, PluginError> {
    Client::connect(ConnectionParams::from_value(inner_params(params)))
}

fn is_single_select(query: &str) -> bool {
    query
        .split_ascii_whitespace()
        .next()
        .is_some_and(|keyword| keyword.eq_ignore_ascii_case("select"))
}

fn normalized_select(query: &str) -> Option<String> {
    let query = query.trim();
    let mut terminal_semicolon = None;
    let mut quote = None;
    let mut chars = query.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        if let Some(active_quote) = quote {
            if character == active_quote {
                if chars.peek().is_some_and(|(_, next)| *next == active_quote) {
                    chars.next();
                } else {
                    quote = None;
                }
            }
            continue;
        }
        match character {
            '\'' | '"' | '`' => quote = Some(character),
            ';' => {
                if terminal_semicolon.is_some() {
                    return None;
                }
                terminal_semicolon = Some(index);
            }
            _ => {}
        }
    }
    if quote.is_some() {
        return None;
    }

    let query = match terminal_semicolon {
        Some(index) if query[index + 1..].trim().is_empty() => query[..index].trim_end(),
        Some(_) => return None,
        None => query,
    };
    let keyword = query.get(..6)?;
    if !keyword.eq_ignore_ascii_case("select") {
        return None;
    }
    let rest = query.get(6..)?;
    if rest.is_empty() {
        return Some(query.to_string());
    }
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(format!("SELECT {}", rest.trim_start()))
}

fn contains_top_level_keyword(query: &str, expected: &str) -> bool {
    let mut quote = None;
    let mut depth = 0_u32;
    let mut word = String::new();
    for character in query.chars().chain(std::iter::once(' ')) {
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' | '`' => quote = Some(character),
            '(' => depth = depth.saturating_add(1),
            ')' => depth = depth.saturating_sub(1),
            character if depth == 0 && (character.is_ascii_alphanumeric() || character == '_') => {
                word.push(character);
            }
            _ if depth == 0 => {
                if word.eq_ignore_ascii_case(expected) {
                    return true;
                }
                word.clear();
            }
            _ => {}
        }
    }
    false
}

fn tabularis_query_result(
    response: Value,
    page: u64,
    page_size: u64,
    elapsed: u64,
    server_paged: bool,
) -> Value {
    let mut values = match response {
        Value::Array(values) => values,
        Value::Null => Vec::new(),
        value => vec![value],
    };
    let offset = (page - 1).saturating_mul(page_size);
    let offset = usize::try_from(offset).unwrap_or(usize::MAX);
    let has_more = server_paged && values.len() > page_size as usize;
    if has_more {
        values.truncate(page_size as usize);
    }
    let columns = collect_columns(&values);
    let total_count = if server_paged {
        offset
            .saturating_add(values.len())
            .saturating_add(usize::from(has_more))
    } else {
        values.len()
    };
    let start = if server_paged { 0 } else { offset };
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
    let mut seen = HashSet::new();
    for value in values {
        match value {
            Value::Object(object) => {
                for column in object.keys() {
                    if seen.insert(column.clone()) {
                        columns.push(column.clone());
                    }
                }
            }
            _ if seen.insert("value".to_string()) => {
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

    use super::{
        contains_top_level_keyword, is_single_select, normalized_select, tabularis_query_result,
    };

    #[test]
    fn read_only_guard_allows_one_select() {
        assert!(is_single_select(
            &normalized_select(" SELECT * FROM data.dog; ").unwrap()
        ));
        assert_eq!(
            normalized_select("SELECT\n  *\nFROM data.dog").unwrap(),
            "SELECT *\nFROM data.dog"
        );
        assert!(normalized_select("SELECT ';' AS punctuation").is_some());
        assert!(normalized_select("SELECT 1; DELETE FROM data.dog").is_none());
        assert!(!is_single_select("DELETE FROM data.dog"));
        assert!(normalized_select("-- comment\nSELECT * FROM data.dog").is_none());
    }

    #[test]
    fn detects_only_top_level_limit_keywords() {
        assert!(contains_top_level_keyword(
            "SELECT * FROM data.dog LIMIT 10",
            "limit"
        ));
        assert!(!contains_top_level_keyword(
            "SELECT 'limit' AS value FROM data.dog",
            "limit"
        ));
        assert!(!contains_top_level_keyword(
            "SELECT * FROM (SELECT * FROM data.dog LIMIT 10) nested",
            "limit"
        ));
    }

    #[test]
    fn query_result_preserves_first_seen_columns_and_missing_values() {
        let response = json!([
            { "name": "Ada", "id": 1 },
            { "id": 2, "active": true }
        ]);
        let result = tabularis_query_result(response, 1, 100, 12, false);

        assert_eq!(result["columns"], json!(["name", "id", "active"]));
        assert_eq!(result["rows"], json!([["Ada", 1, null], [null, 2, true]]));
        assert_eq!(result["total_count"], 2);
        assert_eq!(result["execution_time_ms"], 12);
    }

    #[test]
    fn query_result_paginates_without_rewriting_sql() {
        let response = json!([{ "id": 1 }, { "id": 2 }, { "id": 3 }]);
        let result = tabularis_query_result(response, 2, 2, 0, false);

        assert_eq!(result["rows"], json!([[3]]));
        assert_eq!(result["total_count"], 3);
    }

    #[test]
    fn object_responses_are_displayed_as_one_row() {
        let result =
            tabularis_query_result(json!({ "message": "ok", "count": 1 }), 1, 100, 0, false);
        assert_eq!(result["columns"], json!(["message", "count"]));
        assert_eq!(result["rows"], json!([["ok", 1]]));
    }

    #[test]
    fn server_paging_uses_an_extra_row_as_a_next_page_signal() {
        let response = json!([{ "id": 1 }, { "id": 2 }, { "id": 3 }]);
        let result = tabularis_query_result(response, 2, 2, 0, true);

        assert_eq!(result["rows"], json!([[1], [2]]));
        assert_eq!(result["total_count"], 5);
    }
}
