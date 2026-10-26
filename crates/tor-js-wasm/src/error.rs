//! Error types for JavaScript consumption

use serde::Serialize;
use wasm_bindgen::prelude::*;

/// `e`'s message followed by each source that adds to it, joined with ": ".
///
/// Arti's top-level message often stops at the category ("Protocol error while
/// launching a data stream"); the reason, such as the exit's END reason, is
/// further down the source chain.
pub fn with_sources(e: &dyn std::error::Error) -> String {
    let mut message = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        let part = cause.to_string();
        if !message.contains(&part) {
            message.push_str(": ");
            message.push_str(&part);
        }
        source = cause.source();
    }
    message
}

/// Error type exposed to JavaScript with structured error information
#[derive(Debug, Clone, Serialize)]
pub struct JsTorError {
    /// Stable error code for programmatic handling
    pub code: String,
    /// Error category
    pub kind: String,
    /// Human-readable error message
    pub message: String,
    /// Whether the operation can be retried
    pub retryable: bool,
}

impl JsTorError {
    /// Create a new error
    pub fn new(code: &str, kind: &str, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.to_string(),
            kind: kind.to_string(),
            message: message.into(),
            retryable,
        }
    }

    /// Create a "not initialized" error
    pub fn not_initialized() -> Self {
        Self::new(
            "NOT_INITIALIZED",
            "state",
            "TorClient has not been initialized or has been closed",
            false,
        )
    }

    /// Create an HTTP request error
    pub fn http_request(message: impl Into<String>) -> Self {
        Self::new("HTTP_REQUEST", "network", message, true)
    }

    /// Create a connection error
    pub fn connection(message: impl Into<String>) -> Self {
        Self::new("CONNECTION", "network", message, true)
    }

    /// Create a TLS error
    pub fn tls(message: impl Into<String>) -> Self {
        Self::new("TLS", "network", message, true)
    }

    /// Create a bootstrap error
    pub fn bootstrap(message: impl Into<String>) -> Self {
        Self::new("BOOTSTRAP", "circuit", message, true)
    }

    /// Create a configuration error
    pub fn config(message: impl Into<String>) -> Self {
        Self::new("CONFIGURATION", "configuration", message, false)
    }

    /// Create an internal error
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("INTERNAL", "internal", message, false)
    }

    /// Create an abort error (signal was set before or during the operation)
    pub fn aborted() -> Self {
        Self::new("ABORT", "abort", "The operation was aborted", false)
    }

    /// Convert to JsValue for returning to JavaScript.
    ///
    /// Returns a proper `Error` instance with `code`, `kind`, and `retryable`
    /// properties, so `instanceof Error` works and `.stack` is available.
    pub fn into_js_value(self) -> JsValue {
        let err = js_sys::Error::new(&self.message);
        js_sys::Reflect::set(&err, &"code".into(), &self.code.into()).ok();
        js_sys::Reflect::set(&err, &"kind".into(), &self.kind.into()).ok();
        js_sys::Reflect::set(&err, &"retryable".into(), &self.retryable.into()).ok();
        err.into()
    }
}

impl From<arti_client::Error> for JsTorError {
    fn from(e: arti_client::Error) -> Self {
        use arti_client::{ErrorKind, HasKind};

        let message = e.to_string();

        // Map arti-client errors to our error codes using structured ErrorKind
        // instead of fragile string matching on error messages.
        match e.kind() {
            ErrorKind::BootstrapRequired => Self::bootstrap(message),
            ErrorKind::InvalidConfig | ErrorKind::InvalidConfigTransition => Self::config(message),
            ErrorKind::TorAccessFailed
            | ErrorKind::RemoteNetworkTimeout
            | ErrorKind::DirectoryExpired => Self::connection(message),
            _ => Self::internal(message),
        }
    }
}

impl From<url::ParseError> for JsTorError {
    fn from(e: url::ParseError) -> Self {
        Self::new("INVALID_URL", "validation", e.to_string(), false)
    }
}

impl From<std::io::Error> for JsTorError {
    fn from(e: std::io::Error) -> Self {
        Self::new("IO_ERROR", "network", e.to_string(), true)
    }
}

impl std::fmt::Display for JsTorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}: {}", self.kind, self.code, self.message)
    }
}

impl std::error::Error for JsTorError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);

    impl std::fmt::Display for Layer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref().map(|l| l as _)
        }
    }

    #[test]
    fn with_sources_appends_causes_the_message_lacks() {
        let e = Layer(
            "tor: remote stream error: Protocol error while launching a data stream",
            Some(Box::new(Layer(
                "Protocol error while launching a data stream",
                Some(Box::new(Layer(
                    "Received an END cell with reason MISC",
                    None,
                ))),
            ))),
        );
        assert_eq!(
            with_sources(&e),
            "tor: remote stream error: Protocol error while launching a data stream: \
             Received an END cell with reason MISC"
        );
    }
}
