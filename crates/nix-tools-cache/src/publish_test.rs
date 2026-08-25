use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::Duration;

use nix_tools_core::outcome::Error;
use nix_tools_core::process::Cancellation;

use crate::{
    AdapterError, AdapterResult, BatchPublicationRequest, BinaryCachePublisher, CacheObjectStore,
    CacheSigner, FailureClass, PublicationControl, PublicationReceipt, PublicationSource,
    StorePathIndex, StorePathInfo,
};

const SIGNATURE: &str = "cache-1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";

struct TempStore(PathBuf);

impl TempStore {
    fn new() -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "nix-tools-cache-publish-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&path).expect("create temporary store");
        Self(path)
    }

    fn path(&self, hash: &str, name: &str, contents: &[u8]) -> String {
        let path = self.0.join(format!("{hash}-{name}"));
        fs::create_dir_all(&path).expect("create store path");
        fs::write(path.join("data"), contents).expect("write store file");
        path.to_str().expect("UTF-8 temporary path").to_owned()
    }
}

impl Drop for TempStore {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct FakeIndex {
    entries: BTreeMap<String, StorePathInfo>,
    queries: Mutex<Vec<Vec<String>>>,
    failure: Option<Error>,
}

impl StorePathIndex for FakeIndex {
    fn info(
        &self,
        paths: &[String],
        control: &PublicationControl<'_>,
    ) -> AdapterResult<BTreeMap<String, StorePathInfo>> {
        control.check()?;
        self.queries.lock().expect("queries").push(paths.to_vec());
        if let Some(error) = &self.failure {
            return Err(error.clone().into());
        }
        Ok(paths
            .iter()
            .filter_map(|path| {
                self.entries
                    .get(path)
                    .map(|info| (path.clone(), info.clone()))
            })
            .collect())
    }
}

#[derive(Default)]
struct FakeSigner {
    fingerprints: Mutex<Vec<String>>,
    failure: Option<Error>,
    signature: Option<String>,
}

impl CacheSigner for FakeSigner {
    fn sign(&self, fingerprint: &str, control: &PublicationControl<'_>) -> AdapterResult<String> {
        control.check()?;
        self.fingerprints
            .lock()
            .expect("fingerprints")
            .push(fingerprint.to_owned());
        if let Some(error) = &self.failure {
            Err(error.clone().into())
        } else {
            Ok(self
                .signature
                .clone()
                .unwrap_or_else(|| SIGNATURE.to_owned()))
        }
    }
}

#[derive(Default)]
struct FakeObjectStore {
    existing: BTreeSet<String>,
    objects: Mutex<BTreeMap<String, (Vec<u8>, String)>>,
    probes: AtomicUsize,
    failure: Option<Error>,
    cancel_on_probe: Option<Cancellation>,
}

impl CacheObjectStore for FakeObjectStore {
    fn contains(&self, key: &str, control: &PublicationControl<'_>) -> AdapterResult<bool> {
        control.check()?;
        self.probes.fetch_add(1, Ordering::SeqCst);
        if let Some(cancellation) = &self.cancel_on_probe {
            cancellation.request(2);
        }
        control.check()?;
        Ok(self.existing.contains(key))
    }

