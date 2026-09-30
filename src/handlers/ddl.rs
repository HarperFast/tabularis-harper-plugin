use std::borrow::Cow;
use std::fmt::Write as _;

use serde_json::{json, Value};

use crate::client::Client;
use crate::error::PluginError;
use crate::handlers::crud::invalidate_primary_key_cache;
use crate::handlers::metadata::database;
use crate::models::{inner_params, ConnectionParams};
use crate::rpc::{not_implemented, result_response};

const COMMAND_PREFIX: &str = "/*tabularis-harper:";
const COMMAND_SUFFIX: &str = "*/";

pub fn get_create_table_sql(id: Value, params: &Value) -> Value {
    result_response(id, create_table_sql(params).map(|sql| json!([sql])))
}

pub fn get_add_column_sql(id: Value, params: &Value) -> Value {
    result_response(id, add_column_sql(params).map(|sql| json!([sql])))
}

pub fn get_alter_column_sql(id: Value, _params: &Value) -> Value {
    result_response(
        id,
        Err(PluginError::invalid_params(
            "Harper does not support atomic attribute rename or type alteration",
        )),
    )
}

pub fn get_create_index_sql(id: Value, _params: &Value) -> Value {
    result_response(
        id,
        Err(PluginError::invalid_params(
            "Harper manages per-attribute indexes automatically and does not support named, unique, or compound index creation",
        )),
    )
}

pub fn get_create_foreign_key_sql(id: Value, _params: &Value) -> Value {
    not_implemented(id, "get_create_foreign_key_sql")
}

pub fn drop_index(id: Value, _params: &Value) -> Value {
    result_response(
        id,
        Err(PluginError::invalid_params(
            "Harper-managed attribute indexes cannot be dropped through the Operations API",
        )),
    )
}

pub fn drop_foreign_key(id: Value, _params: &Value) -> Value {
    not_implemented(id, "drop_foreign_key")
}

pub(crate) fn execute_ddl(params: &Value, query: &str) -> Option<Result<Value, PluginError>> {
    if let Some(command) = decode_command(query) {
        return Some(command.and_then(|command| execute_command(params, &command)));
    }
    let query = match ddl_dispatch_query(query) {
        Ok(Some(query)) => query,
        Ok(None) => return None,
        Err(error) => return Some(Err(error)),
    };
    let query = query.as_ref();
    if starts_with_keyword(query, "DROP TABLE") {
        return Some(parse_drop_table(query).and_then(|(parsed_database, table, if_exists)| {
            let database = parsed_database.or_else(|| database(params).ok());
            let database = database.ok_or_else(|| {
                PluginError::invalid_params(
                    "DROP TABLE must qualify the Harper database when multiple databases are selected",
                )
            })?;
            let client = client(params)?;
            if if_exists {
                let description = client.describe_database(&database)?;
                let exists = description
                    .as_object()
                    .ok_or_else(|| {
                        PluginError::connection(
                            "Harper returned an invalid database description",
                        )
                    })?
                    .contains_key(&table);
                if !exists {
                    return Ok(empty_result());
                }
            }
            client.drop_table(&database, &table)?;
            invalidate_primary_key_cache(params, &database, &table);
            Ok(empty_result())
        }));
    }
    if starts_with_keyword(query, "ALTER TABLE") {
        return Some(parse_drop_column(query).and_then(|(parsed_database, table, attribute)| {
            let database = parsed_database.or_else(|| database(params).ok());
            let database = database.ok_or_else(|| {
                PluginError::invalid_params(
                    "ALTER TABLE must qualify the Harper database when multiple databases are selected",
                )
            })?;
            let client = client(params)?;
            let description = client.describe_table(&database, &table)?;
            let schema_defined = description
                .get("schema_defined")
                .and_then(Value::as_bool)
                .ok_or_else(|| {
                    PluginError::connection(
                        "Harper table description did not identify whether the schema is defined",
                    )
                })?;
            if schema_defined {
                return Err(PluginError::invalid_params(
                    "Harper retains stored values when an attribute is dropped from a schema-defined table; refusing the operation because it would not erase the column data",
                ));
            }
            client.drop_attribute(&database, &table, &attribute)?;
            invalidate_primary_key_cache(params, &database, &table);
            Ok(empty_result())
        }));
    }
    None
}

