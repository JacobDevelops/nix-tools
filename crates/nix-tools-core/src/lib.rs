#![forbid(unsafe_code)]

//! Reusable, policy-free foundations for repository tooling around Nix.

pub mod fs;
pub mod history;
pub mod outcome;
pub mod platform;
pub mod process;
pub mod redaction;
pub mod schedule;
pub mod system;
pub mod terminal;

#[cfg(test)]
#[path = "temp_dir_test.rs"]
pub(crate) mod temp_dir_test;
