//! Integration tests for Bun-compatible cache entry names and links.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use bun2nix::{Error, cache_entry_name, create_cache_entry};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "bun2nix-{label}-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn matches_buns_npm_cache_names() {
    let cases = [
        (
            "react@1.2.3-beta.1+build.123",
            None,
            "react@1.2.3-c0734e9369ab610d+F48F05ED5AABC3A0@@@1",
        ),
        (
            "tailwindcss@4.0.0-beta.9",
            None,
            "tailwindcss@4.0.0-73c5c46324e78b9b@@@1",
        ),
        ("react@1.2.3", None, "react@1.2.3@@@1"),
        (
            "@types/react-dom@19.0.4",
            None,
            "@types/react-dom@19.0.4@@@1",
        ),
        (
            "@scope/pkg@1.0.0-beta.1",
            Some("npm.pkg.github.com"),
            "@scope/pkg@1.0.0-c0734e9369ab610d@@npm.pkg.github.com@@@1",
        ),
    ];

    for (resolution, registry, expected) in cases {
        assert_eq!(cache_entry_name(resolution, registry).unwrap(), expected);
    }
}

#[test]
fn normalizes_raw_lockfile_tarball_and_source_control_resolutions() {
    assert_eq!(
        cache_entry_name("zod@https://registry.npmjs.org/zod/-/zod-3.21.4.tgz", None).unwrap(),
        "@T@3be02e19198e30ee@@@1"
    );
    assert_eq!(
        cache_entry_name("zod@github:colinhacks/zod#f9bbb50", None).unwrap(),
        "@GH@colinhacks-zod-f9bbb50@@@1"
    );
    assert_eq!(
        cache_entry_name(
            "semantic@git+https://gitlab.example/repo#ee100d81f12ae315a81c2a664979a6cc1bce99a2",
            None
        )
        .unwrap(),
        "@G@ee100d81f12ae315a81c2a664979a6cc1bce99a2"
    );
    assert_eq!(
        cache_entry_name("semantic@git+ssh://git@gitlab.example/repo#facefeed", None).unwrap(),
        "@G@facefeed"
    );
}

#[test]
fn creates_an_absolute_scoped_cache_symlink() {
    let root = temp_dir("link");
    let package = root.join("package");
    let cache = root.join("cache");
    fs::create_dir(&package).unwrap();
    fs::write(package.join("package.json"), "{}").unwrap();

    let entry = create_cache_entry(&cache, "@scope/pkg@1.0.0", &package, None).unwrap();

    assert_eq!(entry, cache.join("@scope/pkg@1.0.0@@@1"));
    assert_eq!(
        fs::canonicalize(&entry).unwrap(),
        fs::canonicalize(&package).unwrap()
    );
    assert!(fs::read_link(&entry).unwrap().is_absolute());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn rejects_cache_names_that_can_escape_the_output() {
    assert!(matches!(
        cache_entry_name("../escape@1.0.0", None),
        Err(Error::InvalidCacheEntryName(_))
    ));
}

#[test]
fn path_form_local_tarballs_use_buns_tarball_cache_key() {
    assert_eq!(
        cache_entry_name("tar-local@../../vendor/tar-local.tgz", None).unwrap(),
        "@T@8fc799915bc02928@@@1"
    );
}
