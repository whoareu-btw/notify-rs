//! Minimal D-Bus client/server, std only.

pub mod conn;
pub mod message;
pub mod value;

pub use conn::Conn;
pub use message::{Message, MsgType};
pub use value::Value;

use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Protocol(String),
    Timeout,
    Closed,
    /// The other side answered with a D-Bus error.
    Remote { name: String, text: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Protocol(s) => write!(f, "D-Bus protocol error: {s}"),
            Error::Timeout => write!(f, "D-Bus call timed out"),
            Error::Closed => write!(f, "D-Bus connection closed"),
            Error::Remote { name, text } if text.is_empty() => write!(f, "{name}"),
            Error::Remote { name, text } => write!(f, "{name}: {text}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
