use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::client::Client;
use crate::error::PluginError;
use crate::handlers::ddl;
use crate::models::{inner_params, ConnectionParams};
use crate::rpc::{not_implemented, result_response};

pub(crate) const MAX_UNBOUNDED_ROWS: u64 = 10_000;
const MAX_PAGE_SIZE: u64 = MAX_UNBOUNDED_ROWS;
const MAX_RESULT_COLUMNS: usize = 1_000;
const MAX_PADDING_CELLS: usize = 1_000_000;
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

pub fn test_connection(id: Value, params: &Value) -> Value {
    let result = client_with_timeout(params, PROBE_TIMEOUT)
        .and_then(|client| client.user_info())
        .map(|_| json!({ "success": true }));
    result_response(id, result)
}

pub fn ping(id: Value, params: &Value) -> Value {
    let result = client_with_timeout(params, PROBE_TIMEOUT)
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
    if let Some(result) = ddl::execute_ddl(params, query) {
        return result;
    }
    let query = normalized_statement(query)?;
    let kind = statement_kind(&query).ok_or_else(|| {
        PluginError::invalid_params(
            "the Harper driver accepts one SELECT, INSERT, UPDATE, or DELETE statement at a time",
        )
    })?;

    let page = params
        .get("page")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);
    let page_size = requested_page_size(params);
    let is_select = kind == "SELECT";
    if !is_select {
        require_qualified_write_target(&query, kind)?;
    }
    let has_paging_clause = ["limit", "offset", "fetch", "top"]
        .into_iter()
        .any(|keyword| contains_top_level_keyword(&query, keyword));
    let server_paged = is_select && page_size.is_some() && !has_paging_clause;
    let unbounded_needs_server_cap = is_select && page_size.is_none() && !has_paging_clause;
    let executed_query = match (page_size, server_paged, unbounded_needs_server_cap) {
        (Some(page_size), true, _) => {
            let offset = (page - 1).saturating_mul(page_size);
            format!("{query}\nLIMIT {} OFFSET {offset}", page_size + 1)
        }
        (None, _, true) => format!("{query}\nLIMIT {}", MAX_UNBOUNDED_ROWS + 1),
        _ => query,
    };

    let started = Instant::now();
    let client = client(params)?;
    let response = if is_select {
        client.sql(&executed_query)?
    } else {
        client.sql_mutation(&executed_query)?
    };
    let elapsed = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);

    if !is_select {
        return write_query_result(response, elapsed);
    }
    match page_size {
        Some(page_size) => tabularis_query_result(response, page, page_size, elapsed, server_paged),
        None => unbounded_query_result(response, elapsed, true),
    }
}

fn requested_page_size(params: &Value) -> Option<u64> {
    if params.get("limit") == Some(&Value::Null) {
        return None;
    }
    Some(
        params
            .get("limit")
            .and_then(Value::as_u64)
            .or_else(|| params.get("page_size").and_then(Value::as_u64))
            .unwrap_or(100)
            .clamp(1, MAX_PAGE_SIZE),
    )
}

fn client(params: &Value) -> Result<Client, PluginError> {
    Client::connect(ConnectionParams::from_value(inner_params(params)))
}

fn client_with_timeout(params: &Value, timeout: Duration) -> Result<Client, PluginError> {
    Client::connect_with_timeout(ConnectionParams::from_value(inner_params(params)), timeout)
}

fn statement_kind(query: &str) -> Option<&'static str> {
    let first = scan_sql(query)?.top_level_keywords.into_iter().next()?;
    ["SELECT", "INSERT", "UPDATE", "DELETE"]
        .into_iter()
        .find(|kind| first.eq_ignore_ascii_case(kind))
}

