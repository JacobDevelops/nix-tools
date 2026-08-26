use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest as _, Sha256};

use crate::{HashingWriter, NarInfo, NarInfoInput, fingerprint, nix_base32, write_nar};

const TREE_HASH: &str = "sha256:0jl9ljnfy8nhb626rswzsi3j9n0gwqp88905jp9j7940b9ksdf5i";
const TREE_SIZE: u64 = 1248;
const FILE_HASH: &str = "sha256:04zwf782yjwnh3q6hz5izfd6jyip8kgw6g6yj43fiqhbyhdd0dqw";
const SYMLINK_HASH: &str = "sha256:14hjby44la6x2qlb4xsihw41zh7k6pv0m2244nknqccxswmpsf7h";
const SIGNATURE: &str = "cache-1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";
const OTHER_SIGNATURE: &str = "other-1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";

struct TempTree(PathBuf);

impl TempTree {
    fn new() -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "nix-tools-cache-nar-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&path).expect("create temporary tree");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn file(&self, relative: &str, contents: &[u8], mode: u32) {
        let path = self.0.join(relative);
        fs::write(&path, contents).expect("write file");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("set file mode");
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn archive(path: &Path) -> (Vec<u8>, String, u64) {
    let mut bytes = Vec::new();
    let mut hashing = HashingWriter::new(&mut bytes);
    write_nar(path, &mut hashing).expect("serialize archive");
    let (hash, size) = hashing.finish();
    (bytes, hash, size)
}

#[test]
fn canonical_tree_matches_nix_archive_bytes() {
    let tree = TempTree::new();
    fs::create_dir(tree.path().join("sub")).expect("create subdirectory");
    fs::create_dir(tree.path().join("empty")).expect("create empty directory");
    tree.file("file.txt", b"hello\n", 0o644);
    tree.file("zero", b"", 0o644);
    tree.file("sub/run.sh", b"#!/bin/sh\necho hi\n", 0o755);
    std::os::unix::fs::symlink("../file.txt", tree.path().join("sub/link"))
        .expect("create symlink");

    let (bytes, hash, size) = archive(tree.path());

    assert_eq!(hash, TREE_HASH);
    assert_eq!(size, TREE_SIZE);
    assert_eq!(size, bytes.len() as u64);
}

#[test]
fn regular_file_and_symlink_match_nix_hashes() {
    let tree = TempTree::new();
    tree.file("file.txt", b"hello\n", 0o644);
    std::os::unix::fs::symlink("../file.txt", tree.path().join("link")).expect("create symlink");

    assert_eq!(archive(&tree.path().join("file.txt")).1, FILE_HASH);
    assert_eq!(archive(&tree.path().join("link")).1, SYMLINK_HASH);
}

#[test]
fn only_the_owner_execute_bit_sets_the_nar_executable_marker() {
    let tree = TempTree::new();
    tree.file("group-executable", b"hello\n", 0o654);
    tree.file("owner-executable", b"hello\n", 0o744);

    assert_eq!(archive(&tree.path().join("group-executable")).1, FILE_HASH);
    assert_ne!(archive(&tree.path().join("owner-executable")).1, FILE_HASH);
}

#[test]
fn symlink_target_is_not_followed() {
    let tree = TempTree::new();
    std::os::unix::fs::symlink("missing/../../target", tree.path().join("link"))
        .expect("create dangling symlink");

    let (first, _, _) = archive(&tree.path().join("link"));
    fs::create_dir(tree.path().join("missing")).expect("create unrelated target prefix");
    let (second, _, _) = archive(&tree.path().join("link"));

    assert_eq!(first, second);
}

#[test]
fn creation_order_does_not_change_directory_bytes() {
    let forward = TempTree::new();
    forward.file("a", b"a", 0o644);
    forward.file("b", b"b", 0o644);
    let reverse = TempTree::new();
    reverse.file("b", b"b", 0o644);
    reverse.file("a", b"a", 0o644);

    assert_eq!(archive(forward.path()).0, archive(reverse.path()).0);
}

#[test]
fn unsupported_and_missing_paths_are_rejected() {
    let unsupported = write_nar(Path::new("/dev/null"), &mut Vec::new())
        .expect_err("character devices have no NAR representation");
    let tree = TempTree::new();
    let missing = write_nar(&tree.path().join("missing"), &mut Vec::new())
        .expect_err("missing path cannot be archived");

    assert!(unsupported.to_string().contains("not a regular file"));
    assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn nix_base32_and_fingerprint_match_nix() {
    assert_eq!(
        nix_base32(&Sha256::digest(b"")),
        "0mdqa9w1p6cmli6976v4wi0sw9r4p5prkj7lzfd1877wk11c9c73"
    );
    assert_eq!(
        fingerprint(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo",
            TREE_HASH,
            TREE_SIZE,
            &["/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-ref".to_owned()]
        ),
        format!(
            "1;/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo;{TREE_HASH};{TREE_SIZE};/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-ref"
        )
    );
}

#[test]
fn narinfo_has_exact_nix_text_format() {
    let info = NarInfo::new(NarInfoInput {
        store_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo".to_owned(),
        url: "nar/hash.nar".to_owned(),
        compression: "none".to_owned(),
        file_hash: TREE_HASH.to_owned(),
        file_size: TREE_SIZE,
        nar_hash: TREE_HASH.to_owned(),
        nar_size: TREE_SIZE,
        references: vec![
            "/nix/store/cccccccccccccccccccccccccccccccc-z".to_owned(),
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-a".to_owned(),
        ],
        deriver: Some("/nix/store/dddddddddddddddddddddddddddddddd-demo.drv".to_owned()),
        signatures: vec![SIGNATURE.to_owned()],
        content_address: None,
    })
    .expect("valid narinfo");

    assert_eq!(
        info.to_string(),
        format!(
            "StorePath: /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo\nURL: nar/hash.nar\nCompression: none\nFileHash: {TREE_HASH}\nFileSize: {TREE_SIZE}\nNarHash: {TREE_HASH}\nNarSize: {TREE_SIZE}\nReferences: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-a cccccccccccccccccccccccccccccccc-z\nDeriver: dddddddddddddddddddddddddddddddd-demo.drv\nSig: {SIGNATURE}\n"
        )
    );
}

#[test]
fn narinfo_parses_and_preserves_multiple_signatures() {
    let mut input = narinfo_input(
        "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo",
        "nar/hash.nar",
        SIGNATURE,
    );
    input.signatures.push(OTHER_SIGNATURE.to_owned());
    let rendered = NarInfo::new(input)
        .expect("multi-signature narinfo")
        .to_string();

    let parsed = NarInfo::parse(&rendered).expect("parse multiple signatures");

    assert_eq!(
        parsed.signatures(),
        &[SIGNATURE.to_owned(), OTHER_SIGNATURE.to_owned()]
    );
    assert_eq!(parsed.to_string(), rendered);
    assert_eq!(rendered.matches("Sig: ").count(), 2);
}

#[test]
fn narinfo_rejects_line_injection_and_non_relative_urls() {
    let injected = NarInfo::new(narinfo_input(
        "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo\nSig: forged",
        "nar/hash.nar",
        SIGNATURE,
    ))
    .expect_err("metadata lines cannot be injected");
    let absolute_url = NarInfo::new(narinfo_input(
        "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo",
        "https://cache.invalid/nar/hash.nar",
        SIGNATURE,
    ))
    .expect_err("cache policy does not belong in a narinfo URL");
    let traversal = NarInfo::new(narinfo_input(
        "/nix/store/../store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo",
        "nar/hash.nar",
        SIGNATURE,
    ))
    .expect_err("store paths cannot traverse");
    let malformed_signature = NarInfo::new(narinfo_input(
        "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo",
        "nar/hash.nar",
        "cache-1:not-base64",
    ))
    .expect_err("signature must be a canonical Ed25519 encoding");

    assert_eq!(injected.field(), "store_path");
    assert_eq!(absolute_url.field(), "url");
    assert_eq!(traversal.field(), "store_path");
    assert_eq!(malformed_signature.field(), "signatures");
}

#[test]
fn narinfo_rejects_noncanonical_reference_and_deriver_paths() {
    let reference = NarInfo::new(NarInfoInput {
        references: vec!["/tmp/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-ref".to_owned()],
        ..narinfo_input(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo",
            "nar/hash.nar",
            SIGNATURE,
        )
    })
    .expect_err("references must belong to the canonical Nix store");
    let deriver = NarInfo::new(NarInfoInput {
        deriver: Some("/nix/store/not-a-canonical-deriver.drv".to_owned()),
        ..narinfo_input(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo",
            "nar/hash.nar",
            SIGNATURE,
        )
    })
    .expect_err("deriver must have a canonical Nix store hash");

    assert_eq!(reference.field(), "references");
    assert_eq!(deriver.field(), "deriver");
}

#[test]
fn narinfo_rejects_illegal_or_overlong_store_names() {
    let illegal_reference = NarInfo::new(NarInfoInput {
        references: vec!["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bad name".to_owned()],
        ..narinfo_input(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo",
            "nar/hash.nar",
            SIGNATURE,
        )
    })
    .expect_err("reference name cannot contain spaces");
    let illegal_deriver = NarInfo::new(NarInfoInput {
        deriver: Some("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-bad:name.drv".to_owned()),
        ..narinfo_input(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo",
            "nar/hash.nar",
            SIGNATURE,
        )
    })
    .expect_err("deriver name cannot contain a colon");
    let overlong = NarInfo::new(NarInfoInput {
        references: vec![format!(
            "/nix/store/cccccccccccccccccccccccccccccccc-{}",
            "x".repeat(212)
        )],
        ..narinfo_input(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo",
            "nar/hash.nar",
            SIGNATURE,
        )
    })
    .expect_err("store name cannot exceed Nix's 211-byte limit");

    assert_eq!(illegal_reference.field(), "references");
    assert_eq!(illegal_deriver.field(), "deriver");
    assert_eq!(overlong.field(), "references");
}

#[test]
fn narinfo_rejects_sha256_with_nonzero_unused_high_bits() {
    let error = NarInfo::new(NarInfoInput {
        nar_hash: format!("sha256:z{}", "0".repeat(51)),
        ..narinfo_input(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo",
            "nar/hash.nar",
            SIGNATURE,
        )
    })
    .expect_err("52 base32 digits are not canonical when unused high bits are set");

    assert_eq!(error.field(), "nar_hash");
}

fn narinfo_input(store_path: &str, url: &str, signature: &str) -> NarInfoInput {
    NarInfoInput {
        store_path: store_path.to_owned(),
        url: url.to_owned(),
        compression: "none".to_owned(),
        file_hash: TREE_HASH.to_owned(),
        file_size: TREE_SIZE,
        nar_hash: TREE_HASH.to_owned(),
        nar_size: TREE_SIZE,
        references: Vec::new(),
        deriver: None,
        signatures: vec![signature.to_owned()],
        content_address: None,
    }
}
