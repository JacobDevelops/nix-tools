//! Bun lockfile conversion and dependency-cache support.
//!
//! The lockfile converter is derived from `nix-community/bun2nix`, copyright
//! 2025 Luke Bailey, under the MIT License. See the repository `NOTICE` file.

mod cache;
mod conversion;
mod error;
mod inspection;
mod lockfile;
mod resolution;

pub use cache::{cache_entry_name, create_cache_entry};
pub use conversion::{
    ConvertOptions, NixPrefetcher, Prefetcher, convert_lockfile, convert_lockfile_with_prefetcher,
};
pub use error::{Error, Result};
pub use inspection::{LockfileInspection, PlatformConstraints, inspect_lockfile};
pub use lockfile::Lockfile;
