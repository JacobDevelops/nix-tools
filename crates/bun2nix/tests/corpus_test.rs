//! Differential corpus tests against the repository's pinned Bun 1.4.0.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

use bun2nix::{
    ConvertOptions, Error, Lockfile, Prefetcher, Result, cache_entry_name,
    convert_lockfile_with_prefetcher,
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

const LOCAL_LOCK: &str = include_str!("fixtures/corpus/local/bun.lock");
const REGISTRY_LOCK: &str = include_str!("fixtures/corpus/registry/bun.lock");

struct FixedPrefetcher;

impl Prefetcher for FixedPrefetcher {
    fn prefetch(&self, _: &str) -> Result<String> {
        Ok("sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned())
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "bun2nix-corpus-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/corpus")
        .join(name)
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn bun(project: &Path, cache: &Path, arguments: &[&str]) -> Output {
    Command::new("bun")
        .args(arguments)
        .current_dir(project)
        .env("BUN_INSTALL_CACHE_DIR", cache)
        .output()
        .unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "Bun failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn tree_signature(root: &Path) -> BTreeSet<String> {
    fn visit(root: &Path, path: &Path, entries: &mut BTreeSet<String>) {
        let mut children = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect::<Vec<_>>();
        children.sort_by_key(std::fs::DirEntry::file_name);
        for child in children {
            let child_path = child.path();
            let relative = child_path.strip_prefix(root).unwrap().to_string_lossy();
            let file_type = child.file_type().unwrap();
            if file_type.is_symlink() {
                entries.insert(format!("link:{relative}"));
            } else if file_type.is_dir() {
                entries.insert(format!("dir:{relative}"));
                visit(root, &child_path, entries);
            } else {
                entries.insert(format!("file:{relative}"));
            }
        }
    }

    let mut entries = BTreeSet::new();
    visit(root, root, &mut entries);
    entries
}

#[test]
fn parses_generated_source_forms_and_preserves_exact_consumer_closures() {
    let local = Lockfile::parse(LOCAL_LOCK).unwrap();
    assert_eq!(
        local.production_dependency_closures().unwrap(),
        [
            ("@corpus/local-app".to_owned(), Vec::new()),
            (
                "corpus-local".to_owned(),
                vec!["tar-pkg@./vendor/tar-pkg-1.0.0.tgz".to_owned()],
            ),
            ("workspace-lib".to_owned(), Vec::new()),
        ]
        .into_iter()
        .collect()
    );

    let registry = Lockfile::parse(REGISTRY_LOCK).unwrap();
    let production = registry.production_dependency_closures().unwrap();
    let checks = registry.check_dependency_closures().unwrap();
    let root = &production["corpus-registry"];
    let app = &production["@corpus/app"];

    assert!(root.contains(&"@colors/colors@1.6.0".to_owned()));
    assert!(root.contains(&"@rollup/rollup-linux-x64-gnu@4.46.2".to_owned()));
    assert!(root.contains(&"fsevents@2.3.3".to_owned()));
    assert!(
        root.contains(&"is-number-object@github:inspect-js/is-number-object#5181bb2".to_owned())
    );
    assert!(root.contains(&"kleur@https://registry.npmjs.org/kleur/-/kleur-4.1.5.tgz".to_owned()));
    assert!(root.contains(&"react@19.1.1".to_owned()));
    assert_eq!(app, &["is-number@7.0.0", "is-odd@3.0.1", "kleur@4.1.5"]);
    assert!(!app.contains(&"is-even@1.0.0".to_owned()));
    assert!(checks["@corpus/app"].contains(&"is-even@1.0.0".to_owned()));
}

#[test]
fn conversion_is_deterministic_across_the_generated_corpus() {
    for lock in [
        LOCAL_LOCK,
        REGISTRY_LOCK,
        include_str!("fixtures/corpus/private-registry/bun.lock"),
        include_str!("fixtures/corpus/compatibility.lock"),
    ] {
        let first =
            convert_lockfile_with_prefetcher(lock, &ConvertOptions::default(), &FixedPrefetcher)
                .unwrap();
        let second =
            convert_lockfile_with_prefetcher(lock, &ConvertOptions::default(), &FixedPrefetcher)
                .unwrap();
        assert_eq!(first, second);
    }
}

#[test]
fn cache_names_match_bun_1_4_0_install_output() {
    let cases = [
        ("@colors/colors@1.6.0", None, "@colors/colors@1.6.0@@@1"),
        (
            "@rollup/rollup-linux-x64-gnu@4.46.2",
            None,
            "@rollup/rollup-linux-x64-gnu@4.46.2@@@1",
        ),
        ("is-number@7.0.0", None, "is-number@7.0.0@@@1"),
        (
            "is-number-object@github:inspect-js/is-number-object#5181bb2",
            None,
            "@GH@inspect-js-is-number-object-5181bb2@@@1",
        ),
        (
            "kleur@https://registry.npmjs.org/kleur/-/kleur-4.1.5.tgz",
            None,
            "@T@2a9b1cca31458601@@@1",
        ),
        (
            "@private/example@1.0.0",
            Some("registry.example.test"),
            "@private/example@1.0.0@@registry.example.test@@@1",
        ),
    ];

    for (resolution, registry, bun_name) in cases {
        assert_eq!(cache_entry_name(resolution, registry).unwrap(), bun_name);
    }
}

#[test]
fn bun_recreates_the_local_lock_and_offline_workspace_tree_deterministically() {
    let version = Command::new("bun").arg("--version").output().unwrap();
    assert_success(&version);
    assert_eq!(String::from_utf8_lossy(&version.stdout).trim(), "1.4.0");

    let project = TempDir::new("offline-project");
    let cache = TempDir::new("offline-cache");
    copy_tree(&fixture("local"), &project.0);
    let expected_lock = fs::read(project.0.join("bun.lock")).unwrap();

    fs::remove_file(project.0.join("bun.lock")).unwrap();
    let regenerate = bun(
        &project.0,
        &cache.0,
        &[
            "install",
            "--lockfile-only",
            "--offline",
            "--ignore-scripts",
        ],
    );
    assert_success(&regenerate);
    assert_eq!(fs::read(project.0.join("bun.lock")).unwrap(), expected_lock);

    let install = bun(
        &project.0,
        &cache.0,
        &[
            "install",
            "--offline",
            "--frozen-lockfile",
            "--ignore-scripts",
        ],
    );
    assert_success(&install);
    let first = tree_signature(&project.0.join("node_modules"));
    for package in ["file-pkg", "self", "tar-pkg", "workspace-lib"] {
        assert!(
            first.contains(&format!("link:{package}")) || first.contains(&format!("dir:{package}"))
        );
    }

    fs::remove_dir_all(project.0.join("node_modules")).unwrap();
    let reinstall = bun(
        &project.0,
        &cache.0,
        &[
            "install",
            "--offline",
            "--frozen-lockfile",
            "--ignore-scripts",
        ],
    );
    assert_success(&reinstall);
    assert_eq!(tree_signature(&project.0.join("node_modules")), first);
}

#[test]
fn lifecycle_scripts_run_only_after_explicit_opt_in() {
    let project = TempDir::new("lifecycle-project");
    let cache = TempDir::new("lifecycle-cache");
    copy_tree(&fixture("local"), &project.0);

    let ignored = bun(
        &project.0,
        &cache.0,
        &[
            "install",
            "--offline",
            "--frozen-lockfile",
            "--ignore-scripts",
        ],
    );
    assert_success(&ignored);
    assert!(
        !project
            .0
            .join("node_modules/lifecycle-pkg/lifecycle-ran")
            .exists()
    );

    fs::remove_dir_all(project.0.join("node_modules")).unwrap();
    let opted_in = bun(
        &project.0,
        &cache.0,
        &["install", "--offline", "--frozen-lockfile"],
    );
    assert_success(&opted_in);
    assert!(
        project
            .0
            .join("node_modules/lifecycle-pkg/lifecycle-ran")
            .exists()
    );
}

#[test]
fn rejects_bun_generated_patches_with_an_actionable_error() {
    let error = Lockfile::parse(include_str!("fixtures/corpus/patch/bun.lock")).unwrap_err();
    assert!(matches!(
        error,
        Error::UnsupportedSemantics("patchedDependencies")
    ));
    assert!(error.to_string().contains("patchedDependencies"));
}

#[test]
fn rejects_global_link_resolutions_because_bun_drops_the_source_path() {
    let error = Lockfile::parse(include_str!("fixtures/corpus/link/bun.lock")).unwrap_err();
    assert!(matches!(
        error,
        Error::UnsupportedSemantics("link dependencies")
    ));
    assert!(error.to_string().contains("link dependencies"));
}