fn ddl_dispatch_query(query: &str) -> Result<Option<Cow<'_, str>>, PluginError> {
    if starts_with_keyword(query, "DROP TABLE") || starts_with_keyword(query, "ALTER TABLE") {
        return Ok(Some(Cow::Borrowed(query)));
    }
    let first_code = strip_leading_ddl_comments(query)?;
    if first_code.starts_with(COMMAND_PREFIX) {
        return Err(PluginError::invalid_params(
            "generated Harper DDL commands must not be preceded by comments",
        ));
    }
    if !starts_with_ddl_prefix(first_code, "DROP") && !starts_with_ddl_prefix(first_code, "ALTER") {
        return Ok(None);
    }
    let uncommented = without_sql_comments(query)?;
    if starts_with_keyword(&uncommented, "DROP TABLE")
        || starts_with_keyword(&uncommented, "ALTER TABLE")
    {
        Ok(Some(Cow::Owned(uncommented)))
    } else {
        Ok(None)
    }
}

fn starts_with_ddl_prefix(input: &str, keyword: &str) -> bool {
    let input = input.trim_start();
    let Some(prefix) = input.get(..keyword.len()) else {
        return false;
    };
    if !prefix.eq_ignore_ascii_case(keyword) {
        return false;
    }
    let rest = &input[keyword.len()..];
    rest.is_empty()
        || rest.starts_with(char::is_whitespace)
        || rest.starts_with('`')
        || rest.starts_with("/*")
        || starts_mysql_line_comment(rest)
}

fn strip_leading_ddl_comments(mut input: &str) -> Result<&str, PluginError> {
    loop {
        input = input.trim_start();
        if input.starts_with(COMMAND_PREFIX) {
            return Ok(input);
        }
        if let Some(comment) = input.strip_prefix("/*") {
            let (_, remainder) = comment.split_once("*/").ok_or_else(|| {
                PluginError::invalid_params("schema change contains an unterminated comment")
            })?;
            input = remainder;
            continue;
        }
        if starts_mysql_line_comment(input) {
            let comment = input
                .strip_prefix("--")
                .expect("line comment prefix was checked");
            input = comment
                .split_once('\n')
                .map_or("", |(_, remainder)| remainder);
            continue;
        }
        return Ok(input);
    }
}

fn starts_mysql_line_comment(input: &str) -> bool {
    input
        .strip_prefix("--")
        .is_some_and(|rest| rest.chars().next().is_none_or(char::is_whitespace))
}

fn create_table_sql(params: &Value) -> Result<String, PluginError> {
    let database = database(params)?;
    reject_host_rewritten_identifier(&database)?;
    let table = required_string(params, "table_name")?;
    reject_host_rewritten_identifier(table)?;
    let columns = params
        .get("columns")
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::invalid_params("columns must be an array"))?;
    if columns.is_empty() {
        return Err(PluginError::invalid_params(
            "Harper tables require at least one column",
        ));
    }

    let mut attributes = Vec::with_capacity(columns.len());
    let mut primary_key = None;
    for column in columns {
        let name = required_string(column, "name")?;
        reject_host_rewritten_identifier(name)?;
        let data_type = required_string(column, "data_type")?;
        let native_type = harper_type(data_type)?;
        let is_primary = column.get("is_pk").and_then(Value::as_bool) == Some(true);
        let nullable = column
            .get("is_nullable")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let auto_increment = column
            .get("is_auto_increment")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if column
            .get("default_value")
            .is_some_and(|value| !value.is_null() && value.as_str() != Some(""))
        {
            return Err(PluginError::invalid_params(format!(
                "Harper table creation does not support a default value for '{name}'"
            )));
        }
        if is_primary && primary_key.replace(name.to_string()).is_some() {
            return Err(PluginError::invalid_params(
                "Harper tables require exactly one primary key",
            ));
        }
        if auto_increment && (!is_primary || !matches!(native_type, None | Some("Int" | "Long"))) {
            return Err(PluginError::invalid_params(format!(
                "auto increment is supported only for an INTEGER, LONG, or ANY Harper primary key ('{name}')"
            )));
        }

        let mut attribute = json!({
            "name": name,
            "indexed": true,
            "nullable": nullable || auto_increment,
        });
        if let Some(native_type) = native_type {
            attribute["type"] = Value::String(native_type.to_string());
        }
        if is_primary {
            attribute["is_primary_key"] = Value::Bool(true);
        }
        if auto_increment {
            attribute["auto_increment"] = Value::Bool(true);
        }
        attributes.push(attribute);
    }
    let primary_key = primary_key.ok_or_else(|| {
        PluginError::invalid_params("Harper tables require exactly one primary key")
    })?;
    let command = json!({
        "kind": "create_table",
        "database": database,
        "table": table,
        "primary_key": primary_key,
        "attributes": attributes,
    });
    let preview = preview_for_command(&command)?;
    Ok(encode_command(&command, &preview))
}

