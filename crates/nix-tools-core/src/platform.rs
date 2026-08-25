//! Host-platform checks for operations with explicit portability requirements.

use std::fmt;

use crate::outcome::{Error, Result};

/// Host platform families supported by repository tooling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    /// Linux.
    Linux,
    /// macOS.
    MacOs,
    /// A host outside the supported families.
    Unsupported(&'static str),
}

impl Platform {
    /// Detects the platform used to compile the current process.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Unsupported(std::env::consts::OS)
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Linux => formatter.write_str("linux"),
            Self::MacOs => formatter.write_str("macos"),
            Self::Unsupported(name) => formatter.write_str(name),
        }
    }
}

/// Portability constraint declared by an operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformRequirement {
    /// Supported on Linux and macOS.
    Portable,
    /// Requires Linux-specific behavior.
    LinuxOnly,
}

/// Checks whether a host satisfies an operation's portability constraint.
pub trait PlatformCheck: Send + Sync {
    /// Validates a platform requirement before an operation begins.
    ///
    /// # Errors
    ///
    /// Returns a preflight error when `platform` does not satisfy `requirement`.
    fn preflight(
        &self,
        platform: Platform,
        requirement: PlatformRequirement,
        operation: &str,
    ) -> Result<()>;
}

/// Default check supporting portable Linux/macOS work and Linux-only work.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultPlatformCheck;

impl PlatformCheck for DefaultPlatformCheck {
    fn preflight(
        &self,
        platform: Platform,
        requirement: PlatformRequirement,
        operation: &str,
    ) -> Result<()> {
        let supported = matches!(
            (platform, requirement),
            (
                Platform::Linux | Platform::MacOs,
                PlatformRequirement::Portable
            ) | (Platform::Linux, PlatformRequirement::LinuxOnly)
        );
        if supported {
            Ok(())
        } else {
            Err(Error::preflight(format!(
                "{operation} is not supported on {platform}"
            )))
        }
    }
}

#[cfg(test)]
#[path = "platform_test.rs"]
mod platform_test;
