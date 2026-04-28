//! Error types for the bridge.
//!
//! All fallible bridge operations return [`BridgeResult<T>`]. Errors are
//! modelled as a closed enum so callers can match exhaustively.

use std::path::PathBuf;
use thiserror::Error;

pub type BridgeResult<T> = Result<T, BridgeError>;

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("failed to spawn PHPStan via PHP at {php}: {source}")]
    Spawn {
        php: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("PHPStan stdout was not valid UTF-8: {0}")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),

    #[error("PHPStan emitted invalid JSON: {source}\n--- raw output ---\n{raw}")]
    InvalidJson {
        #[source]
        source: serde_json::Error,
        raw: String,
    },

    #[error("file URI {0} could not be converted to a filesystem path")]
    InvalidUri(String),

    #[error("PHPStan crashed (exit code {code:?}): {stderr}")]
    PhpStanCrashed { code: Option<i32>, stderr: String },
}

impl BridgeError {
    /// Render the error as a single-line, user-facing message suitable for an
    /// LSP diagnostic. The raw JSON dump from [`BridgeError::InvalidJson`] is
    /// truncated to keep editor tooltips readable.
    pub fn to_diagnostic_message(&self) -> String {
        match self {
            BridgeError::InvalidJson { source, raw } => {
                let snippet: String = raw.chars().take(120).collect();
                format!("PHPStan emitted invalid JSON: {source} (output starts with: {snippet:?})")
            }
            other => other.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_json_message_truncates_raw_output() {
        let raw = "x".repeat(500);
        let source = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let err = BridgeError::InvalidJson { source, raw };
        let msg = err.to_diagnostic_message();
        assert!(msg.contains("invalid JSON"));
        // The truncated snippet should be at most ~120 characters of x's plus
        // surrounding context.
        assert!(msg.len() < 300, "message was {} chars: {msg}", msg.len());
    }

    #[test]
    fn spawn_error_renders_path() {
        let err = BridgeError::Spawn {
            php: PathBuf::from("/missing/php"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        };
        let msg = err.to_string();
        assert!(msg.contains("/missing/php"));
        assert!(msg.contains("no such file"));
    }
}