fn require_qualified_write_target(query: &str, kind: &str) -> Result<(), PluginError> {
    let mut rest = strip_leading_comments(query)?;
    rest = strip_sql_keyword(rest, kind)
        .ok_or_else(|| PluginError::invalid_params("could not locate the SQL write target"))?;
    if kind == "INSERT" {
        rest = strip_sql_keyword(rest, "INTO").ok_or_else(|| {
            PluginError::invalid_params("INSERT must include INTO followed by database.table")
        })?;
    } else if kind == "DELETE" {
        rest = strip_sql_keyword(rest, "FROM").ok_or_else(|| {
            PluginError::invalid_params("DELETE must include FROM followed by database.table")
        })?;
    }
    let rest = consume_sql_identifier(rest)
        .ok_or_else(|| PluginError::invalid_params("SQL write target must be database.table"))?;
    let rest = skip_sql_separators(rest)
        .ok_or_else(|| PluginError::invalid_params("SQL contains an unterminated comment"))?;
    let Some(rest) = rest.strip_prefix('.') else {
        return Err(PluginError::invalid_params(
            "SQL INSERT, UPDATE, and DELETE targets must be qualified as database.table",
        ));
    };
    if consume_sql_identifier(rest).is_none() {
        return Err(PluginError::invalid_params(
            "SQL write target must include a table after the database name",
        ));
    }
    Ok(())
}

fn strip_leading_comments(query: &str) -> Result<&str, PluginError> {
    skip_sql_separators(query)
        .ok_or_else(|| PluginError::invalid_params("SQL contains an unterminated comment"))
}

fn skip_sql_separators(mut input: &str) -> Option<&str> {
    loop {
        input = input.trim_start();
        if input.starts_with("--") {
            let comment = input
                .strip_prefix("--")
                .expect("line comment prefix was checked");
            input = comment
                .split_once('\n')
                .map_or("", |(_, remainder)| remainder);
            continue;
        }
        if let Some(comment) = input.strip_prefix("/*") {
            let (_, remainder) = comment.split_once("*/")?;
            input = remainder;
            continue;
        }
        return Some(input);
    }
}

fn strip_sql_keyword<'a>(input: &'a str, keyword: &str) -> Option<&'a str> {
    let input = skip_sql_separators(input)?;
    let prefix = input.get(..keyword.len())?;
    if !prefix.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = &input[keyword.len()..];
    (rest.is_empty()
        || rest.starts_with(char::is_whitespace)
        || rest.starts_with('`')
        || rest.starts_with("/*")
        || rest.starts_with("--"))
    .then_some(rest)
}

fn consume_sql_identifier(input: &str) -> Option<&str> {
    let input = skip_sql_separators(input)?;
    if let Some(mut rest) = input.strip_prefix('`') {
        loop {
            let index = rest.find('`')?;
            rest = &rest[index + 1..];
            if let Some(escaped) = rest.strip_prefix('`') {
                rest = escaped;
            } else {
                return Some(rest);
            }
        }
    }
    let end = input
        .char_indices()
        .take_while(|(_, character)| {
            character.is_alphanumeric() || *character == '_' || *character == '-'
        })
        .map(|(index, character)| index + character.len_utf8())
        .last()?;
    let identifier = &input[..end];
    if identifier.contains("--") {
        return None;
    }
    Some(&input[end..])
}

fn normalized_statement(query: &str) -> Result<String, PluginError> {
    let query = query.trim();
    let scan = scan_sql(query).ok_or_else(|| {
        PluginError::invalid_params("SQL contains an unterminated comment or quote")
    })?;
    if scan.semicolons.len() > 1 {
        return Err(PluginError::invalid_params(
            "the Harper driver accepts exactly one SQL statement",
        ));
    }

    let normalized = match scan.semicolons.first().copied() {
        Some(index) if scan.last_code_index == Some(index) => {
            let mut normalized = query.to_string();
            normalized.remove(index);
            normalized
        }
        Some(_) => {
            return Err(PluginError::invalid_params(
                "the Harper driver accepts exactly one SQL statement",
            ));
        }
        None => query.to_string(),
    };
    if normalized.is_empty() {
        return Err(PluginError::invalid_params("query is required"));
    }
    Ok(normalized)
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
) -> Result<Value, PluginError> {
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
    let total_count = if server_paged {
        offset
            .saturating_add(values.len())
            .saturating_add(usize::from(has_more))
    } else {
        values.len()
    };
    let start = if server_paged { 0 } else { offset };
    let page_values = values
        .into_iter()
        .skip(start)
        .take(page_size as usize)
        .collect::<Vec<_>>();
    let (columns, included_rows, _) = bounded_shape(&page_values, false)?;
    let rows = page_values
        .into_iter()
        .take(included_rows)
        .map(|value| row_values(value, &columns))
        .collect::<Vec<_>>();
    let pagination = json!({
        "page": page,
        "page_size": page_size,
        "total_rows": total_rows,
        "has_more": has_more,
    });

    Ok(json!({
        "columns": columns,
        "rows": rows,
        "affected_rows": 0,
        "truncated": has_more,
        "pagination": pagination,
        "total_count": total_count,
        "execution_time_ms": elapsed,
    }))
}

