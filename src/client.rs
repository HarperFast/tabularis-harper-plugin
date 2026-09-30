use std::error::Error as StdError;
use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;

use reqwest::blocking::{Client as HttpClient, RequestBuilder, Response};
use reqwest::redirect::Policy;
use reqwest::{StatusCode, Url};
use serde_json::{json, Value};

use crate::error::PluginError;
use crate::models::ConnectionParams;

const DEFAULT_OPERATIONS_PORT: u16 = 9925;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(105);
const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;

static HTTP_CLIENT: OnceLock<Result<HttpClient, String>> = OnceLock::new();

pub struct Client {
    endpoint: Url,
    https_inferred: bool,
    username: Option<String>,
    password: Option<String>,
    http: HttpClient,
}

impl Client {
    pub fn connect(params: ConnectionParams) -> Result<Self, PluginError> {
        let https_inferred = inferred_https(&params);
        let endpoint = build_endpoint(&params)?;
        let username = params.username.filter(|username| !username.is_empty());
        let password = params.password.filter(|password| !password.is_empty());

        match (&username, &password) {
            (Some(_), None) => {
                return Err(PluginError::invalid_params(
                    "a password is required when a Harper username is provided",
                ));
            }
            (None, Some(_)) => {
                return Err(PluginError::invalid_params(
                    "a username is required when a Harper password is provided",
                ));
            }
            _ => {}
        }

        Ok(Self {
            endpoint,
            https_inferred,
            username,
            password,
            http: shared_http_client()?,
        })
    }

    pub fn health(&self) -> Result<(), PluginError> {
        let url = self.endpoint.join("health").map_err(|error| {
            PluginError::invalid_params(format!("invalid Harper health URL: {error}"))
        })?;
        let mut response = self
            .http
            .get(url)
            .send()
            .map_err(|error| self.transport_error("Harper health check failed", &error))?;
        let status = response.status();
        let body = read_body(&mut response).map_err(|error| {
            PluginError::connection(format!("failed to read Harper response: {error}"))
        })?;

        if status.is_success() {
            Ok(())
        } else {
            Err(http_error(status, &body))
        }
    }

    pub fn describe_all(&self) -> Result<Value, PluginError> {
        self.operation(json!({ "operation": "describe_all" }))
    }

    pub fn user_info(&self) -> Result<Value, PluginError> {
        self.operation(json!({ "operation": "user_info" }))
    }

    pub fn describe_database(&self, database: &str) -> Result<Value, PluginError> {
        self.operation(json!({
            "operation": "describe_database",
            "database": database,
        }))
    }

    pub fn describe_table(&self, database: &str, table: &str) -> Result<Value, PluginError> {
        self.operation(json!({
            "operation": "describe_table",
            "database": database,
            "table": table,
        }))
    }

    pub fn sql(&self, sql: &str) -> Result<Value, PluginError> {
        self.operation(json!({
            "operation": "sql",
            "sql": sql,
        }))
    }

    pub fn sql_mutation(&self, sql: &str) -> Result<Value, PluginError> {
        self.mutation_operation(
            "SQL mutation",
            json!({
                "operation": "sql",
                "sql": sql,
            }),
        )
    }

    pub fn insert(&self, database: &str, table: &str, record: Value) -> Result<Value, PluginError> {
        self.mutation_operation(
            "insert",
            json!({
                "operation": "insert",
                "database": database,
                "table": table,
                "records": [record],
            }),
        )
    }

    pub fn update(&self, database: &str, table: &str, record: Value) -> Result<Value, PluginError> {
        self.mutation_operation(
            "update",
            json!({
                "operation": "update",
                "database": database,
                "table": table,
                "records": [record],
            }),
        )
    }

    pub fn delete(&self, database: &str, table: &str, key: Value) -> Result<Value, PluginError> {
        self.mutation_operation(
            "delete",
            json!({
                "operation": "delete",
                "database": database,
                "table": table,
                "hash_values": [key],
            }),
        )
    }

    pub fn create_table(
        &self,
        database: &str,
        table: &str,
        primary_key: &str,
        attributes: Vec<Value>,
    ) -> Result<Value, PluginError> {
        self.mutation_operation(
            "create_table",
            json!({
                "operation": "create_table",
                "database": database,
                "table": table,
                "primary_key": primary_key,
                "attributes": attributes,
            }),
        )
    }