fn add_column_sql(params: &Value) -> Result<String, PluginError> {
    let database = database(params)?;
    reject_host_rewritten_identifier(&database)?;
    let table = required_string(params, "table")?;
    reject_host_rewritten_identifier(table)?;
    let column = params
        .get("column")
        .ok_or_else(|| PluginError::invalid_params("column is required"))?;
    let attribute = required_string(column, "name")?;
    reject_host_rewritten_identifier(attribute)?;
    let data_type = required_string(column, "data_type")?;
    if harper_type(data_type)?.is_some() {
        return Err(PluginError::invalid_params(
            "Harper create_attribute cannot enforce a type; choose ANY when adding a column",
        ));
    }
    if column.get("is_nullable").and_then(Value::as_bool) == Some(false) {
        return Err(PluginError::invalid_params(
            "Harper create_attribute cannot enforce NOT NULL",
        ));
    }
    if column.get("is_pk").and_then(Value::as_bool) == Some(true)
        || column.get("is_auto_increment").and_then(Value::as_bool) == Some(true)
        || column
            .get("default_value")
            .is_some_and(|value| !value.is_null() && value.as_str() != Some(""))
    {
        return Err(PluginError::invalid_params(
            "Harper added attributes cannot change the primary key, auto increment, or defaults",
        ));
    }
    let command = json!({
        "kind": "create_attribute",
        "database": database,
        "table": table,
        "attribute": attribute,
    });
    let preview = preview_for_command(&command)?;
    Ok(encode_command(&command, &preview))
}

fn execute_command(params: &Value, command: &Value) -> Result<Value, PluginError> {
    let kind = required_string(command, "kind")?;
    let database = required_string(command, "database")?;
    let table = required_string(command, "table")?;
    let client = client(params)?;
    match kind {
        "create_table" => {
            let primary_key = required_string(command, "primary_key")?;
            let attributes = command
                .get("attributes")
                .and_then(Value::as_array)
                .ok_or_else(|| PluginError::invalid_params("attributes must be an array"))?;
            let attributes = sanitize_generated_attributes(attributes, primary_key)?;
            client.create_table(database, table, primary_key, attributes)?;
            invalidate_primary_key_cache(params, database, table);
        }
        "create_attribute" => {
            let attribute = required_string(command, "attribute")?;
            client.create_attribute(database, table, attribute)?;
            invalidate_primary_key_cache(params, database, table);
        }
        _ => {
            return Err(PluginError::invalid_params(
                "unsupported generated Harper DDL command",
            ));
        }
    }
    Ok(empty_result())
}

fn sanitize_generated_attributes(
    attributes: &[Value],
    primary_key: &str,
) -> Result<Vec<Value>, PluginError> {
    let mut sanitized = Vec::with_capacity(attributes.len());
    let mut primary_key_seen = false;
    for attribute in attributes {
        let name = required_string(attribute, "name")?;
        let nullable = attribute
            .get("nullable")
            .and_then(Value::as_bool)
            .ok_or_else(|| PluginError::invalid_params("attribute nullable must be a boolean"))?;
        let is_primary = attribute
            .get("is_primary_key")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let auto_increment = attribute
            .get("auto_increment")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let native_type = attribute.get("type").and_then(Value::as_str);
        if auto_increment && (!is_primary || !matches!(native_type, None | Some("Int" | "Long"))) {
            return Err(PluginError::invalid_params(
                "generated Harper DDL has auto increment on an unsupported primary-key type",
            ));
        }
        if is_primary {
            if name != primary_key || primary_key_seen {
                return Err(PluginError::invalid_params(
                    "generated Harper DDL has inconsistent primary-key attributes",
                ));
            }
            primary_key_seen = true;
        }
        if native_type.is_some_and(|native_type| {
            !matches!(
                native_type,
                "Int"
                    | "Long"
                    | "Float"
                    | "BigInt"
                    | "String"
                    | "Boolean"
                    | "Date"
                    | "Bytes"
                    | "Blob"
            )
        }) {
            return Err(PluginError::invalid_params(
                "generated Harper DDL contains an unsupported attribute type",
            ));
        }
        let mut clean = json!({
            "name": name,
            "indexed": true,
            "nullable": nullable || auto_increment,
        });
        if let Some(native_type) = native_type {
            clean["type"] = Value::String(native_type.to_string());
        }
        if is_primary {
            clean["is_primary_key"] = Value::Bool(true);
        }
        sanitized.push(clean);
    }
    if !primary_key_seen {
        return Err(PluginError::invalid_params(
            "generated Harper DDL is missing its primary-key attribute",
        ));
    }
    Ok(sanitized)
}

