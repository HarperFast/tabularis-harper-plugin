use std::fmt;

#[derive(Debug)]
pub struct PluginError {
    pub code: i64,
    pub message: String,
}

impl PluginError {
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: msg.into(),
        }
    }

    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: msg.into(),
        }
    }

    pub fn connection(msg: impl Into<String>) -> Self {
        Self {
            code: -32001,
            message: msg.into(),
        }
    }
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for PluginError {}

#[cfg(test)]
mod tests {
    use super::PluginError;

    #[test]
    fn constructors_use_expected_json_rpc_codes() {
        assert_eq!(PluginError::invalid_params("bad input").code, -32602);
        assert_eq!(PluginError::internal("failed").code, -32603);
        assert_eq!(PluginError::connection("offline").code, -32001);
    }
}
