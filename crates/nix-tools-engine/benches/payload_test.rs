#[cfg(unix)]
#[test]
fn synthetic_fixtures_preserve_existing_files_and_clean_up_only_their_own() {
    use std::{env, fs, sync::atomic::Ordering};

    let directory = env::temp_dir().join(format!("nix-tools-payload-test-{}", std::process::id()));
    fs::create_dir(&directory).expect("create test directory");
    let target = directory.join("preserved.json");
    fs::write(&target, b"preserve me").expect("write target");
    let predictable = directory.join(format!(
        "nix-tools-synthetic-graph-{}-{}.json",
        std::process::id(),
        super::FIXTURE_SEQUENCE.load(Ordering::Relaxed)
    ));
    std::os::unix::fs::symlink(&target, &predictable).expect("create symlink");

    let first = super::create_synthetic_fixture(&directory).expect("create first fixture");
    let second = super::create_synthetic_fixture(&directory).expect("create second fixture");
    assert_ne!(first.path, predictable);
    assert_ne!(first.path, second.path);
    let generated = first.path.clone();
    drop(first);
    assert!(!generated.exists());
    assert!(second.path.exists());
    drop(second);
    drop(super::Fixture {
        path: target.clone(),
        synthetic: false,
    });
    assert_eq!(fs::read(&target).expect("read target"), b"preserve me");

    fs::remove_file(&predictable).expect("remove test symlink");
    fs::remove_file(&target).expect("remove test target");
    fs::remove_dir(&directory).expect("remove test directory");
}
