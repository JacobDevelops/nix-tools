//! Live interoperability checks against the Nix executable available to the test environment.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Write as _;
use std::num::NonZeroUsize;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nix_tools_cache::{
    AdapterResult, ArchiveCodec, BatchPublicationRequest, BinaryCachePublisher, CacheObjectStore,
    CacheSigner, EncodedArchive, HashingWriter, NarInfo, NarInfoInput, PublicationControl,
    PublicationReceipt, PublicationSource, StorePathIndex, StorePathInfo, write_nar,
};
use nix_tools_core::outcome::Error;
use nix_tools_core::process::Cancellation;

const ADDITIONAL_SIGNATURE: &str = "other-1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "nix-tools-cache-interop-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&path).expect("create temporary directory");
        Self(path)
    }
}

struct IdentityCodec;

impl ArchiveCodec for IdentityCodec {
    fn encode(
        &self,
        nar: &[u8],
        nar_hash: &str,
        control: &PublicationControl<'_>,
    ) -> AdapterResult<EncodedArchive> {
        control.check()?;
        Ok(EncodedArchive {
            body: nar.to_vec(),
            compression: "none".to_owned(),
            object_key: format!("nar/{}.nar", nar_hash.trim_start_matches("sha256:")),
        })
    }

    fn decode(
        &self,
        archive: &[u8],
        compression: &str,
        control: &PublicationControl<'_>,
    ) -> AdapterResult<Vec<u8>> {
        control.check()?;
        if compression != "none" {
            return Err(Error::external("unsupported integration compression").into());
        }
        Ok(archive.to_vec())
    }
}

struct FilesystemIndex(BTreeMap<String, StorePathInfo>);

impl StorePathIndex for FilesystemIndex {
    fn info(
        &self,
        paths: &[String],
        control: &PublicationControl<'_>,
    ) -> AdapterResult<BTreeMap<String, StorePathInfo>> {
        control.check()?;
        Ok(paths
            .iter()
            .filter_map(|path| self.0.get(path).map(|info| (path.clone(), info.clone())))
            .collect())
    }
}

struct FilesystemObjectStore {
    root: PathBuf,
    writes: Mutex<Vec<String>>,
}

impl CacheObjectStore for FilesystemObjectStore {
    fn get(&self, key: &str, control: &PublicationControl<'_>) -> AdapterResult<Option<Vec<u8>>> {
        control.check()?;
        match fs::read(self.root.join(key)) {
            Ok(body) => Ok(Some(body)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(Error::io(format!("read cache object {key}: {error}")).into()),
        }
    }

    fn put(
        &self,
        key: &str,
        body: &[u8],
        _content_type: &str,
        control: &PublicationControl<'_>,
    ) -> AdapterResult<()> {
        control.check()?;
        let destination = self.root.join(key);
        let parent = destination
            .parent()
            .ok_or_else(|| Error::external("cache object has no parent"))?;
        fs::create_dir_all(parent)
            .map_err(|error| Error::io(format!("create cache object parent: {error}")))?;
        let temporary = destination.with_extension("publishing");
        fs::write(&temporary, body)
            .map_err(|error| Error::io(format!("write cache object: {error}")))?;
        control.check()?;
        fs::rename(&temporary, &destination)
            .map_err(|error| Error::io(format!("commit cache object: {error}")))?;
        self.writes
            .lock()
            .expect("cache writes")
            .push(key.to_owned());
        Ok(())
    }
}

struct NixKeySigner {
    name: String,
    key_der: PathBuf,
    work: PathBuf,
    sequence: AtomicU64,
}

impl NixKeySigner {
    fn new(secret: &str, work: &Path) -> Self {
        let (name, encoded) = secret.trim().split_once(':').expect("Nix secret key");
        let raw = openssl_with_input(["base64", "-d", "-A"], encoded.as_bytes());
        assert_eq!(raw.len(), 64, "Nix Ed25519 secret length");
        let mut der = hex("302e020100300506032b657004220420");
        der.extend_from_slice(&raw[..32]);
        let key_der = work.join("secret.der");
        fs::write(&key_der, der).expect("write temporary DER key");
        Self {
            name: name.to_owned(),
            key_der,
            work: work.to_owned(),
            sequence: AtomicU64::new(0),
        }
    }

