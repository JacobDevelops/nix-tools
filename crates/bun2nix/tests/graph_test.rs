//! Integration tests for per-workspace dependency graph traversal.

use std::collections::BTreeMap;

use bun2nix::{Error, Lockfile};

const LOCK: &str = r#"
{
  "lockfileVersion": 3,
  "packages": {
    "@scope/bar": ["@scope/bar@1.0.0", "", {}],
    "@scope/react": ["@scope/react@1.0.0", "", { "dependencies": { "child": "1", "react": "1" } }],
    "@scope/react/child": ["child@1.0.0", "", { "dependencies": { "bar": "1" } }],
    "a/shared": ["shared@2.0.0", "", { "dependencies": { "nested": "1" } }],
    "a/shared/nested": ["nested@1.0.0", "", {}],
    "arrayFallbackChild": ["array-fallback-child@1.0.0", "", {}],
    "bar": ["bar@1.0.0", "", {}],
    "devA": ["dev-a@1.0.0", "", {}],
    "formatter": ["formatter@1.0.0", "", {}],
    "leaf": ["leaf@1.0.0", "", {}],
    "local": ["local@file:apps/a/local", { "dependencies": { "localChild": "1" } }, null],
    "localChild": ["local-child@1.0.0", "", {}],
    "react": ["react@1.0.0", "", {}],
    "shared": ["shared@1.0.0", "", { "dependencies": { "leaf": "1" } }],
    "uniqueA": ["unique-a@1.0.0", "", { "dependencies": { "@scope/react": "1" } }],
    "uniqueB": ["unique-b@1.0.0", { "dependencies": { "arrayFallbackChild": "1" } }, []]
  },
  "workspaces": {
    "": { "devDependencies": { "formatter": "1" }, "name": "root" },
    "apps/a": {
      "dependencies": { "local": "file:local", "shared": "1", "uniqueA": "1" },
      "devDependencies": { "devA": "1" },
      "name": "a"
    },
    "apps/b": { "dependencies": { "shared": "1", "uniqueB": "1" }, "name": "b" }
  }
}
"#;

#[test]
fn computes_the_typescript_generators_workspace_closures() {
    let actual = Lockfile::parse(LOCK)
        .unwrap()
        .dependency_closures()
        .unwrap();
    let expected = BTreeMap::from([
        (
            "a".to_owned(),
            vec![
                "@scope/react@1.0.0",
                "bar@1.0.0",
                "child@1.0.0",
                "dev-a@1.0.0",
                "local-child@1.0.0",
                "nested@1.0.0",
                "react@1.0.0",
                "shared@2.0.0",
                "unique-a@1.0.0",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        ),
        (
            "b".to_owned(),
            vec![
                "array-fallback-child@1.0.0",
                "leaf@1.0.0",
                "shared@1.0.0",
                "unique-b@1.0.0",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        ),
        ("root".to_owned(), vec!["formatter@1.0.0".to_owned()]),
    ]);

    assert_eq!(actual, expected);
}

#[test]
fn excludes_development_dependencies_of_workspace_dependencies() {
    let lock = Lockfile::parse(
        r#"{
          "lockfileVersion": 1,
          "workspaces": {
            "": {},
            "apps/a": { "name": "a", "dependencies": { "b": "workspace:*" } },
            "apps/b": { "name": "b", "devDependencies": { "test-b": "1" } }
          },
          "packages": { "test-b": ["test-b@1.0.0", "", {}] }
        }"#,
    )
    .unwrap();

    assert!(lock.check_dependency_closures().unwrap()["a"].is_empty());
}

#[test]
fn includes_resolved_optional_peers_and_skips_missing_ones() {
    let lock = Lockfile::parse(
        r#"{
          "lockfileVersion": 1,
          "workspaces": { "apps/a": { "name": "a", "dependencies": { "parent": "1" } } },
          "packages": {
            "parent": ["parent@1.0.0", "", {
              "peerDependencies": { "required": "1", "optional-present": "1", "optional-missing": "1" },
              "optionalPeers": ["optional-present", "optional-missing"]
            }],
            "required": ["required@1.0.0", "", {}],
            "optional-present": ["optional-present@1.0.0", "", {}]
          }
        }"#,
    )
    .unwrap();

    assert_eq!(
        lock.dependency_closures().unwrap()["a"],
        ["optional-present@1.0.0", "parent@1.0.0", "required@1.0.0"]
    );
}

