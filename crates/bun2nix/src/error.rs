use std::io;

use thiserror::Error;

/// Result type used throughout `bun2nix`.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced while parsing, converting, or caching Bun dependencies.
#[derive(Debug, Error)]
pub enum Error {
    /// The JSONC parser rejected the lockfile.
    #[error("failed to parse Bun lockfile as JSONC: {0}")]
    ParseJsonc(#[from] jsonc_parser::errors::ParseError),

    /// The parsed JSON did not match the Bun lockfile schema used by this tool.
    #[error("invalid Bun lockfile: {0}")]
    InvalidLockfile(#[from] serde_json::Error),

    /// The lockfile contained only whitespace or comments.
    #[error("Bun lockfile contains no JSON value")]
    EmptyLockfile,

    /// The lockfile version is newer than the supported textual formats.
    #[error("unsupported Bun lockfile version {0}; expected a version from 0 through 3")]
    UnsupportedLockfileVersion(u64),

    /// A dependency required by the graph was absent from `packages`.
    #[error("Bun lockfile is missing {context}/{dependency}")]
    MissingDependency {
        /// Package or workspace from which resolution started.
        context: String,
        /// Missing dependency name.
        dependency: String,
    },

    /// A package tuple did not contain a string resolution.
    #[error("Bun lockfile package {key} is missing its resolution")]
    MissingPackageResolution {
        /// Lockfile package key.
        key: String,
    },

    /// A workspace dependency referenced a workspace entry that was absent.
    #[error("Bun lockfile is missing workspace {0}")]
    MissingWorkspace(String),

    /// Two named workspaces used the same package name.
    #[error("Bun lockfile contains duplicate named workspace {0}")]
    DuplicateWorkspaceName(String),

    /// A package tuple used a source shape that cannot be represented safely.
    #[error("invalid Bun lockfile package {key}: {reason}")]
    InvalidPackage {
        /// Lockfile package key.
        key: String,
        /// Description of the invalid source declaration.
        reason: String,
    },

    /// Multiple package keys declared incompatible data for one resolution.
    #[error("Bun lockfile resolution {0} has conflicting package entries")]
    ConflictingResolution(String),

    /// `nix flake prefetch` could not fetch an external source.
    #[error("failed to prefetch {locator}: {stderr}")]
    PrefetchFailed {
        /// Source passed to Nix.
        locator: String,
        /// Diagnostic emitted by Nix.
        stderr: String,
    },

    /// `nix flake prefetch` returned an unexpected JSON response.
    #[error("invalid prefetch response for {locator}: {reason}")]
    InvalidPrefetchResponse {
        /// Source passed to Nix.
        locator: String,
        /// Response parsing failure.
        reason: String,
    },

    /// A computed cache name contained an absolute or parent path component.
    #[error("invalid Bun cache entry name {0}")]
    InvalidCacheEntryName(String),

    /// Cache entries must link to an extracted package directory.
    #[error("Bun cache package path is not a directory: {0}")]
    PackagePathNotDirectory(std::path::PathBuf),

    /// A filesystem operation failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}