    fn put(
        &self,
        key: &str,
        body: &[u8],
        content_type: &str,
        control: &PublicationControl<'_>,
    ) -> AdapterResult<()> {
        control.check()?;
        if let Some(error) = &self.failure {
            return Err(error.clone().into());
        }
        self.objects
            .lock()
            .expect("objects")
            .insert(key.to_owned(), (body.to_vec(), content_type.to_owned()));
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockingStage {
    Index,
    Probe,
    Signer,
    Upload,
    Metadata,
}

struct BlockingAdapters {
    stage: BlockingStage,
    entries: BTreeMap<String, StorePathInfo>,
    puts: AtomicUsize,
}

impl BlockingAdapters {
    fn block_until_control(control: &PublicationControl<'_>) -> AdapterResult<()> {
        let (_sender, receiver) = mpsc::channel::<()>();
        let _ = receiver.recv_timeout(control.remaining());
        control.check().map_err(AdapterError::from)
    }
}

impl StorePathIndex for BlockingAdapters {
    fn info(
        &self,
        paths: &[String],
        control: &PublicationControl<'_>,
    ) -> AdapterResult<BTreeMap<String, StorePathInfo>> {
        if self.stage == BlockingStage::Index {
            Self::block_until_control(control)?;
        }
        control.check()?;
        Ok(paths
            .iter()
            .filter_map(|path| {
                self.entries
                    .get(path)
                    .map(|info| (path.clone(), info.clone()))
            })
            .collect())
    }
}

impl CacheSigner for BlockingAdapters {
    fn sign(&self, _fingerprint: &str, control: &PublicationControl<'_>) -> AdapterResult<String> {
        if self.stage == BlockingStage::Signer {
            Self::block_until_control(control)?;
        }
        control.check()?;
        Ok(SIGNATURE.to_owned())
    }
}

impl CacheObjectStore for BlockingAdapters {
    fn contains(&self, _key: &str, control: &PublicationControl<'_>) -> AdapterResult<bool> {
        if self.stage == BlockingStage::Probe {
            Self::block_until_control(control)?;
        }
        control.check()?;
        Ok(false)
    }

    fn put(
        &self,
        _key: &str,
        _body: &[u8],
        _content_type: &str,
        control: &PublicationControl<'_>,
    ) -> AdapterResult<()> {
        let put = self.puts.fetch_add(1, Ordering::SeqCst);
        if self.stage == BlockingStage::Upload
            || (self.stage == BlockingStage::Metadata && put == 1)
        {
            Self::block_until_control(control)?;
        }
        control.check()?;
        Ok(())
    }
}

fn source(path: &str) -> PublicationSource {
    PublicationSource::new(path).expect("valid publication source")
}

fn request(available: &[PublicationSource], owned: &[&str]) -> BatchPublicationRequest {
    BatchPublicationRequest::select_owned(
        available,
        owned.iter().copied(),
        NonZeroUsize::new(2).expect("positive concurrency"),
        Duration::from_mins(2),
        Duration::from_mins(50),
    )
    .expect("select owned paths")
}

fn nar_identity(path: &str) -> (String, u64) {
    let mut bytes = Vec::new();
    let mut hashing = crate::HashingWriter::new(&mut bytes);
    crate::write_nar(Path::new(path), &mut hashing).expect("serialize NAR");
    hashing.finish()
}

fn index_for(paths: &[&str]) -> FakeIndex {
    FakeIndex {
        entries: paths
            .iter()
            .map(|path| {
                let (nar_hash, nar_size) = nar_identity(path);
                (
                    (*path).to_owned(),
                    StorePathInfo {
                        references: Vec::new(),
                        deriver: None,
                        nar_hash,
                        nar_size,
                    },
                )
            })
            .collect(),
        ..FakeIndex::default()
    }
}

#[test]
fn selection_exposes_only_owned_paths_to_the_store_and_cache() {
    let directory = TempStore::new();
    let owned = directory.path("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "owned", b"owned");
    let foreign = directory.path("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "foreign", b"foreign");
    let available = vec![source(&foreign), source(&owned)];
    let index = index_for(&[owned.as_str(), foreign.as_str()]);
    let signer = FakeSigner::default();
    let store = FakeObjectStore::default();
    let publisher = BinaryCachePublisher::new(&index, &signer, &store);

    let result = publisher
        .publish_batch(
            &request(&available, &[owned.as_str()]),
            &Cancellation::default(),
        )
        .expect("publish owned path");

    assert_eq!(result.paths.len(), 1);
    assert_eq!(result.paths[0].path, owned);
    assert_eq!(
        index.queries.lock().expect("queries").as_slice(),
        &[vec![owned]]
    );
    assert_eq!(store.probes.load(Ordering::SeqCst), 1);
}

#[test]
fn selecting_an_unavailable_or_duplicate_path_is_rejected() {
    let source = source("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-one");

    let missing = BatchPublicationRequest::select_owned(
        std::slice::from_ref(&source),
        ["/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-two"],
        NonZeroUsize::MIN,
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .expect_err("unavailable path is not owned by this schedule unit");
    let duplicate = BatchPublicationRequest::select_owned(
        &[source.clone(), source],
        ["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-one"],
        NonZeroUsize::MIN,
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .expect_err("ambiguous source set is rejected");

    assert_eq!(missing.class, FailureClass::Precondition);
    assert_eq!(duplicate.class, FailureClass::Precondition);
}

#[test]
fn source_rejects_traversal_and_line_injection() {
    let traversal =
        PublicationSource::new("/nix/store/../store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo")
            .expect_err("source path cannot traverse");
    let injection =
        PublicationSource::new("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo\nforged")
            .expect_err("source path cannot inject metadata");

    assert_eq!(traversal.class, FailureClass::Provenance);
    assert_eq!(injection.class, FailureClass::Provenance);
}

#[test]
fn uploaded_nar_is_exactly_the_bytes_that_are_hashed_and_signed() {
    let directory = TempStore::new();
    let path = directory.path("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "demo", b"payload");
    let index = index_for(&[path.as_str()]);
    let signer = FakeSigner::default();
    let store = FakeObjectStore::default();
    let publisher = BinaryCachePublisher::new(&index, &signer, &store);

    let result = publisher
        .publish_batch(
            &request(&[source(&path)], &[path.as_str()]),
            &Cancellation::default(),
        )
        .expect("publish path");

    assert_eq!(result.paths[0].result, Ok(PublicationReceipt::Uploaded));
    let objects = store.objects.lock().expect("objects");
    let narinfo = String::from_utf8(
        objects
            .get("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.narinfo")
            .expect("narinfo")
            .0
            .clone(),
    )
    .expect("UTF-8 narinfo");
    let nar_key = narinfo
        .lines()
        .find_map(|line| line.strip_prefix("URL: "))
        .expect("NAR URL");
    let uploaded = &objects.get(nar_key).expect("NAR object").0;
    let mut expected = Vec::new();
    let mut hashing = crate::HashingWriter::new(&mut expected);
    crate::write_nar(Path::new(&path), &mut hashing).expect("serialize expected NAR");
    let (hash, size) = hashing.finish();
    assert_eq!(uploaded, &expected);
    assert_eq!(
        signer.fingerprints.lock().expect("fingerprints")[0],
        crate::fingerprint(&path, &hash, size, &[])
    );
}

#[test]
fn hash_or_size_mismatch_fails_integrity_before_upload() {
    let directory = TempStore::new();
    let path = directory.path("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "demo", b"payload");
    let mut index = index_for(&[path.as_str()]);
    let info = index.entries.get_mut(&path).expect("path info");
    info.nar_hash = "sha256:0000000000000000000000000000000000000000000000000000".to_owned();
    info.nar_size += 8;
    let signer = FakeSigner::default();
    let store = FakeObjectStore::default();
    let publisher = BinaryCachePublisher::new(&index, &signer, &store);

    let result = publisher
        .publish_batch(
            &request(&[source(&path)], &[path.as_str()]),
            &Cancellation::default(),
        )
        .expect("settle batch");
    let error = result.paths[0]
        .result
        .as_ref()
        .expect_err("mismatched NAR cannot be published");

    assert_eq!(error.class, FailureClass::Integrity);
    assert!(store.objects.lock().expect("objects").is_empty());
}

#[test]
fn noncanonical_reference_metadata_fails_integrity_before_signing() {
    let directory = TempStore::new();
    let path = directory.path("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "demo", b"payload");
    let mut index = index_for(&[path.as_str()]);
    index.entries.get_mut(&path).expect("path info").references =
        vec!["/tmp/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-ref".to_owned()];
    let signer = FakeSigner::default();
    let store = FakeObjectStore::default();

    let result = BinaryCachePublisher::new(&index, &signer, &store)
        .publish_batch(
            &request(&[source(&path)], &[path.as_str()]),
            &Cancellation::default(),
        )
        .expect("settle invalid metadata");

    assert_eq!(
        result.paths[0]
            .result
            .as_ref()
            .expect_err("noncanonical reference cannot be signed")
            .class,
        FailureClass::Integrity
    );
    assert!(signer.fingerprints.lock().expect("fingerprints").is_empty());
}

#[test]
fn oversized_archive_is_bounded_and_classified_as_precondition() {
    let directory = TempStore::new();
    let path = directory.path("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "demo", &[0; 4_096]);
    let index = index_for(&[path.as_str()]);
    let signer = FakeSigner::default();
    let store = FakeObjectStore::default();
    let publisher = BinaryCachePublisher::new(&index, &signer, &store).with_max_nar_bytes(512);

    let result = publisher
        .publish_batch(
            &request(&[source(&path)], &[path.as_str()]),
            &Cancellation::default(),
        )
        .expect("settle batch");
    let error = result.paths[0]
        .result
        .as_ref()
        .expect_err("oversized NAR cannot be published");

    assert_eq!(error.class, FailureClass::Precondition);
    assert!(store.objects.lock().expect("objects").is_empty());
}

#[test]
fn cancellation_and_zero_deadlines_start_no_cache_io() {
    let directory = TempStore::new();
    let path = directory.path("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "demo", b"payload");
    let index = index_for(&[path.as_str()]);
    let signer = FakeSigner::default();
    let store = FakeObjectStore::default();
    let publisher = BinaryCachePublisher::new(&index, &signer, &store);
    let cancelled = Cancellation::default();
    cancelled.request(2);

    let cancelled_result = publisher
        .publish_batch(&request(&[source(&path)], &[path.as_str()]), &cancelled)
        .expect("settle cancelled batch");
    let timeout_request = BatchPublicationRequest::select_owned(
        &[source(&path)],
        [path.as_str()],
        NonZeroUsize::MIN,
        Duration::ZERO,
        Duration::ZERO,
    )
    .expect("valid request");
    let timeout_result = publisher
        .publish_batch(&timeout_request, &Cancellation::default())
        .expect("settle timed out batch");

    assert_eq!(
        cancelled_result.paths[0]
            .result
            .as_ref()
            .expect_err("cancelled")
            .class,
        FailureClass::Cancelled
    );
    assert_eq!(
        timeout_result.paths[0]
            .result
            .as_ref()
            .expect_err("timed out")
            .class,
        FailureClass::Timeout
    );
    assert_eq!(store.probes.load(Ordering::SeqCst), 0);
}

#[test]
fn malformed_store_path_is_rejected_before_cache_io() {
    let index = FakeIndex::default();
    let signer = FakeSigner::default();
    let store = FakeObjectStore::default();
    let publisher = BinaryCachePublisher::new(&index, &signer, &store);
    let malformed = "/nix/store/short-name";

    let result = publisher
        .publish_batch(
            &request(&[source(malformed)], &[malformed]),
            &Cancellation::default(),
        )
        .expect("settle invalid path");

    assert_eq!(
        result.paths[0]
            .result
            .as_ref()
            .expect_err("malformed")
            .class,
        FailureClass::Provenance
    );
    assert_eq!(store.probes.load(Ordering::SeqCst), 0);
}

#[test]
fn adapter_failures_have_stable_classes() {
    let directory = TempStore::new();
    let path = directory.path("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "demo", b"payload");
    let index = FakeIndex {
        failure: Some(Error::external("index unavailable")),
        ..index_for(&[path.as_str()])
    };
    let signer = FakeSigner::default();
    let store = FakeObjectStore::default();
    let publisher = BinaryCachePublisher::new(&index, &signer, &store);

    let error = publisher
        .publish_batch(
            &request(&[source(&path)], &[path.as_str()]),
            &Cancellation::default(),
        )
        .expect_err("index failure aborts batch");

    assert_eq!(error.class, FailureClass::Read);
}

#[test]
fn signer_and_object_store_failures_have_stable_classes() {
    let directory = TempStore::new();
    let path = directory.path("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "demo", b"payload");
    let index = index_for(&[path.as_str()]);
    let signer = FakeSigner {
        failure: Some(Error::external("signer unavailable")),
        ..FakeSigner::default()
    };
    let healthy_store = FakeObjectStore::default();
    let trust_result = BinaryCachePublisher::new(&index, &signer, &healthy_store)
        .publish_batch(
            &request(&[source(&path)], &[path.as_str()]),
            &Cancellation::default(),
        )
        .expect("settle signing failure");

    assert_eq!(
        trust_result.paths[0]
            .result
            .as_ref()
            .expect_err("unsigned object cannot be published")
            .class,
        FailureClass::Trust
    );
    assert!(healthy_store.objects.lock().expect("objects").is_empty());

    let healthy_signer = FakeSigner::default();
    let failing_store = FakeObjectStore {
        failure: Some(Error::external("object store unavailable")),
        ..FakeObjectStore::default()
    };
    let write_result = BinaryCachePublisher::new(&index, &healthy_signer, &failing_store)
        .publish_batch(
            &request(&[source(&path)], &[path.as_str()]),
            &Cancellation::default(),
        )
        .expect("settle object-store failure");

    assert_eq!(
        write_result.paths[0]
            .result
            .as_ref()
            .expect_err("unstored object cannot be published")
            .class,
        FailureClass::Write
    );
}

#[test]
fn malformed_signer_output_fails_trust_before_upload() {
    let directory = TempStore::new();
    let path = directory.path("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "demo", b"payload");
    let index = index_for(&[path.as_str()]);
    let signer = FakeSigner {
        signature: Some("cache-1:not-base64".to_owned()),
        ..FakeSigner::default()
    };
    let store = FakeObjectStore::default();

    let result = BinaryCachePublisher::new(&index, &signer, &store)
        .publish_batch(
            &request(&[source(&path)], &[path.as_str()]),
            &Cancellation::default(),
        )
        .expect("settle malformed signer output");

    assert_eq!(
        result.paths[0]
            .result
            .as_ref()
            .expect_err("invalid signature cannot be published")
            .class,
        FailureClass::Trust
    );
    assert!(store.objects.lock().expect("objects").is_empty());
}

#[test]
fn cancellation_during_existing_probe_wins_before_receipt() {
    let directory = TempStore::new();
    let path = directory.path("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "demo", b"payload");
    let index = index_for(&[path.as_str()]);
    let signer = FakeSigner::default();
    let cancellation = Cancellation::default();
    let store = FakeObjectStore {
        existing: BTreeSet::from(["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.narinfo".to_owned()]),
        cancel_on_probe: Some(cancellation.clone()),
        ..FakeObjectStore::default()
    };

    let result = BinaryCachePublisher::new(&index, &signer, &store)
        .publish_batch(&request(&[source(&path)], &[path.as_str()]), &cancellation)
        .expect("settle cancelled probe");

    assert_eq!(
        result.paths[0]
            .result
            .as_ref()
            .expect_err("cancellation wins")
            .class,
        FailureClass::Cancelled
    );
    assert!(signer.fingerprints.lock().expect("fingerprints").is_empty());
}

#[test]
fn cooperative_adapters_settle_every_io_stage_by_deadline() {
    let directory = TempStore::new();
    let path = directory.path("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "demo", b"payload");
    let entries = index_for(&[path.as_str()]).entries;

    for stage in [
        BlockingStage::Index,
        BlockingStage::Probe,
        BlockingStage::Signer,
        BlockingStage::Upload,
        BlockingStage::Metadata,
    ] {
        let adapters = BlockingAdapters {
            stage,
            entries: entries.clone(),
            puts: AtomicUsize::new(0),
        };
        let request = BatchPublicationRequest::select_owned(
            &[source(&path)],
            [path.as_str()],
            NonZeroUsize::MIN,
            Duration::from_millis(20),
            Duration::from_millis(20),
        )
        .expect("valid bounded request");
        let started = std::time::Instant::now();

        let result = BinaryCachePublisher::new(&adapters, &adapters, &adapters)
            .publish_batch(&request, &Cancellation::default());

        let class = match result {
            Ok(batch) => {
                batch.paths[0]
                    .result
                    .as_ref()
                    .expect_err("blocked adapter must time out")
                    .class
            }
            Err(error) => error.class,
        };
        assert_eq!(class, FailureClass::Timeout, "stage {stage:?}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "stage {stage:?} exceeded its cooperative bound"
        );
    }
}
