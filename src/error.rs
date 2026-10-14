use std::fmt;
use std::path::PathBuf;

#[derive(Debug)]
pub enum Error {
    Io {
        path: PathBuf,
        action: &'static str,
        source: std::io::Error,
    },
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    Toml {
        path: PathBuf,
        detail: String,
    },
    Command {
        program: String,
        detail: String,
    },
    Invalid(String),
    Missing(String),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, action: &'static str, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            action,
            source,
        }
    }

    pub fn command(program: impl Into<String>, detail: impl Into<String>) -> Self {
        Error::Command {
            program: program.into(),
            detail: detail.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io {
                path,
                action,
                source,
            } => write!(f, "{action} {}: {source}", path.display()),
            Error::Json { path, source } => write!(f, "{}: {source}", path.display()),
            Error::Toml { path, detail } => write!(f, "{}: {detail}", path.display()),
            Error::Command { program, detail } => write!(f, "{program}: {detail}"),
            Error::Invalid(detail) => write!(f, "{detail}"),
            Error::Missing(what) => write!(f, "{what} not found"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