fn unbounded_query_result(
    response: Value,
    elapsed: u64,
    cap_enforced: bool,
) -> Result<Value, PluginError> {
    let mut values = match response {
        Value::Array(values) => values,
        Value::Null => Vec::new(),
        value => vec![value],
    };
    let mut truncated = cap_enforced && values.len() > MAX_UNBOUNDED_ROWS as usize;
    if truncated {
        values.truncate(MAX_UNBOUNDED_ROWS as usize);
    }
    let (columns, included_rows, shape_truncated) = bounded_shape(&values, true)?;
    truncated |= shape_truncated;
    let rows = values
        .into_iter()
        .take(included_rows)
        .map(|value| row_values(value, &columns))
        .collect::<Vec<_>>();
    Ok(json!({
        "columns": columns,
        "total_count": rows.len(),
        "rows": rows,
        "affected_rows": 0,
        "truncated": truncated,
        "pagination": null,
        "execution_time_ms": elapsed,
    }))
}

fn write_query_result(response: Value, elapsed: u64) -> Result<Value, PluginError> {
    let affected_rows = mutation_affected_rows(&response);
    let skipped = response
        .get("skipped_hashes")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if skipped > 0 {
        return Err(PluginError::connection(match affected_rows {
            Some(written) => format!(
                "Harper committed {written} record(s) and skipped {skipped} record(s); the committed records remain written"
            ),
            None => format!(
                "Harper skipped {skipped} record(s) and may have committed others; verify the database state before retrying"
            ),
        }));
    }
    let affected_rows = affected_rows.ok_or_else(|| {
        PluginError::connection(
            "Harper write response did not include a recognized affected-record count",
        )
    })?;
    Ok(json!({
        "columns": [],
        "rows": [],
        "affected_rows": affected_rows,
        "truncated": false,
        "pagination": null,
        "total_count": 0,
        "execution_time_ms": elapsed,
    }))
}

fn mutation_affected_rows(response: &Value) -> Option<usize> {
    [
        "inserted_hashes",
        "update_hashes",
        "deleted_hashes",
        "upserted_hashes",
        "put_hashes",
    ]
    .into_iter()
    .find_map(|field| response.get(field).and_then(Value::as_array).map(Vec::len))
    .or_else(|| {
        response
            .get("affected_rows")
            .and_then(Value::as_u64)
            .map(|affected_rows| affected_rows as usize)
    })
}