#[test]
fn reports_a_missing_required_dependency_with_its_context() {
    let lock = Lockfile::parse(
        r#"{
          "lockfileVersion": 1,
          "workspaces": { "apps/a": { "name": "a", "dependencies": { "parent": "1" } } },
          "packages": { "parent": ["parent@1.0.0", "", { "dependencies": { "missing": "1" } }] }
        }"#,
    )
    .unwrap();

    assert!(matches!(
        lock.dependency_closures(),
        Err(Error::MissingDependency { context, dependency })
          if context == "parent" && dependency == "missing"
    ));
}

#[test]
fn registry_resolution_wins_over_a_same_named_workspace() {
    let lock = Lockfile::parse(include_str!("fixtures/workspace-name-collision.lock")).unwrap();

    assert_eq!(
        lock.dependency_closures().unwrap(),
        BTreeMap::from([
            ("app".to_owned(), vec!["is-number@7.0.0".to_owned()]),
            ("collision-root".to_owned(), vec![]),
            ("is-number".to_owned(), vec!["kleur@4.1.5".to_owned()]),
        ])
    );
}

#[test]
fn separates_production_check_and_development_dependency_closures() {
    let lock = Lockfile::parse(
        r#"{
          "lockfileVersion": 3,
          "workspaces": {
            "": {
              "name": "root",
              "dependencies": { "root-runtime": "1" },
              "devDependencies": { "root-tool": "1" }
            },
            "apps/api": {
              "name": "api",
              "dependencies": { "runtime": "1" },
              "devDependencies": { "test-tool": "1" }
            }
          },
          "packages": {
            "api": ["api@workspace:apps/api"],
            "root-runtime": ["root-runtime@1.0.0", "", {}],
            "root-tool": ["root-tool@1.0.0", "", {}],
            "runtime": ["runtime@1.0.0", "", { "dependencies": { "transitive": "1" } }],
            "test-tool": ["test-tool@1.0.0", "", {}],
            "transitive": ["transitive@1.0.0", "", {}]
          }
        }"#,
    )
    .unwrap();

    assert_eq!(
        lock.production_dependency_closures().unwrap()["api"],
        ["runtime@1.0.0", "transitive@1.0.0"]
    );
    assert_eq!(
        lock.check_dependency_closures().unwrap()["api"],
        ["runtime@1.0.0", "test-tool@1.0.0", "transitive@1.0.0"]
    );
    assert_eq!(
        lock.development_dependency_closures().unwrap()["api"],
        [
            "root-runtime@1.0.0",
            "root-tool@1.0.0",
            "runtime@1.0.0",
            "test-tool@1.0.0",
            "transitive@1.0.0"
        ]
    );
}

#[test]
fn computes_closures_for_a_named_root_project() {
    let lock = Lockfile::parse(
        r#"{
          "lockfileVersion": 3,
          "workspaces": {
            "": {
              "name": "root-app",
              "dependencies": { "runtime": "1" },
              "devDependencies": { "test-tool": "1" }
            }
          },
          "packages": {
            "runtime": ["runtime@1.0.0", "", {}],
            "test-tool": ["test-tool@1.0.0", "", {}]
          }
        }"#,
    )
    .unwrap();

    assert_eq!(
        lock.production_dependency_closures().unwrap()["root-app"],
        ["runtime@1.0.0"]
    );
    assert_eq!(
        lock.check_dependency_closures().unwrap()["root-app"],
        ["runtime@1.0.0", "test-tool@1.0.0"]
    );
}