fn preview_for_command(command: &Value) -> Result<String, PluginError> {
    let kind = required_string(command, "kind")?;
    let database = required_string(command, "database")?;
    let table = required_string(command, "table")?;
    match kind {
        "create_table" => {
            let primary_key = required_string(command, "primary_key")?;
            let raw_attributes = command
                .get("attributes")
                .and_then(Value::as_array)
                .ok_or_else(|| PluginError::invalid_params("attributes must be an array"))?;
            let attributes = sanitize_generated_attributes(raw_attributes, primary_key)?;
            let mut columns = Vec::with_capacity(attributes.len());
            for (raw, attribute) in raw_attributes.iter().zip(attributes.iter()) {
                let name = required_string(attribute, "name")?;
                let native_type = attribute.get("type").and_then(Value::as_str);
                let nullable = attribute
                    .get("nullable")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let is_primary = attribute
                    .get("is_primary_key")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let auto_increment = raw
                    .get("auto_increment")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut column =
                    format!("{} {}", quote_identifier(name), preview_type(native_type));
                if !nullable {
                    column.push_str(" NOT NULL");
                }
                if is_primary {
                    column.push_str(" PRIMARY KEY");
                }
                if auto_increment {
                    column.push_str(" AUTO_INCREMENT");
                }
                columns.push(column);
            }
            Ok(format!(
                "CREATE TABLE {} ({})",
                quote_table(database, table),
                columns.join(", ")
            ))
        }
        "create_attribute" => {
            let attribute = required_string(command, "attribute")?;
            Ok(format!(
                "ALTER TABLE {} ADD COLUMN {} ANY",
                quote_table(database, table),
                quote_identifier(attribute)
            ))
        }
        _ => Err(PluginError::invalid_params(
            "unsupported generated Harper DDL command",
        )),
    }
}

fn harper_type(data_type: &str) -> Result<Option<&'static str>, PluginError> {
    let base = data_type
        .split_once('(')
        .map_or(data_type, |(base, _)| base)
        .trim()
        .to_ascii_uppercase();
    match base.as_str() {
        "ANY" => Ok(None),
        "INT" | "INTEGER" => Ok(Some("Int")),
        "LONG" => Ok(Some("Long")),
        "FLOAT" | "DOUBLE" | "NUMBER" => Ok(Some("Float")),
        "BIGINT" => Ok(Some("BigInt")),
        "TEXT" | "STRING" | "VARCHAR" => Ok(Some("String")),
        "BOOLEAN" | "BOOL" => Ok(Some("Boolean")),
        "DATE" | "DATETIME" | "TIMESTAMP" => Ok(Some("Date")),
        "BYTES" | "BINARY" => Ok(Some("Bytes")),
        "BLOB" => Ok(Some("Blob")),
        _ => Err(PluginError::invalid_params(format!(
            "unsupported Harper type '{data_type}'"
        ))),
    }
}

fn preview_type(native_type: Option<&str>) -> &'static str {
    match native_type {
        None => "ANY",
        Some("Int") => "INTEGER",
        Some("Long") => "LONG",
        Some("Float") => "FLOAT",
        Some("BigInt") => "BIGINT",
        Some("String") => "TEXT",
        Some("Boolean") => "BOOLEAN",
        Some("Date") => "DATE",
        Some("Bytes") => "BYTES",
        Some("Blob") => "BLOB",
        Some(_) => "ANY",
    }
}

fn parse_drop_table(query: &str) -> Result<(Option<String>, String, bool), PluginError> {
    let query = one_statement(query)?;
    let rest = strip_keyword(&query, "DROP TABLE")
        .ok_or_else(|| PluginError::invalid_params("expected a single DROP TABLE statement"))?;
    let (rest, if_exists) = match strip_keyword(rest, "IF EXISTS") {
        Some(rest) => (rest, true),
        None => (rest, false),
    };
    let (database, table, rest) = parse_table_reference(rest)?;
    ensure_empty(rest)?;
    Ok((database, table, if_exists))
}

fn parse_drop_column(query: &str) -> Result<(Option<String>, String, String), PluginError> {
    let query = one_statement(query)?;
    let rest = strip_keyword(&query, "ALTER TABLE")
        .ok_or_else(|| PluginError::invalid_params("expected a single ALTER TABLE statement"))?;
    let (database, table, rest) = parse_table_reference(rest)?;
    let rest = strip_keyword(rest, "DROP COLUMN").ok_or_else(|| {
        PluginError::invalid_params("Harper supports only ALTER TABLE ... DROP COLUMN")
    })?;
    let (attribute, rest) = parse_identifier(rest)?;
    ensure_empty(rest)?;
    Ok((database, table, attribute))
}

