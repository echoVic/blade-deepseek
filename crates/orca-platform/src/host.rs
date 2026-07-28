use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingSystem {
    MacOs,
    Linux,
    Windows,
    Other(String),
}

impl OperatingSystem {
    fn current() -> Self {
        match std::env::consts::OS {
            "macos" => Self::MacOs,
            "linux" => Self::Linux,
            "windows" => Self::Windows,
            other => Self::Other(other.to_string()),
        }
    }
}

impl fmt::Display for OperatingSystem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MacOs => formatter.write_str("macOS"),
            Self::Linux => formatter.write_str("Linux"),
            Self::Windows => formatter.write_str("Windows"),
            Self::Other(name) => formatter.write_str(name),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Architecture {
    X86_64,
    Aarch64,
    Other(String),
}

impl Architecture {
    fn current() -> Self {
        match std::env::consts::ARCH {
            "x86_64" => Self::X86_64,
            "aarch64" => Self::Aarch64,
            other => Self::Other(other.to_string()),
        }
    }
}

impl fmt::Display for Architecture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::X86_64 => formatter.write_str("x86_64"),
            Self::Aarch64 => formatter.write_str("aarch64"),
            Self::Other(name) => formatter.write_str(name),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct HostPlatform {
    pub os: OperatingSystem,
    pub architecture: Architecture,
}

impl HostPlatform {
    pub fn new(os: OperatingSystem, architecture: Architecture) -> Self {
        Self { os, architecture }
    }

    pub fn current() -> Self {
        Self::new(OperatingSystem::current(), Architecture::current())
    }

    pub fn is_supported(&self) -> bool {
        matches!(
            (&self.os, &self.architecture),
            (
                OperatingSystem::MacOs | OperatingSystem::Linux | OperatingSystem::Windows,
                Architecture::X86_64 | Architecture::Aarch64
            )
        )
    }
}

impl fmt::Display for HostPlatform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {}", self.os, self.architecture)
    }
}
