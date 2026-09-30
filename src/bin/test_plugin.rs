use std::env;
use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let connection = match connection_from_env() {
        Ok(connection) => connection,
        Err(error) => {
            eprintln!("{error}");
            return;
        }
    };

    println!("test_plugin — type a method, `query <SQL>`, raw JSON-RPC, `help`, or `exit`");

    let mut next_id = 1;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let command = line.trim();
        if command.is_empty() {
            continue;
        }
        if command == "exit" || command == "quit" {
            break;
        }
        if command == "help" {
            print_help();
            continue;
        }

        let request = match request_for_command(command, next_id, &connection) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("{error}");
                continue;
            }
        };
        next_id += 1;

        let response = harper::handle_line(&request.to_string());
        let pretty =
            serde_json::to_string_pretty(&response).unwrap_or_else(|_| response.to_string());
        writeln!(out, "{pretty}").ok();
        out.flush().ok();
    }
}

fn print_help() {
    println!("  test_connection          authenticated Harper connectivity check");
    println!("  ping                     GET /health liveness check");
    println!("  get_databases            list Harper databases");
    println!("  get_tables               list tables in HARPER_DATABASE");
    println!("  query <SQL>              execute one SELECT/INSERT/UPDATE/DELETE statement");
    println!("  {{...}}                   send a complete JSON-RPC request");
    println!("  connection env: HARPER_HOST, HARPER_PORT, HARPER_DATABASE,");
    println!("                  HARPER_USERNAME, HARPER_PASSWORD, HARPER_SSL_MODE");
}

fn connection_from_env() -> Result<Value, String> {
    let port = env::var("HARPER_PORT")
        .unwrap_or_else(|_| "9925".to_string())
        .parse::<u16>()
        .map_err(|_| "HARPER_PORT must be an integer from 1 through 65535".to_string())?;
    Ok(json!({
        "driver": "harper",
        "host": env::var("HARPER_HOST").unwrap_or_else(|_| "http://localhost".to_string()),
        "port": port,
        "database": env::var("HARPER_DATABASE").ok(),
        "username": env::var("HARPER_USERNAME").ok(),
        "password": env::var("HARPER_PASSWORD").ok(),
        "ssl_mode": env::var("HARPER_SSL_MODE").ok(),
    }))
}

fn request_for_command(command: &str, id: u64, connection: &Value) -> Result<Value, String> {
    if command.starts_with('{') {
        return serde_json::from_str(command)
            .map_err(|error| format!("invalid JSON-RPC request: {error}"));
    }

    let (method, query) = command
        .strip_prefix("query ")
        .map(|query| ("execute_query", query))
        .unwrap_or((command, ""));
    Ok(json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": {
            "params": connection,
            "schema": null,
            "query": query,
            "page": 1,
            "page_size": 100,
        },
        "id": id,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::request_for_command;

    #[test]
    fn query_shorthand_builds_an_execute_request() {
        let connection = json!({ "host": "localhost", "password": "secret" });
        let request = request_for_command("query SELECT 1", 8, &connection).unwrap();
        assert_eq!(request["method"], "execute_query");
        assert_eq!(request["params"]["query"], "SELECT 1");
        assert_eq!(request["params"]["params"], connection);
        assert_eq!(request["id"], 8);
    }

    #[test]
    fn raw_json_rpc_is_preserved() {
        let request = request_for_command(
            "{\"jsonrpc\":\"2.0\",\"method\":\"ping\",\"id\":9}",
            1,
            &json!({}),
        )
        .unwrap();
        assert_eq!(request["method"], "ping");
        assert_eq!(request["id"], 9);
    }
}