fn parse_table_reference(input: &str) -> Result<(Option<String>, String, &str), PluginError> {
    let (first, rest) = parse_identifier(input)?;
    let trimmed = rest.trim_start();
    if let Some(after_dot) = trimmed.strip_prefix('.') {
        let (table, rest) = parse_identifier(after_dot)?;
        Ok((Some(first), table, rest))
    } else {
        Ok((None, first, rest))
    }
}

fn parse_identifier(input: &str) -> Result<(String, &str), PluginError> {
    let input = input.trim_start();
    if let Some(mut rest) = input.strip_prefix('`') {
        let mut value = String::new();
        loop {
            let Some(index) = rest.find('`') else {
                return Err(PluginError::invalid_params(
                    "unterminated quoted identifier",
                ));
            };
            value.push_str(&rest[..index]);
            rest = &rest[index + 1..];
            if let Some(next) = rest.strip_prefix('`') {
                value.push('`');
                rest = next;
            } else {
                return Ok((value, rest));
            }
        }
    }
    let end = input
        .char_indices()
        .take_while(|(_, character)| {
            character.is_alphanumeric() || *character == '_' || *character == '-'
        })
        .map(|(index, character)| index + character.len_utf8())
        .last()
        .unwrap_or(0);
    if end == 0 {
        return Err(PluginError::invalid_params("identifier is required"));
    }
    Ok((input[..end].to_string(), &input[end..]))
}

fn one_statement(query: &str) -> Result<String, PluginError> {
    let query = without_sql_comments(query)?;
    let query = query.trim();
    let mut quoted_identifier = false;
    let mut semicolons = Vec::new();
    let mut characters = query.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        if character == '`' {
            if quoted_identifier
                && characters
                    .peek()
                    .is_some_and(|(_, next_character)| *next_character == '`')
            {
                characters.next();
            } else {
                quoted_identifier = !quoted_identifier;
            }
        } else if character == ';' && !quoted_identifier {
            semicolons.push(index);
        }
    }
    if semicolons.len() > 1
        || semicolons
            .first()
            .is_some_and(|index| *index != query.len() - 1)
    {
        return Err(PluginError::invalid_params(
            "schema changes accept exactly one statement",
        ));
    }
    Ok(semicolons
        .first()
        .map_or(query, |index| query[..*index].trim_end())
        .to_string())
}

fn without_sql_comments(query: &str) -> Result<String, PluginError> {
    let mut output = String::with_capacity(query.len());
    let mut characters = query.chars().peekable();
    let mut quoted_identifier = false;
    while let Some(character) = characters.next() {
        if character == '`' {
            output.push(character);
            if quoted_identifier && characters.peek() == Some(&'`') {
                output.push(characters.next().expect("peeked character exists"));
            } else {
                quoted_identifier = !quoted_identifier;
            }
            continue;
        }
        if !quoted_identifier && character == '-' && characters.peek() == Some(&'-') {
            let mut lookahead = characters.clone();
            lookahead.next();
            if lookahead
                .peek()
                .is_none_or(|character| character.is_whitespace())
            {
                characters.next();
                output.push(' ');
                for character in characters.by_ref() {
                    if character == '\n' {
                        output.push('\n');
                        break;
                    }
                }
                continue;
            }
        }
        if !quoted_identifier && character == '/' && characters.peek() == Some(&'*') {
            characters.next();
            output.push(' ');
            let mut closed = false;
            while let Some(character) = characters.next() {
                if character == '*' && characters.peek() == Some(&'/') {
                    characters.next();
                    closed = true;
                    break;
                }
            }
            if !closed {
                return Err(PluginError::invalid_params(
                    "schema change contains an unterminated comment",
                ));
            }
            continue;
        }
        output.push(character);
    }
    if quoted_identifier {
        return Err(PluginError::invalid_params(
            "schema change contains an unterminated quoted identifier",
        ));
    }
    Ok(output)
}

fn starts_with_keyword(query: &str, keyword: &str) -> bool {
    strip_keyword(query.trim_start(), keyword).is_some()
}

