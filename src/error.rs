//! Error types for the shuriken build engine.

use std::fmt;
use std::io;

/// The result type used throughout this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// An error produced while loading a manifest or running a build.
#[derive(Debug)]
pub enum Error {
    /// A filesystem operation failed. The string names the path involved.
    Io(String, io::Error),
    /// A manifest (or dyndep/depfile) could not be parsed. Messages are
    /// pre-formatted the way ninja formats them (`file:line: message`).
    Manifest(String),
    /// A problem with the requested targets or the shape of the graph: an
    /// unknown target, a missing input, a dependency cycle. Reported before any
    /// command runs.
    Graph(String),
    /// A condition that makes continuing pointless, such as a manifest that
    /// requires a newer version than this engine implements.
    Fatal(String),
    /// The build ran but did not finish successfully.
    Build(String),
    /// The build was interrupted (e.g. by SIGINT).
    Interrupted,
}

impl Error {
    pub(crate) fn build<S: Into<String>>(msg: S) -> Error {
        Error::Build(msg.into())
    }

    pub(crate) fn graph<S: Into<String>>(msg: S) -> Error {
        Error::Graph(msg.into())
    }

    pub(crate) fn io<S: Into<String>>(path: S, e: io::Error) -> Error {
        Error::Io(path.into(), e)
    }

    /// Re-classify a build error as a graph error.
    pub(crate) fn into_graph(self) -> Error {
        match self {
            Error::Build(m) => Error::Graph(m),
            other => other,
        }
    }

    /// True if this error was caused by an interruption rather than a failure.
    pub fn is_interrupted(&self) -> bool {
        matches!(self, Error::Interrupted)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(path, e) => {
                if path.is_empty() {
                    write!(f, "{e}")
                } else {
                    write!(f, "{path}: {e}")
                }
            }
            Error::Manifest(m) | Error::Build(m) | Error::Graph(m) | Error::Fatal(m) => {
                write!(f, "{m}")
            }
            Error::Interrupted => write!(f, "interrupted by user"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(_, e) => Some(e),
            _ => None,
        }
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> io::Error {
        match e {
            Error::Io(_, e) => e,
            other => io::Error::other(other.to_string()),
        }
    }
}
