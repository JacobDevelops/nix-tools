//! Canonical Nix system identifiers.

use std::fmt;
use std::str::FromStr;

use crate::outcome::{Error, Result};

/// Nix systems supported by Linux and macOS repository tooling.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum NixSystem {
    /// `x86_64-linux`.
    X86_64Linux,
    /// `aarch64-linux`.
    Aarch64Linux,
    /// `x86_64-darwin`.
    X86_64Darwin,
    /// `aarch64-darwin`.
    Aarch64Darwin,
}

impl NixSystem {
    /// Detects the host Nix system.
    ///
    /// # Errors
    ///
    /// Returns a preflight error when the host architecture/OS pair is unsupported.
    pub fn host() -> Result<Self> {
        Self::from_parts(std::env::consts::ARCH, std::env::consts::OS)
    }

    /// Converts a Rust architecture/OS pair to a canonical Nix system.
    ///
    /// # Errors
    ///
    /// Returns a preflight error for unsupported pairs.
    pub fn from_parts(architecture: &str, operating_system: &str) -> Result<Self> {
        match (architecture, operating_system) {
            ("x86_64", "linux") => Ok(Self::X86_64Linux),
            ("aarch64", "linux") => Ok(Self::Aarch64Linux),
            ("x86_64", "macos" | "darwin") => Ok(Self::X86_64Darwin),
            ("aarch64", "macos" | "darwin") => Ok(Self::Aarch64Darwin),
            _ => Err(Error::preflight(format!(
                "unsupported Nix system {architecture}-{operating_system}"
            ))),
        }
    }

    /// Returns the canonical Nix system string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X86_64Linux => "x86_64-linux",
            Self::Aarch64Linux => "aarch64-linux",
            Self::X86_64Darwin => "x86_64-darwin",
            Self::Aarch64Darwin => "aarch64-darwin",
        }
    }
}

impl fmt::Display for NixSystem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for NixSystem {
    type Err = Error;

    fn from_str(system: &str) -> Result<Self> {
        match system {
            "x86_64-linux" => Ok(Self::X86_64Linux),
            "aarch64-linux" => Ok(Self::Aarch64Linux),
            "x86_64-darwin" => Ok(Self::X86_64Darwin),
            "aarch64-darwin" => Ok(Self::Aarch64Darwin),
            _ => Err(Error::preflight(format!("unsupported Nix system {system}"))),
        }
    }
}

#[cfg(test)]
#[path = "system_test.rs"]
mod system_test;