fn strip_keyword<'a>(input: &'a str, keyword: &str) -> Option<&'a str> {
    let mut input = input.trim_start();
    for (index, word) in keyword.split_whitespace().enumerate() {
        if index > 0 {
            let trimmed = input.trim_start();
            if trimmed.len() == input.len() {
                return None;
            }
            input = trimmed;
        }
        let prefix = input.get(..word.len())?;
        if !prefix.eq_ignore_ascii_case(word) {
            return None;
        }
        input = &input[word.len()..];
    }
    (input.is_empty() || input.starts_with(char::is_whitespace) || input.starts_with('`'))
        .then_some(input)
}

fn ensure_empty(rest: &str) -> Result<(), PluginError> {
    if rest.trim().is_empty() {
        Ok(())
    } else {
        Err(PluginError::invalid_params(
            "unsupported tokens after Harper schema change",
        ))
    }
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, PluginError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PluginError::invalid_params(format!("{field} is required")))
}

fn quote_identifier(identifier: &str) -> String {
    format!("`{}`", identifier.replace('`', "``"))
}

fn quote_table(database: &str, table: &str) -> String {
    format!("{}.{}", quote_identifier(database), quote_identifier(table))
}

fn reject_host_rewritten_identifier(identifier: &str) -> Result<(), PluginError> {
    if identifier
        .chars()
        .any(|character| matches!(character, '\u{2018}' | '\u{2019}' | '\u{201c}' | '\u{201d}'))
    {
        return Err(PluginError::invalid_params(format!(
            "identifier '{identifier}' contains a typographic quote that Tabularis rewrites before execution"
        )));
    }
    Ok(())
}

fn encode_command(command: &Value, preview: &str) -> String {
    let command = command.to_string();
    let mut encoded = String::with_capacity(command.len() * 2);
    for byte in command.bytes() {
        write!(&mut encoded, "{byte:02x}").expect("writing to a string cannot fail");
    }
    format!("{COMMAND_PREFIX}{encoded}{COMMAND_SUFFIX}\n{preview}")
}

fn decode_command(query: &str) -> Option<Result<Value, PluginError>> {
    let query = query.trim_start();
    let (encoded, visible_preview) = query
        .strip_prefix(COMMAND_PREFIX)?
        .split_once(COMMAND_SUFFIX)?;
    Some((|| {
        if encoded.len() % 2 != 0 || !encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(PluginError::invalid_params(
                "invalid generated Harper DDL command",
            ));
        }
        let bytes = encoded
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let pair = std::str::from_utf8(pair).expect("ASCII hex was validated");
                u8::from_str_radix(pair, 16).expect("hex pair was validated")
            })
            .collect::<Vec<_>>();
        let command: Value = serde_json::from_slice(&bytes).map_err(|_| {
            PluginError::invalid_params("invalid generated Harper DDL command payload")
        })?;
        let expected_preview = preview_for_command(&command)?;
        if normalize_preview(visible_preview) != normalize_preview(&expected_preview) {
            return Err(PluginError::invalid_params(
                "the generated Harper DDL preview was edited; regenerate the schema change before executing it",
            ));
        }
        Ok(command)
    })())
}

fn normalize_preview(preview: &str) -> String {
    let mut normalized = String::new();
    let mut quoted = None;
    let mut pending_space = false;
    let mut characters = preview.trim().chars().peekable();
    while let Some(character) = characters.next() {
        if let Some(quote) = quoted {
            normalized.push(character);
            if character == quote {
                if characters
                    .peek()
                    .is_some_and(|next_character| *next_character == quote)
                {
                    normalized.push(characters.next().expect("peeked character exists"));
                } else {
                    quoted = None;
                }
            }
            continue;
        }
        if matches!(character, '`' | '\'' | '"') {
            if pending_space && !normalized.is_empty() {
                normalized.push(' ');
            }
            pending_space = false;
            normalized.push(character);
            quoted = Some(character);
        } else if character.is_whitespace() {
            pending_space = true;
        } else {
            if pending_space && !normalized.is_empty() {
                normalized.push(' ');
            }
            pending_space = false;
            normalized.push(character);
        }
    }
    normalized
}

fn client(params: &Value) -> Result<Client, PluginError> {
    Client::connect(ConnectionParams::from_value(inner_params(params)))
}