    fn signature(&self, fingerprint: &str) -> AdapterResult<String> {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst);
        let message = self.work.join(format!("fingerprint-{sequence}"));
        let signature = self.work.join(format!("signature-{sequence}"));
        fs::write(&message, fingerprint)
            .map_err(|error| Error::io(format!("write fingerprint: {error}")))?;
        let output = Command::new("openssl")
            .args(["pkeyutl", "-sign", "-rawin", "-keyform", "DER", "-inkey"])
            .arg(&self.key_der)
            .arg("-in")
            .arg(&message)
            .arg("-out")
            .arg(&signature)
            .output()
            .map_err(|error| Error::external(format!("run OpenSSL signer: {error}")))?;
        if !output.status.success() {
            return Err(Error::external(format!(
                "OpenSSL signer failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ))
            .into());
        }
        let raw =
            fs::read(signature).map_err(|error| Error::io(format!("read signature: {error}")))?;
        Ok(format!(
            "{}:{}",
            self.name,
            text(openssl_with_input(["base64", "-A"], &raw))
        ))
    }
}

impl CacheSigner for NixKeySigner {
    fn sign(&self, fingerprint: &str, control: &PublicationControl<'_>) -> AdapterResult<String> {
        control.check()?;
        let signature = self.signature(fingerprint)?;
        control.check()?;
        Ok(signature)
    }

