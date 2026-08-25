//! Integration tests for Bun JSONC lockfile parsing.

use bun2nix::{Error, Lockfile};

fn lockfile(version: u64) -> String {
    format!(
        r#"{{
          // Bun lockfiles are JSONC.
          "lockfileVersion": {version},
          "workspaces": {{}},
          "packages": {{}},
        }}"#
    )
}

#[test]
fn accepts_every_current_text_lockfile_version() {
    for version in 0..=3 {
        assert_eq!(
            u64::from(Lockfile::parse(&lockfile(version)).unwrap().version()),
            version
        );
    }
}

#[test]
fn rejects_unknown_lockfile_versions() {
    assert!(matches!(
        Lockfile::parse(&lockfile(4)),
        Err(Error::UnsupportedLockfileVersion(4))
    ));
}

#[test]
fn rejects_empty_jsonc_input() {
    assert!(matches!(
        Lockfile::parse(" // empty"),
        Err(Error::EmptyLockfile)
    ));
}