    pub fn create_attribute(
        &self,
        database: &str,
        table: &str,
        attribute: &str,
    ) -> Result<Value, PluginError> {
        self.mutation_operation(
            "create_attribute",
            json!({
                "operation": "create_attribute",
                "database": database,
                "table": table,
                "attribute": attribute,
            }),
        )
    }

    pub fn drop_table(&self, database: &str, table: &str) -> Result<Value, PluginError> {
        self.mutation_operation(
            "drop_table",
            json!({
                "operation": "drop_table",
                "database": database,
                "table": table,
            }),
        )
    }

    pub fn drop_attribute(
        &self,
        database: &str,
        table: &str,
        attribute: &str,
    ) -> Result<Value, PluginError> {
        self.mutation_operation(
            "drop_attribute",
            json!({
                "operation": "drop_attribute",
                "database": database,
                "table": table,
                "attribute": attribute,
            }),
        )
    }

    fn mutation_operation(&self, name: &str, operation: Value) -> Result<Value, PluginError> {
        self.send_operation(operation, Some(name))
    }

    fn operation(&self, operation: Value) -> Result<Value, PluginError> {
        self.send_operation(operation, None)
    }

    fn send_operation(
        &self,
        operation: Value,
        mutation_name: Option<&str>,
    ) -> Result<Value, PluginError> {
        let request = self.authorize(self.http.post(self.endpoint.clone()).json(&operation));
        let mut response = request.send().map_err(|error| {
            if let Some(name) = mutation_name.filter(|_| !error.is_connect()) {
                unknown_mutation_outcome(name, &error_detail(&error))
            } else {
                self.transport_error("Harper request failed", &error)
            }
        })?;
        let status = response.status();
        let body = read_body(&mut response).map_err(|error| {
            if status.is_success() {
                if let Some(name) = mutation_name {
                    return unknown_mutation_outcome(name, &error.to_string());
                }
            }
            PluginError::connection(format!("failed to read Harper response: {error}"))
        })?;

        if !status.is_success() {
            return Err(http_error(status, &body));
        }
        if body.is_empty() {
            if let Some(name) = mutation_name {
                return Err(unknown_mutation_outcome(
                    name,
                    "Harper returned an empty success response",
                ));
            }
            return Ok(Value::Null);
        }

        serde_json::from_slice(&body).map_err(|error| {
            if let Some(name) = mutation_name {
                unknown_mutation_outcome(name, &format!("Harper returned invalid JSON: {error}"))
            } else {
                PluginError::connection(format!("Harper returned invalid JSON: {error}"))
            }
        })
    }

    fn authorize(&self, request: RequestBuilder) -> RequestBuilder {
        match (&self.username, &self.password) {
            (Some(username), Some(password)) => request.basic_auth(username, Some(password)),
            _ => request,
        }
    }

    fn transport_error(&self, context: &str, error: &reqwest::Error) -> PluginError {
        let mut message = format!("{context}: {}", error_detail(error));
        if self.https_inferred && peer_spoke_plain_http(error) {
            message.push_str(
                "; the connection used HTTPS—if this Harper server intentionally uses plain HTTP, select SSL mode Disabled or enter an explicit http:// host, but only on a trusted network because credentials will be sent unencrypted",
            );
        }
        PluginError::connection(message)
    }
}

fn inferred_https(params: &ConnectionParams) -> bool {
    let bare_host = params
        .host
        .as_deref()
        .map(str::trim)
        .is_some_and(|host| !host.is_empty() && !host.contains("://"));
    let mode_is_unset = params
        .ssl_mode
        .as_deref()
        .map(str::trim)
        .is_none_or(str::is_empty);
    bare_host && mode_is_unset
}

fn peer_spoke_plain_http(error: &reqwest::Error) -> bool {
    error_chain_has_invalid_content_type(error)
}

fn error_chain_has_invalid_content_type(error: &(dyn StdError + 'static)) -> bool {
    if matches!(
        error.downcast_ref::<rustls::Error>(),
        Some(rustls::Error::InvalidMessage(
            rustls::InvalidMessage::InvalidContentType
        ))
    ) {
        return true;
    }
    if let Some(inner) = error
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::get_ref)
    {
        if error_chain_has_invalid_content_type(inner) {
            return true;
        }
    }
    error
        .source()
        .is_some_and(error_chain_has_invalid_content_type)
}

