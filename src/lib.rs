use std::io::{self, BufRead, BufWriter, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};

use serde_json::Value;

mod client;
mod error;
mod handlers;
mod models;
mod rpc;

pub fn handle_line(line: &str) -> Value {
    catch_unwind(AssertUnwindSafe(|| rpc::handle_line(line))).unwrap_or_else(|_| {
        let id = serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|request| request.get("id").cloned())
            .unwrap_or(Value::Null);
        rpc::error_response(id, -32603, "internal plugin error")
    })
}

pub fn run(reader: impl BufRead, writer: impl Write) -> io::Result<()> {
    let mut writer = BufWriter::with_capacity(64 * 1024, writer);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let response = handle_line(line.trim());
        serde_json::to_writer(&mut writer, &response)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{self, Receiver};
    use std::thread;
    use std::time::Duration;

    use serde_json::{json, Value};

    use super::{handle_line, run};

    #[test]
    fn manifest_preserves_harper_runtime_identity() {
        let manifest: Value = serde_json::from_str(include_str!("../.tabularium")).unwrap();

        assert_eq!(manifest["id"], "harper");
        assert_eq!(manifest["name"], "Harper");
        assert_eq!(manifest["engine"], "harper");
        assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(manifest["executable"], env!("CARGO_PKG_NAME"));
        assert_eq!(manifest["capabilities"]["identifier_quote"], "`");
        assert_eq!(manifest["connection_metadata"], true);
    }

    #[test]
    fn connection_metadata_opt_in_returns_empty_overrides() {
        let response = handle_line(
            r#"{"jsonrpc":"2.0","method":"get_connection_metadata","params":{},"id":28}"#,
        );

        assert_eq!(response["result"], json!({}));
    }

    #[test]
    fn stdio_loop_uses_production_dispatch() {
        let input = Cursor::new(b"{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"id\":4}\n");
        let mut output = Vec::new();
        run(input, &mut output).unwrap();

        let response: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            response,
            json!({ "jsonrpc": "2.0", "result": null, "id": 4 })
        );
    }

    #[test]
    fn dispatch_defaults_missing_host_to_localhost() {
        let (host, requests, server) =
            server_responses_on("localhost:0", vec![r#"{"username":"HDB_ADMIN"}"#]);
        let port = host.rsplit(':').next().unwrap().parse::<u16>().unwrap();
        let request = json!({
            "jsonrpc": "2.0",
            "method": "test_connection",
            "params": { "params": { "port": port } },
            "id": 29
        });

        let response = handle_line(&request.to_string());
        assert_eq!(response["result"]["success"], true, "{response}");
        server.join().unwrap();
        let requests = requests.recv().unwrap();

        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains(r#""operation":"user_info""#));
    }

    #[test]
    fn dispatch_executes_select_through_harper_http() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_request(&mut stream);
            let body = r#"[{"name":"Ada","id":1}]"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let request = json!({
            "jsonrpc": "2.0",
            "method": "execute_query",
            "params": {
                "params": { "host": format!("http://{address}") },
                "query": "SELECT * FROM data.person",
                "page": 1,
                "page_size": 100
            },
            "id": 5
        });

        let response = handle_line(&request.to_string());
        server.join().unwrap();

        assert_eq!(response["result"]["columns"], json!(["name", "id"]));
        assert_eq!(response["result"]["rows"], json!([["Ada", 1]]));
        assert_eq!(response["result"]["total_count"], 1);
        assert_eq!(response["id"], 5);
    }

    #[test]
    fn dispatch_quotes_the_count_alias_generated_by_tabularis() {
        let (host, requests, server) = server_responses(vec![r#"[{"count":42}]"#]);
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "execute_query",
                "params": {
                    "params": { "host": host },
                    "query": "SELECT COUNT(*) as count FROM `data`.`File`",
                    "page": 1,
                    "limit": 100
                },
                "id": 32
            })
            .to_string(),
        );
        server.join().unwrap();
        let requests = requests.recv().unwrap();
        let (_, body) = requests[0].split_once("\r\n\r\n").unwrap();
        let operation: Value = serde_json::from_str(body).unwrap();

        assert_eq!(
            operation["sql"],
            "SELECT COUNT(*) as `count` FROM `data`.`File`\nLIMIT 101 OFFSET 0"
        );
        assert_eq!(response["result"]["columns"], json!(["count"]));
        assert_eq!(response["result"]["rows"], json!([[42]]));
    }

    #[test]
    fn dispatch_normalizes_the_multiline_select_generated_by_tabularis() {
        let (host, requests, server) =
            server_responses(vec![r#"[{"file_id":"file-1","package_id":"package-1"}]"#]);
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "execute_query",
                "params": {
                    "params": { "host": host },
                    "query": "SELECT\n  t1.`id` AS `file_id`,\n  t2.`id` AS `package_id`\nFROM\n  `data`.`File` t1\nINNER JOIN `packages`.`Package` t2 ON t1.`id` = t2.`id`",
                    "page": 2,
                    "limit": 2
                },
                "id": 33
            })
            .to_string(),
        );
        server.join().unwrap();
        let requests = requests.recv().unwrap();
        let (_, body) = requests[0].split_once("\r\n\r\n").unwrap();
        let operation: Value = serde_json::from_str(body).unwrap();

        assert_eq!(
            operation["sql"],
            "SELECT   t1.`id` AS `file_id`,\n  t2.`id` AS `package_id`\nFROM\n  `data`.`File` t1\nINNER JOIN `packages`.`Package` t2 ON t1.`id` = t2.`id`\nLIMIT 3 OFFSET 2"
        );
        assert_eq!(
            response["result"]["columns"],
            json!(["file_id", "package_id"])
        );
        assert_eq!(response["result"]["rows"], json!([["file-1", "package-1"]]));
    }

    #[test]
    fn dispatch_explains_unjoined_visual_query_tables_before_http() {
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "execute_query",
                "params": {
                    "params": { "host": "http://127.0.0.1:1" },
                    "query": "SELECT\n  t1.`path`,\n  t2.`filename`\nFROM\n  `packages`.`registry` t1,\n  `data`.`File` t2",
                    "page": 1,
                    "limit": 500
                },
                "id": 34
            })
            .to_string(),
        );

        assert_eq!(response["error"]["code"], -32602, "{response}");
        assert_eq!(
            response["error"]["message"],
            "Harper cannot execute comma-separated tables in FROM. Join each table using JOIN ... ON matching columns; in the visual query builder, connect every table node."
        );
    }

    #[test]
    fn dispatch_caps_unbounded_queries_without_tabularis_pagination() {
        let (host, requests, server) = server_responses(vec![r#"[{"id":1}]"#]);
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "execute_query",
                "params": {
                    "params": { "host": host },
                    "query": "SELECT * FROM data.person",
                    "page": 1,
                    "limit": null
                },
                "id": 12
            })
            .to_string(),
        );
        server.join().unwrap();
        let requests = requests.recv().unwrap();

        assert_eq!(response["result"]["pagination"], Value::Null);
        assert_eq!(response["result"]["truncated"], false);
        assert!(requests[0].contains("SELECT * FROM data.person\\nLIMIT 10001"));
    }

    #[test]
    fn dispatch_executes_sql_mutations_through_the_writable_path() {
        let (host, requests, server) =
            server_responses(vec![r#"{"inserted_hashes":[1],"skipped_hashes":[]}"#]);
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "execute_query",
                "params": {
                    "params": { "host": host },
                    "query": "INSERT INTO data.person (id, name) VALUES (1, 'Ada')"
                },
                "id": 21
            })
            .to_string(),
        );
        server.join().unwrap();
        let requests = requests.recv().unwrap();

        assert_eq!(response["result"]["affected_rows"], 1, "{response}");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains(r#""operation":"sql""#));
        assert!(requests[0].contains("INSERT INTO data.person"));
    }

    #[test]
    fn dispatch_inserts_one_record_through_native_harper_operation() {
        let (host, requests, server) = server_responses(vec![
            r#"{"inserted_hashes":["new-id"],"skipped_hashes":[]}"#,
        ]);
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "insert_record",
                "params": {
                    "params": { "host": host, "database": "data" },
                    "table": "person",
                    "data": { "name": "Ada" }
                },
                "id": 6
            })
            .to_string(),
        );
        server.join().unwrap();
        let requests = requests.recv().unwrap();

        assert_eq!(response["result"], 1);
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains(
            r#"{"operation":"insert","database":"data","table":"person","records":[{"name":"Ada"}]}"#
        ));
    }

    #[test]
    fn dispatch_rejects_non_primary_row_identity_before_mutation() {
        let (host, requests, server) = server_responses(vec![
            r#"{"primary_key":"id","attributes":[{"attribute":"id","type":"Int"},{"attribute":"owner_id","type":"Int"}]}"#,
        ]);
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "delete_record",
                "params": {
                    "params": { "host": host, "database": "data" },
                    "table": "identity_validation_test",
                    "pk_map": { "owner_id": 5 }
                },
                "id": 7
            })
            .to_string(),
        );
        server.join().unwrap();
        let requests = requests.recv().unwrap();

        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("primary key 'id'"));
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains(r#"{"operation":"describe_table"#));
    }

    #[test]
    fn dispatch_rejects_primary_key_edits_before_mutation() {
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "update_record",
                "params": {
                    "params": { "host": "http://127.0.0.1:1", "database": "data" },
                    "table": "person",
                    "pk_map": { "id": 5 },
                    "col_name": "id",
                    "new_val": 6
                },
                "id": 13
            })
            .to_string(),
        );

        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cannot be edited"));
    }

    #[test]
    fn dispatch_rejects_non_scalar_primary_keys_before_any_http_request() {
        for key in [json!({}), json!([]), json!(true), Value::Null] {
            let response = handle_line(
                &json!({
                    "jsonrpc": "2.0",
                    "method": "delete_record",
                    "params": {
                        "params": { "host": "http://127.0.0.1:1", "database": "data" },
                        "table": "non_scalar_key_test",
                        "pk_map": { "id": key }
                    },
                    "id": 22
                })
                .to_string(),
            );

            assert_eq!(response["error"]["code"], -32602, "{response}");
            assert!(response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("non-empty string or number"));
        }
    }

    #[test]
    fn dispatch_reuses_a_recent_primary_key_for_update_bursts() {
        let _cache_ttl = crate::handlers::crud::set_primary_key_cache_ttl_for_test(Duration::MAX);
        let (host, requests, server) = server_responses(vec![
            r#"{"primary_key":"id","attributes":[{"attribute":"id","type":"Int"},{"attribute":"name","type":"String"}]}"#,
            r#"{"update_hashes":[5],"skipped_hashes":[]}"#,
            r#"{"update_hashes":[5],"skipped_hashes":[]}"#,
        ]);
        let request = |name: &str, id: u64| {
            json!({
                "jsonrpc": "2.0",
                "method": "update_record",
                "params": {
                    "params": { "host": host, "database": "data" },
                    "table": "person",
                    "pk_map": { "id": 5 },
                    "col_name": "name",
                    "new_val": name
                },
                "id": id
            })
        };

        let first = handle_line(&request("Ada", 15).to_string());
        let second = handle_line(&request("Grace", 16).to_string());
        assert_eq!(first["result"], 1, "{first}");
        assert_eq!(second["result"], 1, "{second}");
        server.join().unwrap();
        let requests = requests.recv().unwrap();

        assert_eq!(requests.len(), 3);
        assert!(requests[0].contains(r#""operation":"describe_table""#));
        assert!(requests[1].contains(r#""operation":"update""#));
        assert!(requests[2].contains(r#""operation":"update""#));
    }

    #[test]
    fn ambiguous_grid_edits_fail_before_any_http_request() {
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "update_record",
                "params": {
                    "params": { "host": "http://127.0.0.1:1", "database": ["data", "staging"] },
                    "schema": null,
                    "table": "grid_edit_database_test",
                    "pk_map": { "id": 5 },
                    "col_name": "name",
                    "new_val": "Ada"
                },
                "id": 23
            })
            .to_string(),
        );

        assert_eq!(response["error"]["code"], -32602, "{response}");
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("exactly one Harper database"));
    }

    #[test]
    fn grid_edits_honor_the_top_level_selected_database() {
        let (host, requests, server) = server_responses(vec![
            r#"{"primary_key":"id","attributes":[{"attribute":"id","type":"Int"},{"attribute":"name","type":"String"}]}"#,
            r#"{"update_hashes":[5],"skipped_hashes":[]}"#,
        ]);
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "update_record",
                "params": {
                    "params": { "host": host, "database": ["data", "staging"] },
                    "database": "staging",
                    "table": "grid_edit_database_test",
                    "pk_map": { "id": 5 },
                    "col_name": "name",
                    "new_val": "Ada"
                },
                "id": 23
            })
            .to_string(),
        );
        server.join().unwrap();
        let requests = requests.recv().unwrap();

        assert_eq!(response["result"], 1, "{response}");
        assert_eq!(requests.len(), 2);
        assert!(requests
            .iter()
            .all(|request| request.contains(r#""database":"staging""#)));
    }

    #[test]
    fn drop_table_if_exists_is_a_noop_when_the_table_is_missing() {
        let (host, requests, server) = server_responses(vec![r#"{"other":{}}"#]);
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "execute_query",
                "params": {
                    "params": { "host": host, "database": "data" },
                    "query": "DROP TABLE IF EXISTS `data`.`missing`"
                },
                "id": 24
            })
            .to_string(),
        );
        server.join().unwrap();
        let requests = requests.recv().unwrap();

        assert_eq!(response["result"]["affected_rows"], 0, "{response}");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains(r#""operation":"describe_database""#));
    }

    #[test]
    fn drop_table_if_exists_rejects_an_invalid_database_description() {
        let (host, _requests, server) = server_responses(vec!["null"]);
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "execute_query",
                "params": {
                    "params": { "host": host, "database": "data" },
                    "query": "DROP TABLE IF EXISTS `data`.`person`"
                },
                "id": 25
            })
            .to_string(),
        );
        server.join().unwrap();

        assert_eq!(response["error"]["code"], -32001, "{response}");
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid database description"));
    }

    #[test]
    fn drop_column_rejects_schema_defined_tables_that_would_retain_data() {
        let (host, _requests, server) = server_responses(vec![
            r#"{"schema_defined":true,"primary_key":"id","attributes":[{"attribute":"id","type":"Int"},{"attribute":"secret","type":"String"}]}"#,
        ]);
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "execute_query",
                "params": {
                    "params": { "host": host, "database": "data" },
                    "query": "ALTER TABLE `data`.`person` DROP COLUMN `secret`"
                },
                "id": 26
            })
            .to_string(),
        );
        server.join().unwrap();

        assert_eq!(response["error"]["code"], -32602, "{response}");
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("retain"));
    }

    #[test]
    fn drop_column_executes_for_dynamic_tables() {
        let (host, requests, server) = server_responses(vec![
            r#"{"schema_defined":false,"primary_key":"id","attributes":[{"attribute":"id"},{"attribute":"nickname"}]}"#,
            r#"{"message":"attribute dropped"}"#,
        ]);
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "execute_query",
                "params": {
                    "params": { "host": host, "database": "data" },
                    "query": "ALTER TABLE `data`.`person` DROP COLUMN `nickname`"
                },
                "id": 27
            })
            .to_string(),
        );
        server.join().unwrap();
        let requests = requests.recv().unwrap();

        assert_eq!(response["result"]["affected_rows"], 0, "{response}");
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains(r#""operation":"drop_attribute""#));
    }

    #[test]
    fn drop_column_requires_explicit_schema_kind_metadata() {
        let (host, _requests, server) = server_responses(vec![
            r#"{"primary_key":"id","attributes":[{"attribute":"id"},{"attribute":"nickname"}]}"#,
        ]);
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "execute_query",
                "params": {
                    "params": { "host": host, "database": "data" },
                    "query": "ALTER TABLE `data`.`person` DROP COLUMN `nickname`"
                },
                "id": 29
            })
            .to_string(),
        );
        server.join().unwrap();

        assert_eq!(response["error"]["code"], -32001, "{response}");
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("schema is defined"));
    }

    #[test]
    fn dispatch_rejects_binary_file_markers_before_any_http_request() {
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "update_record",
                "params": {
                    "params": { "host": "http://127.0.0.1:1", "database": "data" },
                    "table": "person",
                    "pk_map": { "id": 5 },
                    "col_name": "photo",
                    "new_val": "BLOB_FILE_REF:/private/tmp/upload"
                },
                "id": 14
            })
            .to_string(),
        );

        assert_eq!(response["error"]["code"], -32602);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("binary upload markers"));
    }

    #[test]
    fn dispatch_reports_skipped_single_row_write_as_an_error() {
        let (host, _requests, server) = server_responses(vec![
            r#"{"inserted_hashes":[],"skipped_hashes":["existing"]}"#,
        ]);
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "insert_record",
                "params": {
                    "params": { "host": host, "database": "data" },
                    "table": "person",
                    "data": { "id": "existing" }
                },
                "id": 8
            })
            .to_string(),
        );
        server.join().unwrap();

        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("did not affect exactly one record"));
    }

    #[test]
    fn dispatch_schema_snapshot_uses_one_database_description() {
        let (host, requests, server) = server_responses(vec![
            r#"{"person":{"primary_key":"id","attributes":[{"attribute":"id","type":"Int"},{"attribute":"name","type":"String"}]}}"#,
        ]);
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "get_schema_snapshot",
                "params": {
                    "params": { "host": host, "database": "data" },
                    "schema": null
                },
                "id": 9
            })
            .to_string(),
        );
        server.join().unwrap();
        let requests = requests.recv().unwrap();

        assert_eq!(requests.len(), 1);
        assert_eq!(response["result"][0]["name"], "person");
        assert_eq!(response["result"][0]["columns"][0]["name"], "id");
        assert_eq!(response["result"][0]["foreign_keys"], json!([]));
    }

    #[test]
    fn schema_less_read_sequence_uses_the_primary_connection_database() {
        let (host, requests, server) = server_responses(vec![
            r#"{"person":{"primary_key":"id","attributes":[{"attribute":"id","type":"Int"}]}}"#,
            r#"{"primary_key":"id","attributes":[{"attribute":"id","type":"Int"}]}"#,
        ]);
        let connection = json!({ "host": host, "database": ["data", "staging"] });
        let tables = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "get_tables",
                "params": { "params": connection, "schema": null },
                "id": 19
            })
            .to_string(),
        );
        let columns = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "get_columns",
                "params": { "params": connection, "schema": null, "table": "person" },
                "id": 20
            })
            .to_string(),
        );
        server.join().unwrap();
        let requests = requests.recv().unwrap();

        assert_eq!(tables["result"][0]["name"], "person");
        assert_eq!(columns["result"][0]["name"], "id");
        assert!(requests
            .iter()
            .all(|request| request.contains(r#""database":"data""#)));
    }

    #[test]
    fn generated_create_table_executes_one_native_schema_operation() {
        let generated = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "get_create_table_sql",
                "params": {
                    "params": { "host": "unused", "database": "data" },
                    "table_name": "person",
                    "columns": [
                        { "name": "id", "data_type": "INTEGER", "is_pk": true, "is_nullable": false, "is_auto_increment": true, "default_value": null },
                        { "name": "name", "data_type": "TEXT", "is_pk": false, "is_nullable": true, "is_auto_increment": false, "default_value": null }
                    ]
                },
                "id": 10
            })
            .to_string(),
        );
        let statement = generated["result"][0].as_str().unwrap();
        let (host, requests, server) = server_responses(vec![r#"{"message":"table created"}"#]);
        let executed = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "execute_query",
                "params": {
                    "params": { "host": host, "database": "data" },
                    "query": statement
                },
                "id": 11
            })
            .to_string(),
        );
        server.join().unwrap();
        let requests = requests.recv().unwrap();

        assert_eq!(executed["result"]["affected_rows"], 0);
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains(r#""operation":"create_table""#));
        assert!(requests[0].contains(r#""primary_key":"id""#));
        assert!(requests[0].contains(r#""type":"Int""#));
        assert!(requests[0].contains(r#""type":"String""#));
    }

    #[test]
    fn generated_add_column_executes_one_native_schema_operation() {
        let generated = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "get_add_column_sql",
                "params": {
                    "params": { "host": "unused", "database": "data" },
                    "table": "person",
                    "column": {
                        "name": "nickname",
                        "data_type": "ANY",
                        "is_pk": false,
                        "is_nullable": true,
                        "is_auto_increment": false,
                        "default_value": null
                    }
                },
                "id": 17
            })
            .to_string(),
        );
        let statement = generated["result"][0].as_str().unwrap();
        let (host, requests, server) = server_responses(vec![r#"{"message":"attribute created"}"#]);
        let executed = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "execute_query",
                "params": {
                    "params": { "host": host, "database": "data" },
                    "query": statement
                },
                "id": 18
            })
            .to_string(),
        );
        server.join().unwrap();
        let requests = requests.recv().unwrap();

        assert_eq!(executed["result"]["affected_rows"], 0);
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains(
            r#"{"operation":"create_attribute","database":"data","table":"person","attribute":"nickname"}"#
        ));
    }

    #[test]
    fn host_drop_table_sql_executes_native_harper_drop() {
        let (host, requests, server) =
            server_responses(vec![r#"{"message":"successfully deleted table"}"#]);
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "execute_query",
                "params": {
                    "params": { "host": host, "database": ["data", "staging"] },
                    "query": "DROP TABLE `staging`.`person`"
                },
                "id": 14
            })
            .to_string(),
        );
        server.join().unwrap();
        let requests = requests.recv().unwrap();

        assert_eq!(response["result"]["affected_rows"], 0);
        assert!(requests[0]
            .contains(r#"{"operation":"drop_table","database":"staging","table":"person"}"#));
    }

    #[test]
    fn all_mode_caps_a_user_written_sql_limit_without_rewriting_it() {
        let cap = crate::handlers::query::MAX_UNBOUNDED_ROWS as usize;
        let rows = (0..=cap)
            .map(|id| format!(r#"{{"id":{id}}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let body: &'static str = Box::leak(format!("[{rows}]").into_boxed_str());
        let (host, requests, server) = server_responses(vec![body]);
        let response = handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "execute_query",
                "params": {
                    "params": { "host": host },
                    "query": "SELECT * FROM data.person LIMIT 50000",
                    "page": 1,
                    "limit": null
                },
                "id": 31
            })
            .to_string(),
        );
        server.join().unwrap();
        let requests = requests.recv().unwrap();

        assert!(requests[0].contains("SELECT * FROM data.person LIMIT 50000"));
        assert!(!requests[0].contains("LIMIT 10001"), "{}", requests[0]);
        assert_eq!(response["result"]["rows"].as_array().unwrap().len(), cap);
        assert_eq!(response["result"]["truncated"], true);
        assert_eq!(response["result"]["total_count"], cap);
    }

    fn read_request(stream: &mut impl Read) -> String {
        let mut request = Vec::new();
        let mut chunk = [0; 4096];
        let header_end = loop {
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0);
            request.extend_from_slice(&chunk[..read]);
            if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let content_length = String::from_utf8_lossy(&request[..header_end])
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        while request.len() < header_end + content_length {
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0);
            request.extend_from_slice(&chunk[..read]);
        }
        String::from_utf8(request).unwrap()
    }

    fn server_responses(
        responses: Vec<&'static str>,
    ) -> (String, Receiver<Vec<String>>, thread::JoinHandle<()>) {
        server_responses_on("127.0.0.1:0", responses)
    }

    fn server_responses_on(
        bind_address: &str,
        responses: Vec<&'static str>,
    ) -> (String, Receiver<Vec<String>>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(bind_address).unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for body in responses {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_request(&mut stream));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            sender.send(requests).unwrap();
        });
        (format!("http://{address}"), receiver, server)
    }
}
