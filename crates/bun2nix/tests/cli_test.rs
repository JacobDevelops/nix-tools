//! End-to-end tests for the `bun2nix` command-line operations.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

fn temp_dir() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "bun2nix-cli-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn write_lock(root: &Path) -> PathBuf {
    let lock = root.join("bun.lock");
    fs::write(
        &lock,
        r#"{
          "lockfileVersion": 3,
          "workspaces": { "apps/app": { "name": "app", "dependencies": { "dep": "1" } } },
          "packages": {
            "app": ["app@workspace:apps/app"],
            "dep": ["dep@1.0.0", "", {}, "sha512-dep"]
          }
        }"#,
    )
    .unwrap();
    lock
}

#[test]
fn default_operation_converts_to_the_requested_bun_nix() {
    let root = temp_dir();
    let lock = write_lock(&root);
    let output = root.join("generated.nix");

    let result = Command::new(env!("CARGO_BIN_EXE_bun2nix"))
        .args([
            "--lock-file",
            lock.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        fs::read_to_string(&output)
            .unwrap()
            .contains("lockfileVersion = 3;")
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn inspect_and_plan_alias_emit_the_same_json() {
    let root = temp_dir();
    let lock = write_lock(&root);

    let inspect = Command::new(env!("CARGO_BIN_EXE_bun2nix"))
        .args(["inspect", "--lock-file", lock.to_str().unwrap()])
        .output()
        .unwrap();
    let plan = Command::new(env!("CARGO_BIN_EXE_bun2nix"))
        .args(["plan", "--lock-file", lock.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(inspect.status.success());
    assert_eq!(inspect.stdout, plan.stdout);
    let value: serde_json::Value = serde_json::from_slice(&inspect.stdout).unwrap();
    assert_eq!(
        value["productionDependencyClosures"]["app"],
        serde_json::json!(["dep@1.0.0"])
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cache_entry_operation_creates_the_link() {
    let root = temp_dir();
    let package = root.join("package");
    let cache = root.join("cache");
    fs::create_dir(&package).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_bun2nix"))
        .args([
            "cache-entry",
            "--out",
            cache.to_str().unwrap(),
            "--name",
            "pkg@1.0.0",
            "--package",
            package.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::canonicalize(cache.join("pkg@1.0.0@@@1")).unwrap(),
        fs::canonicalize(package).unwrap()
    );
    fs::remove_dir_all(root).unwrap();
}
