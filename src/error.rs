use std::fmt;

/// One error type for the whole companion. Messages are printed to the user and
/// sent to the platform as failure reasons, so they must never carry the device
/// token — construct them from paths, status codes, and process state only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorKind {
    Other,
    Transport,
}

#[derive(Debug)]
pub struct Error {
    message: String,
    kind: ErrorKind,
    /// The HTTP status behind a platform refusal, so a caller can tell a
    /// deliberate answer (409: settle the prior visit first) from a fault.
    status: Option<u16>,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Error {
            message: message.into(),
            kind: ErrorKind::Other,
            status: None,
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn http_status(&self) -> Option<u16> {
        self.status
    }

    pub fn transport(message: impl Into<String>) -> Self {
        Error {
            message: message.into(),
            kind: ErrorKind::Transport,
            status: None,
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn is_transport(&self) -> bool {
        self.kind == ErrorKind::Transport
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Error::new(value.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Error::new(value.to_string())
    }
}

impl From<String> for Error {
    fn from(value: String) -> Self {
        Error::new(value)
    }
}

impl From<&str> for Error {
    fn from(value: &str) -> Self {
        Error::new(value.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