fn empty_result() -> Value {
    json!({
        "columns": [],
        "rows": [],
        "affected_rows": 0,
        "truncated": false,
        "pagination": null,
        "total_count": 0,
        "execution_time_ms": 0,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        add_column_sql, create_table_sql, decode_command, encode_command, execute_ddl,
        parse_drop_column, parse_drop_table, sanitize_generated_attributes,
    };

    #[test]
    fn generated_create_table_round_trips_types_and_quoted_names() {
        let sql = create_table_sql(&json!({
            "params": { "database": "data" },
            "table_name": "odd`table",
            "columns": [
                { "name": "id", "data_type": "INTEGER", "is_pk": true, "is_nullable": false, "is_auto_increment": true, "default_value": null },
                { "name": "display`name", "data_type": "VARCHAR(255)", "is_pk": false, "is_nullable": true, "is_auto_increment": false, "default_value": null }
            ]
        }))
        .unwrap();
        let command = decode_command(&sql).unwrap().unwrap();

        assert!(sql.contains("`odd``table`"));
        assert!(sql.contains("`display``name` TEXT"));
        assert_eq!(command["primary_key"], "id");
        assert_eq!(command["attributes"][0]["type"], "Int");
        assert_eq!(command["attributes"][1]["type"], "String");
    }

    #[test]
    fn generated_ddl_rejects_an_edited_visible_preview() {
        let sql = create_table_sql(&json!({
            "params": { "database": "data" },
            "table_name": "person",
            "columns": [
                { "name": "id", "data_type": "INTEGER", "is_pk": true, "is_nullable": false, "is_auto_increment": true, "default_value": null }
            ]
        }))
        .unwrap();
        let edited = sql.replacen("`person`", "`other`", 1);

        assert!(decode_command(&edited)
            .unwrap()
            .unwrap_err()
            .message
            .contains("was edited"));
    }

    #[test]
    fn generated_ddl_binds_visible_preview_to_executable_fields() {
        let command = json!({
            "kind": "create_attribute",
            "database": "production",
            "table": "shadow",
            "attribute": "secret"
        });
        let disguised = encode_command(
            &command,
            "ALTER TABLE `data`.`notes` ADD COLUMN `title` ANY",
        );

        assert!(decode_command(&disguised)
            .unwrap()
            .unwrap_err()
            .message
            .contains("was edited"));
    }

    #[test]
    fn generated_ddl_preserves_whitespace_inside_quoted_identifiers() {
        let sql = create_table_sql(&json!({
            "params": { "database": "data" },
            "table_name": "person",
            "columns": [
                { "name": "first name", "data_type": "TEXT", "is_pk": true, "is_nullable": false, "is_auto_increment": false, "default_value": null }
            ]
        }))
        .unwrap();
        let edited = sql.replacen("`first name`", "`first  name`", 1);

        assert!(decode_command(&edited).unwrap().is_err());
    }

    #[test]
    fn generated_ddl_rejects_identifiers_the_host_would_rewrite() {
        let result = create_table_sql(&json!({
            "params": { "database": "data" },
            "table_name": "person",
            "columns": [
                { "name": "owner’s note", "data_type": "TEXT", "is_pk": true, "is_nullable": false, "is_auto_increment": false, "default_value": null }
            ]
        }));

        assert!(result.unwrap_err().message.contains("typographic quote"));
    }

    #[test]
    fn generated_attributes_are_rebuilt_from_allowed_fields() {
        let attributes = vec![json!({
            "name": "id",
            "type": "Int",
            "nullable": false,
            "indexed": false,
            "is_primary_key": true,
            "auto_increment": true,
            "hidden_option": "ignored"
        })];

        let sanitized = sanitize_generated_attributes(&attributes, "id").unwrap();

        assert_eq!(sanitized[0]["indexed"], true);
        assert_eq!(sanitized[0]["nullable"], true);
        assert!(sanitized[0].get("auto_increment").is_none());
        assert!(sanitized[0].get("hidden_option").is_none());
    }

    #[test]
    fn auto_increment_uses_only_harper_generated_key_types() {
        for data_type in ["FLOAT", "BIGINT"] {
            let result = create_table_sql(&json!({
                "params": { "database": "data" },
                "table_name": "person",
                "columns": [
                    { "name": "id", "data_type": data_type, "is_pk": true, "is_nullable": false, "is_auto_increment": true, "default_value": null }
                ]
            }));
            assert!(result
                .unwrap_err()
                .message
                .contains("INTEGER, LONG, or ANY"));
        }

        let any_key = create_table_sql(&json!({
            "params": { "database": "data" },
            "table_name": "person",
            "columns": [
                { "name": "id", "data_type": "ANY", "is_pk": true, "is_nullable": false, "is_auto_increment": true, "default_value": null }
            ]
        }))
        .unwrap();
        assert!(any_key.contains("AUTO_INCREMENT"));
    }

    #[test]
    fn parses_host_generated_drop_statements() {
        assert_eq!(
            parse_drop_table("DROP TABLE `data`.`odd``table`").unwrap(),
            (Some("data".to_string()), "odd`table".to_string(), false)
        );
        assert_eq!(
            parse_drop_column("ALTER TABLE `data`.`person` DROP COLUMN `age`").unwrap(),
            (
                Some("data".to_string()),
                "person".to_string(),
                "age".to_string()
            )
        );
        assert_eq!(
            parse_drop_table("DROP TABLE `data`.`user;data`").unwrap(),
            (Some("data".to_string()), "user;data".to_string(), false)
        );
        assert_eq!(
            parse_drop_table("DROP TABLE résumé").unwrap(),
            (None, "résumé".to_string(), false)
        );
        assert_eq!(
            parse_drop_table("DROP TABLE IF EXISTS `data`.`person`").unwrap(),
            (Some("data".to_string()), "person".to_string(), true)
        );
        assert_eq!(
            parse_drop_table("DROP TABLE`data`.`person`; -- old table").unwrap(),
            (Some("data".to_string()), "person".to_string(), false)
        );
        assert_eq!(
            parse_drop_table("DROP TABLE `data`.`person` -- old; table").unwrap(),
            (Some("data".to_string()), "person".to_string(), false)
        );
        assert_eq!(
            parse_drop_table("DROP TABLE data.person--backup").unwrap(),
            (
                Some("data".to_string()),
                "person--backup".to_string(),
                false
            )
        );
        assert_eq!(
            parse_drop_table("DROP /* routing */ TABLE `data`.`person`").unwrap(),
            (Some("data".to_string()), "person".to_string(), false)
        );
        assert_eq!(
            parse_drop_column("ALTER/**/TABLE `data`.`person` DROP /* cleanup */ COLUMN `age`")
                .unwrap(),
            (
                Some("data".to_string()),
                "person".to_string(),
                "age".to_string()
            )
        );
    }

    #[test]
    fn rejects_typed_added_attributes_instead_of_discarding_the_type() {
        let result = add_column_sql(&json!({
            "params": { "database": "data" },
            "table": "person",
            "column": {
                "name": "age",
                "data_type": "INTEGER",
                "is_nullable": true,
                "is_pk": false,
                "is_auto_increment": false,
                "default_value": null
            }
        }));
        assert!(result.unwrap_err().message.contains("choose ANY"));
    }

    #[test]
    fn rejects_ambiguous_unqualified_destructive_ddl() {
        let result = execute_ddl(
            &json!({
                "params": {
                    "host": "http://127.0.0.1:1",
                    "database": ["data", "staging"]
                }
            }),
            "DROP TABLE `person`",
        )
        .unwrap();
        assert!(result.unwrap_err().message.contains("must qualify"));
    }

    #[test]
    fn leading_comments_still_dispatch_destructive_ddl() {
        let result = execute_ddl(
            &json!({
                "params": {
                    "host": "http://127.0.0.1:1",
                    "database": ["data", "staging"]
                }
            }),
            "/* maintenance */ DROP TABLE `person`",
        )
        .expect("DROP TABLE should be recognized after a leading comment");

        assert!(result.unwrap_err().message.contains("must qualify"));
    }

    #[test]
    fn ddl_dispatch_ignores_keywords_inside_comments() {
        let params = json!({ "params": { "host": "http://127.0.0.1:1" } });

        assert!(execute_ddl(
            &params,
            "/* DROP TABLE data.person */ SELECT * FROM data.person"
        )
        .is_none());
        assert!(execute_ddl(&params, "-- DROP TABLE data.person\nSELECT 1").is_none());
    }

    #[test]
    fn ddl_dispatch_rejects_unterminated_leading_comments() {
        let result = execute_ddl(
            &json!({ "params": { "host": "http://127.0.0.1:1" } }),
            "/* maintenance DROP TABLE data.person",
        )
        .expect("unterminated leading comments should return an error");

        assert!(result.unwrap_err().message.contains("unterminated comment"));
    }

    #[test]
    fn comments_cannot_be_prepended_to_generated_commands() {
        let generated = create_table_sql(&json!({
            "params": { "database": "data" },
            "table_name": "person",
            "columns": [
                { "name": "id", "data_type": "INTEGER", "is_pk": true, "is_nullable": false, "is_auto_increment": false, "default_value": null }
            ]
        }))
        .unwrap();
        let query = format!("/* user prefix */ {generated}");
        let result = execute_ddl(
            &json!({ "params": { "host": "http://127.0.0.1:1" } }),
            &query,
        )
        .expect("a prefixed generated command should be rejected");

        assert!(result.unwrap_err().message.contains("must not be preceded"));
    }
}