fn shared_http_client() -> Result<HttpClient, PluginError> {
    HTTP_CLIENT
        .get_or_init(|| {
            HttpClient::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .redirect(Policy::none())
                .user_agent(concat!(
                    "harper-tabularis-plugin/",
                    env!("CARGO_PKG_VERSION")
                ))
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .cloned()
        .map_err(|error| {
            PluginError::internal(format!("failed to initialize HTTP client: {error}"))
        })
}

fn build_endpoint(params: &ConnectionParams) -> Result<Url, PluginError> {
    reject_unsupported_tls_files(params)?;
    let host = params
        .host
        .as_deref()
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .ok_or_else(|| PluginError::invalid_params("Harper host is required"))?;
    let has_scheme = host.contains("://");
    let required_scheme = scheme_for(params.ssl_mode.as_deref())?;
    let endpoint_text = if has_scheme {
        host.to_string()
    } else {
        format!("{required_scheme}://{host}")
    };
    let mut endpoint = Url::parse(&endpoint_text)
        .map_err(|error| PluginError::invalid_params(format!("invalid Harper host: {error}")))?;

    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        return Err(PluginError::invalid_params(
            "Harper credentials must use the username and password fields, not the host URL",
        ));
    }
    if endpoint.query().is_some() || endpoint.fragment().is_some() {
        return Err(PluginError::invalid_params(
            "Harper host must not contain a query string or fragment",
        ));
    }
    if endpoint.path() != "/" && !endpoint.path().is_empty() {
        return Err(PluginError::invalid_params(
            "Harper Operations API host must not contain a path",
        ));
    }
    if endpoint.scheme() != "http" && endpoint.scheme() != "https" {
        return Err(PluginError::invalid_params(
            "Harper host must use http or https",
        ));
    }
    let explicit_secure_mode = params
        .ssl_mode
        .as_deref()
        .map(str::trim)
        .is_some_and(|mode| !mode.is_empty() && required_scheme == "https");
    if explicit_secure_mode && endpoint.scheme() != "https" {
        return Err(PluginError::invalid_params(
            "the selected TLS mode requires an https Harper host",
        ));
    }

    let port = params
        .port
        .or_else(|| (!has_scheme).then_some(DEFAULT_OPERATIONS_PORT));
    if let Some(port) = port {
        endpoint
            .set_port(Some(port))
            .map_err(|_| PluginError::invalid_params("Harper host does not support a port"))?;
    }
    endpoint.set_path("/");

    Ok(endpoint)
}

fn scheme_for(ssl_mode: Option<&str>) -> Result<&'static str, PluginError> {
    match ssl_mode.map(str::trim).filter(|mode| !mode.is_empty()) {
        None => Ok("https"),
        Some(mode) => match mode.to_ascii_lowercase().replace('_', "-").as_str() {
            "disable" | "disabled" => Ok("http"),
            "prefer" | "preferred" | "require" | "required" | "verify-ca" | "verify-full"
            | "verify-identity" => Ok("https"),
            _ => Err(PluginError::invalid_params(format!(
                "unsupported Harper TLS mode '{mode}'"
            ))),
        },
    }
}

fn reject_unsupported_tls_files(params: &ConnectionParams) -> Result<(), PluginError> {
    for (value, label) in [
        (&params.ssl_ca, "CA certificate"),
        (&params.ssl_cert, "client certificate"),
        (&params.ssl_key, "client key"),
    ] {
        if value
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Err(PluginError::invalid_params(format!(
                "Harper {label} files are not supported by this plugin yet"
            )));
        }
    }
    Ok(())
}

fn read_body(response: &mut Response) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    response
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut body)?;
    if body.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(std::io::Error::other(format!(
            "Harper response exceeded the {} MiB safety limit",
            MAX_RESPONSE_BYTES / 1024 / 1024
        )));
    }
    Ok(body)
}

fn unknown_mutation_outcome(name: &str, detail: &str) -> PluginError {
    PluginError::connection(format!(
        "Harper {name} outcome is unknown because the response was interrupted ({detail}); verify the database state before retrying"
    ))
}

