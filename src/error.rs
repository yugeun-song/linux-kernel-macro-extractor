use std::path::PathBuf;

use crate::index::sources::BuildError;
use crate::tree::TreeError;
use crate::tree::arch::ArchError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error(transparent)]
    Arch(#[from] ArchError),
    #[error("{0}")]
    Config(String),
    #[error(transparent)]
    Build(#[from] BuildError),
    #[error("{summary}")]
    Mismatch {
        summary: String,
        details: Vec<String>,
    },
    #[error("{0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error("internal error: {0}")]
    Internal(String),
}

pub mod exit {
    pub const OK: i32 = 0;
    pub const NOT_FOUND: i32 = 1;
    pub const USAGE: i32 = 2;
    pub const MISMATCH: i32 = 3;
    pub const FAILURE: i32 = 4;
}

impl Error {
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Usage(_) | Error::Tree(_) | Error::Arch(_) | Error::Config(_) => exit::USAGE,
            Error::Mismatch { .. } => exit::MISMATCH,
            Error::Build(BuildError::Nested { .. } | BuildError::Untracked { .. }) => exit::USAGE,
            Error::Build(_) | Error::Io(..) | Error::Internal(_) => exit::FAILURE,
        }
    }

    pub fn details(&self) -> &[String] {
        match self {
            Error::Mismatch { details, .. } => details,
            _ => &[],
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub trait IoContext<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T> {
        self.map_err(|e| Error::Io(path.into(), e))
    }
}
