use serde_json::Value;
use std::fmt;

#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct ConnectionParams {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub ssl_mode: Option<String>,
}

impl ConnectionParams {
    pub fn from_value(value: &Value) -> Self {
        let obj = value.as_object();
        let get_str = |k: &str| {
            obj.and_then(|o| o.get(k))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let port = obj.and_then(|o| o.get("port")).and_then(|value| {
            value
                .as_u64()
                .and_then(|port| u16::try_from(port).ok())
                .or_else(|| value.as_str().and_then(|port| port.parse().ok()))
        });

        Self {
            host: get_str("host"),
            port,
            username: get_str("username"),
            password: get_str("password"),
            ssl_mode: get_str("ssl_mode"),
        }
    }
}

impl fmt::Debug for ConnectionParams {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectionParams")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("ssl_mode", &self.ssl_mode)
            .finish()
    }
}

/// Extract the nested `params` object every RPC method receives.
/// Tabularis wraps connection params in `params.params`.
pub fn inner_params(value: &Value) -> &Value {
    value.get("params").unwrap_or(&Value::Null)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::ConnectionParams;

    #[test]
    fn parses_numeric_and_string_ports() {
        assert_eq!(
            ConnectionParams::from_value(&json!({ "port": 9925 })).port,
            Some(9925)
        );
        assert_eq!(
            ConnectionParams::from_value(&json!({ "port": "9926" })).port,
            Some(9926)
        );
    }

    #[test]
    fn debug_output_redacts_password() {
        let params = ConnectionParams::from_value(&json!({
            "host": "localhost",
            "username": "alice",
            "password": "swordfish"
        }));
        let output = format!("{params:?}");

        assert!(output.contains("<redacted>"));
        assert!(!output.contains("swordfish"));
    }
}
