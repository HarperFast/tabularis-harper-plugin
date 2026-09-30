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
    let page_size = requested_page_size(params);
    let offset = (page - 1).saturating_mul(page_size);
    let server_paged = ["limit", "offset", "fetch", "top"]
        .into_iter()
        .all(|keyword| !contains_top_level_keyword(&query, keyword));
    let executed_query = if server_paged {
        format!("{query}\nLIMIT {} OFFSET {offset}", page_size + 1)
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

fn requested_page_size(params: &Value) -> u64 {
    params
        .get("limit")
        .and_then(Value::as_u64)
        .or_else(|| params.get("page_size").and_then(Value::as_u64))
        .unwrap_or(100)
        .clamp(1, 1_000)
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
    let scan = scan_sql(query)?;
    if scan.semicolons.len() > 1 {
        return None;
    }

    let normalized;
    let query = match scan.semicolons.first().copied() {
        Some(index) if scan.last_code_index == Some(index) => {
            normalized = format!("{}{}", &query[..index], &query[index + 1..]);
            normalized.trim()
        }
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
    scan_sql(query).is_some_and(|scan| {
        scan.top_level_keywords
            .iter()
            .any(|keyword| keyword.eq_ignore_ascii_case(expected))
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Normal,
    Quoted(char),
    Bracketed,
    LineComment,
    BlockComment,
}

struct SqlScan {
    semicolons: Vec<usize>,
    last_code_index: Option<usize>,
    top_level_keywords: Vec<String>,
}

fn scan_sql(query: &str) -> Option<SqlScan> {
    let mut state = ScanState::Normal;
    let mut depth = 0_u32;
    let mut semicolons = Vec::new();
    let mut last_code_index = None;
    let mut top_level_keywords = Vec::new();
    let mut word = String::new();
    let mut chars = query.char_indices().peekable();

    while let Some((index, character)) = chars.next() {
        match state {
            ScanState::LineComment => {
                if character == '\n' {
                    state = ScanState::Normal;
                }
            }
            ScanState::BlockComment => {
                if character == '*'
                    && chars
                        .peek()
                        .is_some_and(|(_, next_character)| *next_character == '/')
                {
                    chars.next();
                    state = ScanState::Normal;
                }
            }
            ScanState::Bracketed => {
                last_code_index = Some(index);
                if character == ']' {
                    if chars
                        .peek()
                        .is_some_and(|(_, next_character)| *next_character == ']')
                    {
                        last_code_index = chars.next().map(|(next_index, _)| next_index);
                    } else {
                        state = ScanState::Normal;
                    }
                }
            }
            ScanState::Quoted(quote) => {
                last_code_index = Some(index);
                if character == '\\' {
                    if let Some((next_index, _)) = chars.next() {
                        last_code_index = Some(next_index);
                    }
                } else if character == quote {
                    if chars
                        .peek()
                        .is_some_and(|(_, next_character)| *next_character == quote)
                    {
                        last_code_index = chars.next().map(|(next_index, _)| next_index);
                    } else {
                        state = ScanState::Normal;
                    }
                }
            }
            ScanState::Normal => {
                let next_character = chars.peek().map(|(_, character)| *character);
                if character == '-' && next_character == Some('-') {
                    finish_word(&mut word, depth, &mut top_level_keywords);
                    chars.next();
                    state = ScanState::LineComment;
                    continue;
                }
                if character == '/' && next_character == Some('*') {
                    finish_word(&mut word, depth, &mut top_level_keywords);
                    chars.next();
                    state = ScanState::BlockComment;
                    continue;
                }

                if !character.is_whitespace() {
                    last_code_index = Some(index);
                }
                match character {
                    '\'' | '"' | '`' => {
                        finish_word(&mut word, depth, &mut top_level_keywords);
                        state = ScanState::Quoted(character);
                    }
                    '[' => {
                        finish_word(&mut word, depth, &mut top_level_keywords);
                        state = ScanState::Bracketed;
                    }
                    ';' => {
                        finish_word(&mut word, depth, &mut top_level_keywords);
                        semicolons.push(index);
                    }
                    '(' => {
                        finish_word(&mut word, depth, &mut top_level_keywords);
                        depth = depth.saturating_add(1);
                    }
                    ')' => {
                        word.clear();
                        depth = depth.saturating_sub(1);
                    }
                    character
                        if depth == 0
                            && (character.is_ascii_alphanumeric() || character == '_') =>
                    {
                        word.push(character);
                    }
                    _ => finish_word(&mut word, depth, &mut top_level_keywords),
                }
            }
        }
    }
    finish_word(&mut word, depth, &mut top_level_keywords);

    match state {
        ScanState::Normal | ScanState::LineComment => Some(SqlScan {
            semicolons,
            last_code_index,
            top_level_keywords,
        }),
        ScanState::Quoted(_) | ScanState::Bracketed | ScanState::BlockComment => None,
    }
}

fn finish_word(word: &mut String, depth: u32, keywords: &mut Vec<String>) {
    if depth == 0 && !word.is_empty() {
        keywords.push(std::mem::take(word));
    } else {
        word.clear();
    }
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
    let total_rows = (!server_paged).then_some(values.len());
    let has_more = if server_paged {
        values.len() > page_size as usize
    } else {
        offset.saturating_add(page_size as usize) < values.len()
    };
    if server_paged && has_more {
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
    let pagination = json!({
        "page": page,
        "page_size": page_size,
        "total_rows": total_rows,
        "has_more": has_more,
    });

    json!({
        "columns": columns,
        "rows": rows,
        "affected_rows": 0,
        "truncated": has_more,
        "pagination": pagination,
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
        contains_top_level_keyword, is_single_select, normalized_select, requested_page_size,
        tabularis_query_result,
    };

    #[test]
    fn reads_current_limit_with_legacy_page_size_fallback() {
        assert_eq!(requested_page_size(&json!({ "limit": 25 })), 25);
        assert_eq!(
            requested_page_size(&json!({ "limit": 25, "page_size": 50 })),
            25
        );
        assert_eq!(requested_page_size(&json!({ "page_size": 50 })), 50);
        assert_eq!(requested_page_size(&json!({})), 100);
    }

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
        assert!(normalized_select("SELECT 1 -- owner's result").is_some());
        assert!(normalized_select("SELECT 1; -- terminal semicolon").is_some());
        assert!(normalized_select("SELECT 1; DELETE FROM data.dog").is_none());
        assert!(normalized_select("SELECT 1 -- '\n; DELETE FROM data.dog -- '").is_none());
        assert!(normalized_select(r#"SELECT 'x\' AS a, '; DELETE FROM data.dog --'"#).is_none());
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
            "SELECT * FROM data.dog -- LIMIT 1",
            "limit"
        ));
        assert!(!contains_top_level_keyword(
            "SELECT * FROM data.dog /* LIMIT 1 */",
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
        assert_eq!(result["affected_rows"], 0);
        assert_eq!(result["truncated"], false);
        assert_eq!(
            result["pagination"],
            json!({
                "page": 1,
                "page_size": 100,
                "total_rows": 2,
                "has_more": false,
            })
        );
        assert_eq!(result["total_count"], 2);
        assert_eq!(result["execution_time_ms"], 12);
    }

    #[test]
    fn query_result_paginates_without_rewriting_sql() {
        let response = json!([{ "id": 1 }, { "id": 2 }, { "id": 3 }]);
        let first_page = tabularis_query_result(response.clone(), 1, 2, 0, false);
        let second_page = tabularis_query_result(response, 2, 2, 0, false);

        assert_eq!(first_page["rows"], json!([[1], [2]]));
        assert_eq!(first_page["truncated"], true);
        assert_eq!(first_page["pagination"]["total_rows"], 3);
        assert_eq!(first_page["pagination"]["has_more"], true);
        assert_eq!(second_page["rows"], json!([[3]]));
        assert_eq!(second_page["total_count"], 3);
        assert_eq!(second_page["truncated"], false);
        assert_eq!(second_page["pagination"]["total_rows"], 3);
        assert_eq!(second_page["pagination"]["has_more"], false);
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
        assert_eq!(result["truncated"], true);
        assert_eq!(
            result["pagination"],
            json!({
                "page": 2,
                "page_size": 2,
                "total_rows": null,
                "has_more": true,
            })
        );
    }
}
