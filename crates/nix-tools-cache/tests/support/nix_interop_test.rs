use super::{TempDir, fs};

#[test]
fn temporary_directory_rejects_existing_symlink_without_removing_target() {
    let sentinel = TempDir::new();
    let marker = sentinel.0.join("sentinel");
    fs::write(&marker, b"preserve me").expect("write sentinel");
    let fixture = TempDir::new();
    let link = fixture.0.join("existing");
    std::os::unix::fs::symlink(&sentinel.0, &link).expect("create pre-existing symlink");

    let result = TempDir::create(&link);
    let rejected =
        matches!(&result, Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists);
    drop(result);
    drop(fixture);

    assert_eq!(
        fs::read(&marker).expect("sentinel survives cleanup"),
        b"preserve me"
    );
    assert!(
        rejected,
        "temporary directories must be created exclusively"
    );
}
