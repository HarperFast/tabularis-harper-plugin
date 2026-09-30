use std::io::{self, BufRead, Write};

use serde_json::Value;

mod client;
mod error;
mod handlers;
mod models;
mod rpc;

pub fn handle_line(line: &str) -> Value {
    rpc::handle_line(line)
}

pub fn run(reader: impl BufRead, mut writer: impl Write) -> io::Result<()> {
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
    use std::thread;

    use serde_json::{json, Value};

    use super::{handle_line, run};

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
    fn dispatch_executes_select_through_harper_http() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
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

    fn read_request(stream: &mut impl Read) {
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
    }
}
