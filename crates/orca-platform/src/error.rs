use std::fmt;
use std::io;
use std::path::PathBuf;

use crate::host::HostPlatform;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlatformError {
    UnsupportedHost {
        platform: HostPlatform,
    },
    ExecutableNotFound {
        executable: String,
    },
    InvalidShellOverride {
        value: String,
        reason: String,
    },
    InvalidPathIdentity {
        path: String,
        reason: String,
    },
    ReparsePointRejected {
        path: PathBuf,
    },
    LockContended {
        path: PathBuf,
    },
    Io {
        kind: io::ErrorKind,
        message: String,
    },
}

impl PlatformError {
    pub fn io(operation: &str, error: io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: format!("{operation}: {error}"),
        }
    }
}

impl fmt::Display for PlatformError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedHost { platform } => {
                write!(formatter, "unsupported host platform: {platform}")
            }
            Self::ExecutableNotFound { executable } => {
                write!(formatter, "required executable was not found: {executable}")
            }
            Self::InvalidShellOverride { value, reason } => {
                write!(formatter, "invalid ORCA_SHELL override {value:?}: {reason}")
            }
            Self::InvalidPathIdentity { path, reason } => {
                write!(
                    formatter,
                    "invalid Windows path identity {path:?}: {reason}"
                )
            }
            Self::ReparsePointRejected { path } => {
                write!(
                    formatter,
                    "refusing to follow reparse-point path: {}",
                    path.display()
                )
            }
            Self::LockContended { path } => {
                write!(
                    formatter,
                    "another process already owns the lock: {}",
                    path.display()
                )
            }
            Self::Io { message, .. } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for PlatformError {}
