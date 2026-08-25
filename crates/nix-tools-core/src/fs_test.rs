use std::fs;
use std::path::Path;
use std::sync::{Arc, mpsc};

#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};

use super::{FileSystem, GuardedPublication, StdFileSystem, atomic_write_guarded_with};
use crate::process::Cancellation;
use crate::temp_dir_test::TempDir;

#[test]
fn bounded_nofollow_read_accepts_a_regular_file() {
    let root = TempDir::new("regular");
    fs::create_dir(root.path().join("nested")).expect("nested directory");
    fs::write(root.path().join("nested/manifest.json"), b"manifest").expect("manifest");

    let contents = StdFileSystem
        .read_bounded_nofollow(root.path(), Path::new("nested/manifest.json"), 8)
        .expect("bounded read");

    assert_eq!(contents, b"manifest");
}

#[test]
fn bounded_nofollow_read_rejects_escaping_and_oversized_paths() {
    let root = TempDir::new("bounds");
    fs::write(root.path().join("manifest.json"), b"too large").expect("manifest");

    for relative in ["../manifest.json", "/manifest.json"] {
        let error = StdFileSystem
            .read_bounded_nofollow(root.path(), Path::new(relative), 64)
            .expect_err("escaping path");
        assert!(error.message.contains("must remain beneath"));
    }

    let error = StdFileSystem
        .read_bounded_nofollow(root.path(), Path::new("manifest.json"), 3)
        .expect_err("oversized file");
    assert!(error.message.contains("exceeds 3 bytes"));
}

#[cfg(unix)]
#[test]
fn bounded_nofollow_read_rejects_final_and_parent_symlinks() {
    let root = TempDir::new("symlinks");
    let outside = TempDir::new("outside");
    fs::write(outside.path().join("manifest.json"), b"manifest").expect("outside manifest");
    symlink(
        outside.path().join("manifest.json"),
        root.path().join("manifest.json"),
    )
    .expect("final symlink");
    symlink(outside.path(), root.path().join("linked")).expect("parent symlink");

    let final_error = StdFileSystem
        .read_bounded_nofollow(root.path(), Path::new("manifest.json"), 64)
        .expect_err("final symlink");
    assert!(final_error.message.contains("not a symlink"));

    let parent_error = StdFileSystem
        .read_bounded_nofollow(root.path(), Path::new("linked/manifest.json"), 64)
        .expect_err("parent symlink");
    assert!(parent_error.message.contains("real directory"));
}

#[cfg(unix)]
#[test]
fn atomic_write_replaces_contents_and_mode_without_leaving_staging_files() {
    let root = TempDir::new("atomic");
    let output = root.path().join("result.json");
    fs::write(&output, b"old").expect("old output");

    StdFileSystem
        .write_atomic(&output, b"new", 0o600)
        .expect("atomic write");

    assert_eq!(fs::read(&output).expect("new output"), b"new");
    assert_eq!(
        fs::metadata(&output)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(fs::read_dir(root.path()).expect("directory").all(|entry| {
        !entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .contains(".nix-tools-tmp-")
    }));
}

#[test]
fn cancellation_before_commit_aborts_without_replacing_the_destination() {
    let root = TempDir::new("cancelled-atomic");
    let output = root.path().join("result.json");
    fs::write(&output, b"old").expect("old output");
    let cancellation = Cancellation::default();
    cancellation.request(15);

    let publication = StdFileSystem
        .write_atomic_guarded(&output, b"new", 0o644, &cancellation)
        .expect("guarded write");

    assert_eq!(publication, GuardedPublication::Aborted);
    assert_eq!(fs::read(&output).expect("preserved output"), b"old");
}

#[test]
fn cancellation_after_commit_starts_waits_for_visible_success() {
    let root = TempDir::new("cancellation-commit-gate");
    let output = root.path().join("result.json");
    fs::write(&output, b"old").expect("old output");
    let cancellation = Arc::new(Cancellation::default());
    let requester = Arc::clone(&cancellation);
    let (commit_started, started) = mpsc::channel();
    let (request_entered, entered) = mpsc::channel();
    let request = std::thread::spawn(move || {
        started.recv().expect("commit start");
        requester.request_after_entering_gate(2, || {
            request_entered.send(()).expect("request entered gate");
        });
    });

    let publication = atomic_write_guarded_with(
        &output,
        b"new",
        0o644,
        |_| Ok(()),
        &cancellation,
        || {
            commit_started.send(()).expect("signal commit start");
            entered.recv().expect("request entered commit gate");
            assert_eq!(cancellation.signal(), Some(2));
        },
    )
    .expect("guarded publication");
    request.join().expect("request thread");

    assert!(matches!(publication, GuardedPublication::Published(_)));
    assert_eq!(fs::read(&output).expect("visible output"), b"new");
    assert_eq!(cancellation.signal(), Some(2));
}

#[cfg(unix)]
#[test]
fn atomic_commit_remains_bound_to_the_opened_parent() {
    let root = TempDir::new("parent-swap");
    let outside = TempDir::new("parent-swap-outside");
    let parent = root.path().join("nested");
    let moved_parent = root.path().join("opened-parent");
    let output = parent.join("result.json");
    fs::create_dir(&parent).expect("parent");
    fs::write(&output, b"old").expect("old output");

    let publication = atomic_write_guarded_with(
        &output,
        b"new",
        0o644,
        |_| Ok(()),
        &Cancellation::default(),
        || {
            fs::rename(&parent, &moved_parent).expect("move opened parent");
            symlink(outside.path(), &parent).expect("replace parent path");
        },
    )
    .expect("publication bound to descriptor");

    assert!(matches!(publication, GuardedPublication::Published(_)));
    assert!(!outside.path().join("result.json").exists());
    assert_eq!(
        fs::read(moved_parent.join("result.json")).expect("published output"),
        b"new"
    );
}
