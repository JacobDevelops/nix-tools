//! Integration tests for the machine-readable Bun dependency plan.

use bun2nix::inspect_lockfile;

const LOCK: &str = r#"{
  "lockfileVersion": 3,
  "workspaces": {
    "apps/a": { "name": "a", "dependencies": { "shared": "1", "only-a": "1" } },
    "apps/b": { "name": "b", "dependencies": { "shared": "1" } }
  },
  "packages": {
    "a": ["a@workspace:apps/a"],
    "b": ["b@workspace:apps/b"],
    "only-a": ["only-a@1.0.0", "", { "os": ["linux", "darwin"], "cpu": "x64" }, "sha512-a"],
    "shared": ["shared@2.0.0", "", {}, "sha512-shared"]
  }
}"#;

#[test]
fn exposes_deterministic_closures_consumers_constraints_and_workspace_sources() {
    let plan = inspect_lockfile(LOCK).unwrap();
    let json = serde_json::to_string_pretty(&plan).unwrap();

    assert_eq!(
        json,
        r#"{
  "lockfileVersion": 3,
  "dependencyClosures": {
    "a": [
      "only-a@1.0.0",
      "shared@2.0.0"
    ],
    "b": [
      "shared@2.0.0"
    ]
  },
  "consumerSets": {
    "only-a@1.0.0": [
      "a"
    ],
    "shared@2.0.0": [
      "a",
      "b"
    ]
  },
  "platformConstraints": {
    "a@workspace:apps/a": {
      "os": null,
      "cpu": null
    },
    "b@workspace:apps/b": {
      "os": null,
      "cpu": null
    },
    "only-a@1.0.0": {
      "os": [
        "darwin",
        "linux"
      ],
      "cpu": [
        "x64"
      ]
    },
    "shared@2.0.0": {
      "os": null,
      "cpu": null
    }
  },
  "workspacePackages": [
    "a@workspace:apps/a",
    "b@workspace:apps/b"
  ]
}"#
    );
}
