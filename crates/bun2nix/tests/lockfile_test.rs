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

#[test]
fn accepts_resolved_lockfile_policy_metadata_that_needs_no_cache_transform() {
    Lockfile::parse(
        r#"{
          "lockfileVersion": 3,
          "configVersion": 1,
          "workspaces": {},
          "trustedDependencies": ["native"],
          "overrides": { "dep": "2.0.0" },
          "catalog": { "dep": "2.0.0" },
          "catalogs": { "testing": { "tool": "1.0.0" } },
          "packages": {}
        }"#,
    )
    .unwrap();
}

#[test]
fn rejects_patches_until_cache_entries_can_reproduce_them() {
    let error = Lockfile::parse(
        r#"{
          "lockfileVersion": 3,
          "workspaces": {},
          "patchedDependencies": { "dep@1.0.0": "patches/dep.patch" },
          "packages": {}
        }"#,
    )
    .unwrap_err();

    assert!(
        matches!(error, Error::UnsupportedSemantics(feature) if feature == "patchedDependencies")
    );
}

#[test]
fn rejects_unknown_top_level_semantics_instead_of_ignoring_them() {
    assert!(matches!(
        Lockfile::parse(
            r#"{
              "lockfileVersion": 3,
              "workspaces": {},
              "futureResolutionPolicy": {},
              "packages": {}
            }"#,
        ),
        Err(Error::InvalidLockfile(_))
    ));
}