fn error_detail(error: &reqwest::Error) -> String {
    let mut details = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        let detail = cause.to_string();
        if !details.iter().any(|existing| existing == &detail) {
            details.push(detail);
        }
        source = cause.source();
    }
    details.join(": ")
}

fn http_error(status: StatusCode, body: &[u8]) -> PluginError {
    let detail = response_detail(body);
    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    PluginError::connection(format!("Harper returned HTTP {status}{suffix}"))
}

fn response_detail(body: &[u8]) -> String {
    let detail = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            ["error", "message", "detail", "title"]
                .into_iter()
                .find_map(|field| value.get(field).and_then(Value::as_str).map(str::to_string))
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string());
    detail.chars().take(512).collect()
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{self, Receiver};
    use std::thread;

    use super::{build_endpoint, Client};
    use crate::models::ConnectionParams;

    #[test]
    fn operation_sends_basic_auth_and_json() {
        let (host, request) = server("200 OK", r#"{"data":{}}"#);
        let client = Client::connect(params(&host, Some("alice"), Some("secret"))).unwrap();
        let result = client.describe_all().unwrap();
        let request = request.recv().unwrap();

        assert_eq!(result["data"], serde_json::json!({}));
        assert!(request.starts_with("POST / HTTP/1.1"));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: basic ywxpy2u6c2vjcmv0"));
        assert!(request.contains(r#"{"operation":"describe_all"}"#));
    }

    #[test]
    fn health_uses_health_path() {
        let (host, request) = server("200 OK", "healthy");
        let client = Client::connect(params(&host, None, None)).unwrap();
        client.health().unwrap();

        assert!(request.recv().unwrap().starts_with("GET /health HTTP/1.1"));
    }

    #[test]
    fn errors_include_status_and_harper_message_without_credentials() {
        let (host, _request) = server("401 Unauthorized", r#"{"error":"access denied"}"#);
        let client = Client::connect(params(&host, Some("alice"), Some("secret"))).unwrap();
        let error = client.describe_all().unwrap_err();

        assert!(error.message.contains("401 Unauthorized"));
        assert!(error.message.contains("access denied"));
        assert!(!error.message.contains("secret"));
        assert!(!error.message.contains("YWxpY2U6c2VjcmV0"));
    }

    #[test]
    fn bare_hosts_use_ssl_mode_and_default_operations_port() {
        let mut params = params("example.com", None, None);
        params.ssl_mode = Some("require".to_string());
        let endpoint = build_endpoint(&params).unwrap();

        assert_eq!(endpoint.as_str(), "https://example.com:9925/");
    }

    #[test]
    fn bare_hosts_default_to_https_unless_tls_is_disabled() {
        let endpoint = build_endpoint(&params("example.com", None, None)).unwrap();
        assert_eq!(endpoint.as_str(), "https://example.com:9925/");

        let mut disabled = params("example.com", None, None);
        disabled.ssl_mode = Some("disabled".to_string());
        assert_eq!(
            build_endpoint(&disabled).unwrap().as_str(),
            "http://example.com:9925/"
        );

        let explicit_http = build_endpoint(&params("http://example.com", None, None)).unwrap();
        assert_eq!(explicit_http.as_str(), "http://example.com/");
    }

    #[test]
    fn tabularis_tls_modes_never_downgrade_to_http() {
        for mode in [
            "prefer",
            "preferred",
            "require",
            "required",
            "verify-ca",
            "verify_ca",
            "verify-full",
            "verify_identity",
        ] {
            let mut params = params("example.com", None, None);
            params.ssl_mode = Some(mode.to_string());
            assert_eq!(build_endpoint(&params).unwrap().scheme(), "https", "{mode}");
        }

        let mut params = params("http://example.com", None, None);
        params.ssl_mode = Some("required".to_string());
        assert!(build_endpoint(&params)
            .unwrap_err()
            .message
            .contains("requires an https"));
    }

    #[test]
    fn unsupported_tls_files_and_modes_fail_closed() {
        let mut unknown_mode = params("example.com", None, None);
        unknown_mode.ssl_mode = Some("mystery".to_string());
        assert!(build_endpoint(&unknown_mode)
            .unwrap_err()
            .message
            .contains("unsupported"));

        let mut custom_ca = params("example.com", None, None);
        custom_ca.ssl_ca = Some("/tmp/ca.pem".to_string());
        assert!(build_endpoint(&custom_ca)
            .unwrap_err()
            .message
            .contains("CA certificate"));
    }

    #[test]
    fn insert_uses_native_harper_operation_shape() {
        let (host, request) = server("200 OK", r#"{"inserted_hashes":[1],"skipped_hashes":[]}"#);
        let client = Client::connect(params(&host, None, None)).unwrap();
        client
            .insert("data", "person", serde_json::json!({ "name": "Ada" }))
            .unwrap();
        let request = request.recv().unwrap();

        assert!(request.contains(
            r#"{"operation":"insert","database":"data","table":"person","records":[{"name":"Ada"}]}"#
        ));
    }

    #[test]
    fn interrupted_success_response_reports_unknown_mutation_outcome() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{{\"inserted_hashes\":[1]"
            )
            .unwrap();
        });
        let client = Client::connect(params(&format!("http://{address}"), None, None)).unwrap();

        let error = client
            .insert("data", "person", serde_json::json!({ "name": "Ada" }))
            .unwrap_err();
        server.join().unwrap();

        assert!(error.message.contains("outcome is unknown"));
        assert!(error.message.contains("verify the database state"));
    }

    #[test]
    fn empty_success_response_reports_unknown_mutation_outcome() {
        let (host, _request) = server("200 OK", "");
        let client = Client::connect(params(&host, None, None)).unwrap();

        let error = client.drop_table("data", "person").unwrap_err();

        assert!(error.message.contains("outcome is unknown"));
        assert!(error.message.contains("empty success response"));
        assert!(error.message.contains("verify the database state"));
    }

    #[test]
    fn default_https_failure_explains_how_to_use_intentional_plain_http() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        });
        let mut connection = params("127.0.0.1", None, None);
        connection.port = Some(address.port());
        let client = Client::connect(connection).unwrap();

        let error = client.describe_all().unwrap_err();
        server.join().unwrap();

        assert!(error.message.contains("used HTTPS"), "{}", error.message);
        assert!(error.message.contains("SSL mode Disabled"));
        assert!(error.message.contains("http://"));
    }

    #[test]
    fn explicit_https_failure_never_suggests_disabling_tls() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        });
        let client = Client::connect(params(&format!("https://{address}"), None, None)).unwrap();

        let error = client.describe_all().unwrap_err();
        server.join().unwrap();

        assert!(!error.message.contains("SSL mode Disabled"));
    }

    #[test]
    fn silent_tls_peer_never_suggests_disabling_tls() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
        });
        let mut connection = params("127.0.0.1", None, None);
        connection.port = Some(address.port());
        let client = Client::connect(connection).unwrap();

        let error = client.describe_all().unwrap_err();
        server.join().unwrap();

        assert!(!error.message.contains("SSL mode Disabled"));
    }

    #[test]
    fn explicit_url_keeps_its_default_port() {
        let endpoint = build_endpoint(&params("https://example.com", None, None)).unwrap();
        assert_eq!(endpoint.as_str(), "https://example.com/");
    }

    #[test]
    fn host_urls_must_not_contain_credentials_or_paths() {
        let credential_error =
            build_endpoint(&params("http://alice:secret@example.com", None, None)).unwrap_err();
        let path_error =
            build_endpoint(&params("http://example.com/operations", None, None)).unwrap_err();

        assert!(credential_error.message.contains("credentials"));
        assert!(!credential_error.message.contains("secret"));
        assert!(path_error.message.contains("must not contain a path"));
    }

    fn params(host: &str, username: Option<&str>, password: Option<&str>) -> ConnectionParams {
        ConnectionParams {
            host: Some(host.to_string()),
            username: username.map(str::to_string),
            password: password.map(str::to_string),
            ..ConnectionParams::default()
        }
    }

    fn server(status: &'static str, body: &'static str) -> (String, Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            let header_end = loop {
                let read = stream.read(&mut chunk).unwrap();
                assert!(read > 0);
                request.extend_from_slice(&chunk[..read]);
                if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n")
                {
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
            sender.send(String::from_utf8(request).unwrap()).unwrap();

            write!(
				stream,
				"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
				body.len()
			)
			.unwrap();
        });

        (format!("http://{address}"), receiver)
    }
}