    fn verify(
        &self,
        fingerprint: &str,
        signatures: &[String],
        control: &PublicationControl<'_>,
    ) -> AdapterResult<bool> {
        control.check()?;
        Ok(signatures.contains(&self.signature(fingerprint)?))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn nix_available() -> bool {
    Command::new("nix")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn openssl_raw_ed25519_signing_available(work: &Path) -> bool {
    let mut key = hex("302e020100300506032b657004220420");
    key.extend_from_slice(&[0; 32]);
    let key_path = work.join("openssl-probe-key.der");
    let message_path = work.join("openssl-probe-message");
    let signature_path = work.join("openssl-probe-signature");
    if fs::write(&key_path, key).is_err() || fs::write(&message_path, b"probe").is_err() {
        return false;
    }
    Command::new("openssl")
        .args(["pkeyutl", "-sign", "-rawin", "-keyform", "DER", "-inkey"])
        .arg(key_path)
        .arg("-in")
        .arg(message_path)
        .arg("-out")
        .arg(signature_path)
        .output()
        .is_ok_and(|output| output.status.success())
}

fn nix<I, S>(arguments: I) -> Vec<u8>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("nix")
        .args(["--extra-experimental-features", "nix-command flakes"])
        .args(arguments)
        .output()
        .expect("run Nix");
    assert!(
        output.status.success(),
        "Nix failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn nix_with_input<I, S>(arguments: I, input: &[u8]) -> Vec<u8>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut child = Command::new("nix")
        .args(["--extra-experimental-features", "nix-command flakes"])
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run Nix");
    child
        .stdin
        .take()
        .expect("Nix stdin")
        .write_all(input)
        .expect("write Nix stdin");
    let output = child.wait_with_output().expect("wait for Nix");
    assert!(
        output.status.success(),
        "Nix failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn openssl_with_input<I, S>(arguments: I, input: &[u8]) -> Vec<u8>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut child = Command::new("openssl")
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run OpenSSL");
    child
        .stdin
        .take()
        .expect("OpenSSL stdin")
        .write_all(input)
        .expect("write OpenSSL stdin");
    let output = child.wait_with_output().expect("wait for OpenSSL");
    assert!(
        output.status.success(),
        "OpenSSL failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("hex UTF-8"), 16).expect("hex byte")
        })
        .collect()
}

fn text(output: Vec<u8>) -> String {
    String::from_utf8(output)
        .expect("Nix returned UTF-8")
        .trim()
        .to_owned()
}

fn verify_multi_signature_narinfo(
    cache: &Path,
    hash_part: &str,
    narinfo: &str,
    signature: &str,
    public_key: &str,
    store_path: &str,
) {
    let signed_line = format!("Sig: {signature}\n");
    let multi_signature_narinfo = narinfo.replacen(
        &signed_line,
        &format!("{signed_line}Sig: {ADDITIONAL_SIGNATURE}\n"),
        1,
    );
    assert_eq!(
        NarInfo::parse(&multi_signature_narinfo)
            .expect("parse Nix narinfo with an additional signature")
            .to_string(),
        multi_signature_narinfo
    );
    fs::write(
        cache.join(format!("{hash_part}.narinfo")),
        multi_signature_narinfo,
    )
    .expect("write multi-signature narinfo");
    nix([
        "--store",
        &format!("file://{}", cache.display()),
        "store",
        "verify",
        "--sigs-needed",
        "1",
        "--trusted-public-keys",
        public_key,
        store_path,
    ]);
}

fn store_info(path: &Path, references: Vec<String>) -> StorePathInfo {
    let mut archive = Vec::new();
    let mut hashing = HashingWriter::new(&mut archive);
    write_nar(path, &mut hashing).expect("serialize local store path");
    let (nar_hash, nar_size) = hashing.finish();
    StorePathInfo {
        references,
        deriver: None,
        nar_hash,
        nar_size,
        content_address: None,
    }
}

fn create_publisher_reference_store(root: &Path) -> (PathBuf, String, String) {
    let store = root.join("source-store");
    let input = root.join("publisher-prerequisite");
    fs::create_dir_all(&input).expect("create prerequisite input");
    fs::write(input.join("data"), b"publisher prerequisite\n").expect("write prerequisite");
    let store_uri = store.to_str().expect("UTF-8 source store");
    let prerequisite = text(nix([
        "--store",
        store_uri,
        "store",
        "add",
        input.to_str().expect("UTF-8 input"),
    ]));
    let expression = format!(
        "builtins.toFile \"publisher-dependant\" (toString (builtins.storePath \"{prerequisite}\"))"
    );
    let dependant = text(nix([
        "--store",
        store_uri,
        "eval",
        "--impure",
        "--raw",
        "--expr",
        &expression,
    ]));
    (store, prerequisite, dependant)
}

fn verify_and_copy_publisher_cache(
    cache: &Path,
    destination_store: &Path,
    public_key: &str,
    dependant: &str,
    prerequisite: &str,
) {
    let cache_uri = format!("file://{}", cache.display());
    nix([
        "--store",
        &cache_uri,
        "store",
        "verify",
        "--sigs-needed",
        "1",
        "--trusted-public-keys",
        public_key,
        dependant,
    ]);
    nix([
        "--store",
        destination_store.to_str().expect("UTF-8 destination store"),
        "copy",
        "--from",
        &cache_uri,
        "--trusted-public-keys",
        public_key,
        dependant,
    ]);
    for store_path in [dependant, prerequisite] {
        assert!(
            destination_store
                .join("nix/store")
                .join(Path::new(store_path).file_name().expect("store base name"))
                .exists()
        );
    }
}

#[test]
fn archive_bytes_and_hash_match_live_nix() {
    if !nix_available() {
        eprintln!("skipping live Nix interoperability test: nix is unavailable");
        return;
    }
    let fixture = TempDir::new();
    let tree = fixture.0.join("tree");
    fs::create_dir_all(tree.join("empty")).expect("create empty directory");
    fs::create_dir_all(tree.join("sub")).expect("create subdirectory");
    fs::write(tree.join("plain"), b"plain\n").expect("write plain file");
    fs::write(tree.join("sub/executable"), b"#!/bin/sh\nexit 0\n").expect("write executable");
    fs::set_permissions(
        tree.join("sub/executable"),
        fs::Permissions::from_mode(0o755),
    )
    .expect("make executable");
    std::os::unix::fs::symlink("../plain", tree.join("sub/link")).expect("create symlink");
    std::os::unix::fs::symlink("missing", tree.join("dangling")).expect("create dangling symlink");

    let nix_archive = nix(["nar", "pack", tree.to_str().expect("UTF-8 fixture path")]);
    let nix_hash = text(nix([
        "hash",
        "path",
        "--type",
        "sha256",
        "--base32",
        tree.to_str().expect("UTF-8 fixture path"),
    ]));
    let mut archive = Vec::new();
    let mut hashing = HashingWriter::new(&mut archive);
    write_nar(&tree, &mut hashing).expect("serialize fixture");
    let (hash, size) = hashing.finish();

    assert_eq!(archive, nix_archive);
    assert_eq!(hash, format!("sha256:{nix_hash}"));
    assert_eq!(size, archive.len() as u64);
}

#[test]
fn narinfo_references_and_signature_match_live_nix() {
    if !nix_available() {
        eprintln!("skipping live Nix interoperability test: nix is unavailable");
        return;
    }
    let fixture = TempDir::new();
    let store = fixture.0.join("store");
    let cache = fixture.0.join("cache");
    let prerequisite_input = fixture.0.join("prerequisite");
    fs::create_dir_all(&prerequisite_input).expect("create prerequisite input");
    fs::write(prerequisite_input.join("data"), b"prerequisite\n").expect("write prerequisite");
    let store_uri = store.to_str().expect("UTF-8 store path");
    let prerequisite = text(nix([
        "--store",
        store_uri,
        "store",
        "add",
        prerequisite_input.to_str().expect("UTF-8 input path"),
    ]));
    let expression =
        format!("builtins.toFile \"dependant\" (toString (builtins.storePath \"{prerequisite}\"))");
    let dependant = text(nix([
        "--store",
        store_uri,
        "eval",
        "--impure",
        "--raw",
        "--expr",
        &expression,
    ]));
    let secret = nix(["key", "generate-secret", "--key-name", "interop-1"]);
    let public = text(nix_with_input(["key", "convert-secret-to-public"], &secret));
    let secret_path = fixture.0.join("secret-key");
    fs::write(&secret_path, secret).expect("write temporary signing key");
    let destination = format!(
        "file://{}?compression=none&secret-key={}",
        cache.display(),
        secret_path.display()
    );
    nix([
        "--store",
        store_uri,
        "copy",
        "--to",
        &destination,
        &dependant,
    ]);

    let hash_part = dependant
        .strip_prefix("/nix/store/")
        .expect("canonical store prefix")
        .split_once('-')
        .expect("store hash separator")
        .0;
    let nix_narinfo =
        fs::read_to_string(cache.join(format!("{hash_part}.narinfo"))).expect("read Nix narinfo");
    let fields = nix_narinfo
        .lines()
        .filter_map(|line| line.split_once(": "))
        .collect::<BTreeMap<_, _>>();
    let references = fields["References"]
        .split_whitespace()
        .map(|reference| format!("/nix/store/{reference}"))
        .collect::<Vec<_>>();
    assert_eq!(references, vec![prerequisite.clone()]);
    let narinfo = NarInfo::new(NarInfoInput {
        store_path: fields["StorePath"].to_owned(),
        url: fields["URL"].to_owned(),
        compression: fields["Compression"].to_owned(),
        file_hash: fields["FileHash"].to_owned(),
        file_size: fields["FileSize"].parse().expect("file size"),
        nar_hash: fields["NarHash"].to_owned(),
        nar_size: fields["NarSize"].parse().expect("NAR size"),
        references,
        deriver: fields
            .get("Deriver")
            .map(|deriver| format!("/nix/store/{deriver}")),
        signatures: nix_narinfo
            .lines()
            .filter_map(|line| line.strip_prefix("Sig: ").map(str::to_owned))
            .collect(),
        content_address: fields.get("CA").map(|value| (*value).to_owned()),
    })
    .expect("construct narinfo from Nix metadata");

    assert_eq!(narinfo.to_string(), nix_narinfo);
    let physical_path = store
        .join("nix/store")
        .join(Path::new(&dependant).file_name().expect("store base name"));
    let mut archive = Vec::new();
    write_nar(&physical_path, &mut archive).expect("archive alternate local store path");
    assert_eq!(
        archive,
        fs::read(cache.join(fields["URL"])).expect("read cached NAR")
    );
    verify_multi_signature_narinfo(
        &cache,
        hash_part,
        &nix_narinfo,
        fields["Sig"],
        &public,
        &dependant,
    );
}

#[test]
fn publisher_output_is_accepted_by_live_nix() {
    let fixture = TempDir::new();
    if !nix_available() || !openssl_raw_ed25519_signing_available(&fixture.0) {
        eprintln!(
            "skipping live publisher interoperability test: nix or OpenSSL raw Ed25519 signing is unavailable"
        );
        return;
    }
    let destination_store = fixture.0.join("destination-store");
    let cache = fixture.0.join("rust-cache");
    fs::create_dir_all(&cache).expect("create cache");
    fs::write(
        cache.join("nix-cache-info"),
        b"StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n",
    )
    .expect("write cache info");
    let (source_store, prerequisite, dependant) = create_publisher_reference_store(&fixture.0);
    let secret = text(nix([
        "key",
        "generate-secret",
        "--key-name",
        "publisher-interop-1",
    ]));
    let public = text(nix_with_input(
        ["key", "convert-secret-to-public"],
        secret.as_bytes(),
    ));
    let physical = |store_path: &str| {
        source_store
            .join("nix/store")
            .join(Path::new(store_path).file_name().expect("store base name"))
    };
    let index = FilesystemIndex(BTreeMap::from([
        (
            prerequisite.clone(),
            store_info(&physical(&prerequisite), Vec::new()),
        ),
        (
            dependant.clone(),
            store_info(&physical(&dependant), vec![prerequisite.clone()]),
        ),
    ]));
    let signer = NixKeySigner::new(&secret, &fixture.0);
    let object_store = FilesystemObjectStore {
        root: cache.clone(),
        writes: Mutex::new(Vec::new()),
    };
    let sources = [
        PublicationSource::from_archive_path(&dependant, physical(&dependant).to_string_lossy())
            .expect("dependant source"),
        PublicationSource::from_archive_path(
            &prerequisite,
            physical(&prerequisite).to_string_lossy(),
        )
        .expect("prerequisite source"),
    ];
    let request = BatchPublicationRequest::select_owned(
        &sources,
        [&dependant, &prerequisite].into_iter().map(String::as_str),
        NonZeroUsize::new(2).expect("positive concurrency"),
        Duration::from_secs(30),
        Duration::from_mins(1),
    )
    .expect("publication request");

    let published = BinaryCachePublisher::new(&index, &signer, &object_store, &IdentityCodec)
        .publish_batch(&request, &Cancellation::default())
        .expect("publish Rust cache");

    assert!(
        published
            .paths
            .iter()
            .all(|path| path.result == Ok(PublicationReceipt::Uploaded))
    );
    let dependant_hash = dependant
        .strip_prefix("/nix/store/")
        .expect("store prefix")
        .split_once('-')
        .expect("store hash")
        .0;
    let dependant_metadata_key = format!("{dependant_hash}.narinfo");
    assert_eq!(
        object_store
            .writes
            .lock()
            .expect("cache writes")
            .last()
            .map(String::as_str),
        Some(dependant_metadata_key.as_str())
    );
    verify_and_copy_publisher_cache(
        &cache,
        &destination_store,
        &public,
        &dependant,
        &prerequisite,
    );
}