fn bounded_shape(
    values: &[Value],
    allow_truncate: bool,
) -> Result<(Vec<String>, usize, bool), PluginError> {
    let mut columns = Vec::<&str>::new();
    let mut seen = HashSet::<&str>::new();
    let mut present_fields = 0_usize;
    let mut included_rows = 0_usize;
    for value in values {
        let mut new_columns = Vec::new();
        let row_fields = match value {
            Value::Object(object) => {
                for column in object.keys() {
                    if !seen.contains(column.as_str()) {
                        new_columns.push(column.as_str());
                    }
                }
                object.len()
            }
            _ => {
                if !seen.contains("value") {
                    new_columns.push("value");
                }
                1
            }
        };
        let candidate_column_count = columns
            .len()
            .checked_add(new_columns.len())
            .ok_or_else(|| PluginError::connection("Harper result shape overflowed"))?;
        let candidate_row_count = included_rows
            .checked_add(1)
            .ok_or_else(|| PluginError::connection("Harper result shape overflowed"))?;
        let candidate_present = present_fields
            .checked_add(row_fields)
            .ok_or_else(|| PluginError::connection("Harper result shape overflowed"))?;
        let padding_cells = candidate_row_count
            .checked_mul(candidate_column_count)
            .and_then(|cells| cells.checked_sub(candidate_present))
            .ok_or_else(|| PluginError::connection("Harper result shape overflowed"))?;
        if candidate_column_count > MAX_RESULT_COLUMNS || padding_cells > MAX_PADDING_CELLS {
            if allow_truncate && included_rows > 0 {
                return Ok((
                    columns.into_iter().map(str::to_string).collect(),
                    included_rows,
                    true,
                ));
            }
            return Err(PluginError::connection(format!(
                "Harper result page is too sparse to display safely ({candidate_column_count} columns, {padding_cells} empty cells); select fewer columns or use a smaller page size"
            )));
        }
        for column in new_columns {
            seen.insert(column);
            columns.push(column);
        }
        present_fields = candidate_present;
        included_rows = candidate_row_count;
    }
    Ok((
        columns.into_iter().map(str::to_string).collect(),
        included_rows,
        false,
    ))
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
    use serde_json::{json, Value};

    use super::{
        bounded_shape, contains_top_level_keyword, normalized_statement, requested_page_size,
        require_qualified_write_target, statement_kind, tabularis_query_result,
        unbounded_query_result, write_query_result, MAX_PADDING_CELLS, MAX_RESULT_COLUMNS,
        MAX_UNBOUNDED_ROWS,
    };

    #[test]
    fn reads_current_limit_with_legacy_page_size_fallback() {
        assert_eq!(requested_page_size(&json!({ "limit": 25 })), Some(25));
        assert_eq!(
            requested_page_size(&json!({ "limit": 25, "page_size": 50 })),
            Some(25)
        );
        assert_eq!(requested_page_size(&json!({ "page_size": 50 })), Some(50));
        assert_eq!(requested_page_size(&json!({})), Some(100));
        assert_eq!(requested_page_size(&json!({ "limit": null })), None);
        assert_eq!(
            requested_page_size(&json!({ "limit": 50_000 })),
            Some(10_000)
        );
    }

    #[test]
    fn accepts_one_supported_statement() {
        assert_eq!(
            statement_kind(&normalized_statement(" SELECT * FROM data.dog; ").unwrap()),
            Some("SELECT")
        );
        assert_eq!(
            normalized_statement("SELECT\n  *\nFROM data.dog").unwrap(),
            "SELECT\n  *\nFROM data.dog"
        );
        assert!(normalized_statement("SELECT ';' AS punctuation").is_ok());
        assert!(normalized_statement("SELECT 1 -- owner's result").is_ok());
        assert_eq!(
            normalized_statement("SELECT 1; -- terminal semicolon").unwrap(),
            "SELECT 1 -- terminal semicolon"
        );
        assert!(normalized_statement("SELECT 1; DELETE FROM data.dog").is_err());
        assert!(normalized_statement("SELECT 1 -- '\n; DELETE FROM data.dog -- '").is_err());
        assert!(normalized_statement(r#"SELECT 'x\' AS a, '; DELETE FROM data.dog --'"#).is_err());
        assert_eq!(statement_kind("DELETE FROM data.dog"), Some("DELETE"));
        assert_eq!(
            statement_kind("-- comment\nSELECT * FROM data.dog"),
            Some("SELECT")
        );
        assert_eq!(statement_kind("CREATE TABLE dog (id INT)"), None);
    }

    #[test]
    fn sql_writes_require_database_qualified_targets() {
        for (query, kind) in [
            ("INSERT INTO data.dog (id) VALUES (1)", "INSERT"),
            ("INSERT /* hint */ INTO data.dog (id) VALUES (1)", "INSERT"),
            ("INSERT/* hint */INTO/**/data.dog (id) VALUES (1)", "INSERT"),
            ("UPDATE `data`.`dog` SET name = 'Rover'", "UPDATE"),
            ("UPDATE/* audit */data.dog SET name = 'Rover'", "UPDATE"),
            ("UPDATE--audit\ndata.dog SET name = 'Rover'", "UPDATE"),
            ("-- scoped\nDELETE FROM data.dog WHERE id = 1", "DELETE"),
            ("--scoped\nDELETE FROM data.dog WHERE id = 1", "DELETE"),
            ("DELETE/**/FROM/**/data.dog WHERE id = 1", "DELETE"),
        ] {
            assert!(
                require_qualified_write_target(query, kind).is_ok(),
                "{query}"
            );
        }
        for (query, kind) in [
            ("INSERT INTO dog (id) VALUES (1)", "INSERT"),
            ("UPDATE dog SET name = 'Rover'", "UPDATE"),
            ("DELETE FROM dog WHERE id = 1", "DELETE"),
            ("INSERT INTO dog--.x\n(id) VALUES (1)", "INSERT"),
        ] {
            assert!(
                require_qualified_write_target(query, kind).is_err(),
                "{query}"
            );
        }
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
        let result = tabularis_query_result(response, 1, 100, 12, false).unwrap();

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
        let first_page = tabularis_query_result(response.clone(), 1, 2, 0, false).unwrap();
        let second_page = tabularis_query_result(response, 2, 2, 0, false).unwrap();

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
    fn client_paging_discovers_columns_only_from_the_visible_page() {
        let response = json!([{ "first": 1 }, { "second": 2 }]);
        let result = tabularis_query_result(response, 1, 1, 0, false).unwrap();

        assert_eq!(result["columns"], json!(["first"]));
        assert_eq!(result["rows"], json!([[1]]));
    }

    #[test]
    fn sparse_shape_limits_columns_and_padding_before_materialization() {
        let at_column_limit = (0..MAX_RESULT_COLUMNS)
            .map(|index| (format!("field_{index}"), json!(index)))
            .collect::<serde_json::Map<_, _>>();
        assert!(bounded_shape(&[Value::Object(at_column_limit.clone())], false).is_ok());

        let mut over_column_limit = at_column_limit.clone();
        over_column_limit.insert("one_too_many".to_string(), json!(true));
        assert!(bounded_shape(&[Value::Object(over_column_limit)], false).is_err());

        let mut sparse_rows = vec![Value::Object(at_column_limit)];
        sparse_rows.extend(
            (0..=(MAX_PADDING_CELLS / MAX_RESULT_COLUMNS))
                .map(|_| Value::Object(serde_json::Map::new())),
        );
        let (_, included_rows, truncated) = bounded_shape(&sparse_rows, true).unwrap();
        assert_eq!(included_rows, 1 + MAX_PADDING_CELLS / MAX_RESULT_COLUMNS);
        assert!(truncated);
        assert!(bounded_shape(&sparse_rows, false).is_err());
    }

    #[test]
    fn object_responses_are_displayed_as_one_row() {
        let result =
            tabularis_query_result(json!({ "message": "ok", "count": 1 }), 1, 100, 0, false)
                .unwrap();
        assert_eq!(result["columns"], json!(["message", "count"]));
        assert_eq!(result["rows"], json!([["ok", 1]]));
    }

    #[test]
    fn server_paging_uses_an_extra_row_as_a_next_page_signal() {
        let response = json!([{ "id": 1 }, { "id": 2 }, { "id": 3 }]);
        let result = tabularis_query_result(response, 2, 2, 0, true).unwrap();

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

    #[test]
    fn unbounded_results_are_capped_and_have_no_pagination() {
        let rows = (0..=MAX_UNBOUNDED_ROWS)
            .map(|id| json!({ "id": id }))
            .collect();
        let result = unbounded_query_result(serde_json::Value::Array(rows), 3, true).unwrap();

        assert_eq!(
            result["rows"].as_array().unwrap().len(),
            MAX_UNBOUNDED_ROWS as usize
        );
        assert_eq!(result["truncated"], true);
        assert_eq!(result["pagination"], serde_json::Value::Null);
    }

    #[test]
    fn write_results_use_harper_hash_arrays() {
        let result =
            write_query_result(json!({ "update_hashes": [1, 2], "skipped_hashes": [] }), 4)
                .unwrap();
        assert_eq!(result["affected_rows"], 2);
        assert_eq!(result["rows"], json!([]));
        assert!(
            write_query_result(json!({ "update_hashes": [1], "skipped_hashes": [2] }), 4)
                .unwrap_err()
                .message
                .contains("committed 1 record(s) and skipped 1")
        );
        assert!(write_query_result(json!({ "message": "ok" }), 0).is_err());
    }
}
