//! Stable command outcomes and errors without repository-specific policy.

use std::fmt;

use serde::Serialize;
use serde_json::Value;

const OUTCOME_FIELDS: &[&str] = &[
    "summary",
    "affected_items",
    "warnings",
    "failure_log",
    "data",
    "exit_code",
];

/// Broad error category suitable for structured reporters.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Invalid command input.
    Usage,
    /// A required condition was not met before work began.
    Preflight,
    /// A requested object was not found.
    NotFound,
    /// A child process failed.
    Child,
    /// Work was cancelled by a signal.
    Cancelled,
    /// An external service or adapter failed.
    External,
    /// An operating-system I/O operation failed.
    Io,
    /// An invariant inside the caller or library failed.
    Internal,
}

/// Portable process exit status used by library errors and outcomes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExitCode(u8);

impl ExitCode {
    /// Successful process exit.
    pub const SUCCESS: Self = Self(0);
    /// Generic failure.
    pub const FAILURE: Self = Self(1);
    /// Invalid usage.
    pub const USAGE: Self = Self(2);
    /// Failed preflight.
    pub const PREFLIGHT: Self = Self(3);
    /// Requested object was not found.
    pub const NOT_FOUND: Self = Self(4);
    /// Internal software error, following `sysexits.h`.
    pub const INTERNAL: Self = Self(70);
    /// I/O error, following `sysexits.h`.
    pub const IO: Self = Self(74);

    /// Converts a non-zero child status, falling back to generic failure when it is not portable.
    #[must_use]
    pub fn from_child_code(code: i32) -> Self {
        u8::try_from(code)
            .ok()
            .filter(|code| *code != 0)
            .map_or(Self::FAILURE, Self)
    }

    /// Converts a signal number using the conventional `128 + signal` shell status.
    #[must_use]
    pub fn from_signal(signal: i32) -> Self {
        u8::try_from(128_i32.saturating_add(signal)).map_or(Self::FAILURE, Self)
    }

    /// Returns the numeric exit status.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Serialize for ExitCode {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_u8(self.0)
    }
}

/// An error carrying a stable category and exit status.
///
/// `Debug` intentionally hides the message because it may contain child output or credentials.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct Error {
    /// Structured error category.
    pub kind: ErrorKind,
    /// Human-readable detail for a user-facing reporter.
    pub message: String,
    /// Process status a CLI may return.
    #[serde(skip)]
    pub exit_code: ExitCode,
}

impl fmt::Debug for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Error")
            .field("kind", &self.kind)
            .field("message", &"[REDACTED]")
            .field("exit_code", &self.exit_code)
            .finish()
    }
}

impl Error {
    /// Creates an error with an explicit category and exit status.
    #[must_use]
    pub fn new(kind: ErrorKind, exit_code: ExitCode, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            exit_code,
        }
    }

    /// Creates an invalid-usage error.
    #[must_use]
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Usage, ExitCode::USAGE, message)
    }

    /// Creates a failed-preflight error.
    #[must_use]
    pub fn preflight(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Preflight, ExitCode::PREFLIGHT, message)
    }

    /// Creates a not-found error.
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, ExitCode::NOT_FOUND, message)
    }

    /// Creates an external-adapter error.
    #[must_use]
    pub fn external(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::External, ExitCode::INTERNAL, message)
    }

    /// Creates an I/O error.
    #[must_use]
    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Io, ExitCode::IO, message)
    }

    /// Creates an internal-invariant error.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, ExitCode::INTERNAL, message)
    }

    /// Creates an error preserving a child process status.
    #[must_use]
    pub fn child(code: ExitCode, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Child, code, message)
    }

    /// Creates a cancellation error preserving the conventional signal status.
    #[must_use]
    pub fn cancelled(signal: i32, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Cancelled, ExitCode::from_signal(signal), message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// Result type used throughout this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Reporter-neutral result of a successful or deliberately non-error operation.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct Outcome {
    /// One-line user-facing summary.
    pub summary: String,
    /// Stable identifiers affected by the operation.
    pub affected_items: Vec<String>,
    /// Non-fatal warnings collected during the operation.
    pub warnings: Vec<String>,
    /// Complete failed-tool output for a reporter that chooses to publish it.
    pub failure_log: Option<String>,
    data: Option<Value>,
    exit_code: ExitCode,
}

impl fmt::Debug for Outcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Outcome")
            .field("summary", &"[REDACTED]")
            .field("affected_items", &self.affected_items.len())
            .field("warnings", &self.warnings.len())
            .field(
                "failure_log",
                &self.failure_log.as_ref().map(|_| "[REDACTED]"),
            )
            .field("data", &self.data.as_ref().map(|_| "[REDACTED]"))
            .field("exit_code", &self.exit_code)
            .finish()
    }
}

impl Outcome {
    /// Creates a successful outcome.
    #[must_use]
    pub fn success(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            ..Self::default()
        }
    }

    /// Replaces the process status carried by this outcome.
    #[must_use]
    pub fn with_exit_code(mut self, exit_code: ExitCode) -> Self {
        self.exit_code = exit_code;
        self
    }

    /// Returns the process status carried by this outcome.
    #[must_use]
    pub const fn exit_code(&self) -> ExitCode {
        self.exit_code
    }

    /// Returns repository-specific structured result data.
    #[must_use]
    pub fn data(&self) -> Option<&Value> {
        self.data.as_ref()
    }

    /// Attaches repository-specific object data without allowing it to shadow common fields.
    ///
    /// # Errors
    ///
    /// Returns an error when `data` is not a JSON object or contains a reserved outcome field.
    pub fn with_data<T: Serialize>(mut self, data: &T) -> Result<Self> {
        let data = serde_json::to_value(data)
            .map_err(|error| Error::internal(format!("serialize outcome data: {error}")))?;
        let Value::Object(fields) = &data else {
            return Err(Error::internal("outcome data must be an object"));
        };
        if fields
            .keys()
            .any(|key| OUTCOME_FIELDS.contains(&key.as_str()))
        {
            return Err(Error::internal(
                "outcome data collides with common result fields",
            ));
        }
        self.data = Some(data);
        Ok(self)
    }
}

#[cfg(test)]
#[path = "outcome_test.rs"]
mod outcome_test;
