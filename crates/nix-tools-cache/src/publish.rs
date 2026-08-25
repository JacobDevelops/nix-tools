//! Bounded, cancellation-aware publication through caller-supplied cache adapters.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use nix_tools_core::outcome::{Error, ExitCode};
use nix_tools_core::process::Cancellation;

use crate::nar::{self, HashingWriter, NarInfo};

const NAR_CONTENT_TYPE: &str = "application/x-nix-nar";
const NARINFO_CONTENT_TYPE: &str = "text/x-nix-narinfo";
const DEFAULT_MAX_NAR_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const NIX_BASE32: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// Canonical metadata recorded by the local Nix store for one path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorePathInfo {
    /// Store paths referenced by this path.
    pub references: Vec<String>,
    /// Derivation that produced the path, when recorded.
    pub deriver: Option<String>,
    /// Canonical NAR SHA-256 hash recorded by the store.
    pub nar_hash: String,
    /// Canonical NAR byte count recorded by the store.
    pub nar_size: u64,
}

/// Reads authoritative metadata for selected local store paths.
pub trait StorePathIndex: Send + Sync {
    /// Returns metadata keyed by path. A missing requested key means the path is not publishable.
    ///
    /// Implementations must not inspect paths outside `paths`. They must cooperatively bound every
    /// wait using `control.remaining()` and call `control.check()` around blocking I/O. The
    /// publisher cannot preempt a synchronous implementation that ignores this contract.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the local store cannot be queried.
    fn info(
        &self,
        paths: &[String],
        control: &PublicationControl<'_>,
    ) -> AdapterResult<BTreeMap<String, StorePathInfo>>;
}

/// Signs canonical binary-cache fingerprints.
pub trait CacheSigner: Send + Sync {
    /// Returns the complete `key-name:base64-signature` narinfo value.
    ///
    /// Implementations must not include signing material in returned errors. They must
    /// cooperatively bound every wait using `control.remaining()` and call `control.check()`
    /// around blocking I/O.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the signer is unavailable or refuses the fingerprint.
    fn sign(&self, fingerprint: &str, control: &PublicationControl<'_>) -> AdapterResult<String>;
}

/// Stores opaque binary-cache objects under relative keys.
pub trait CacheObjectStore: Send + Sync {
    /// Reports whether a durable object exists at `key`.
    ///
    /// Implementations must cooperatively bound every wait using `control.remaining()` and call
    /// `control.check()` around blocking I/O.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when existence cannot be determined.
    fn contains(&self, key: &str, control: &PublicationControl<'_>) -> AdapterResult<bool>;

    /// Durably stores `body` without transforming it.
    ///
    /// # Errors
    ///
    /// Implementations must cooperatively bound I/O with `control`. Metadata objects must become
    /// visible atomically only on `Ok`; on a control or backend error they must remain absent. This
    /// lets cancellation win before metadata publication without holding an unbounded commit gate.
    ///
    /// The publisher cannot hard-preempt a synchronous implementation that ignores `control`.
    ///
    /// Returns an adapter error unless the exact bytes are durable when this method returns.
    fn put(
        &self,
        key: &str,
        body: &[u8],
        content_type: &str,
        control: &PublicationControl<'_>,
    ) -> AdapterResult<()>;
}

/// Error returned across an external adapter boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdapterError {
    /// Failure from the underlying store, signer, or object-store implementation.
    Backend(Error),
    /// Cooperative deadline or cancellation failure reported by [`PublicationControl`].
    Control(PublicationError),
}

impl From<Error> for AdapterError {
    fn from(error: Error) -> Self {
        Self::Backend(error)
    }
}

impl From<PublicationError> for AdapterError {
    fn from(error: PublicationError) -> Self {
        Self::Control(error)
    }
}

impl fmt::Display for AdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(error) => error.fmt(formatter),
            Self::Control(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for AdapterError {}

/// Result returned by cache adapter methods.
pub type AdapterResult<T> = Result<T, AdapterError>;

/// A local store path eligible for explicit selection by a publication unit.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PublicationSource {
    path: String,
}

impl PublicationSource {
    /// Creates a source without applying repository or scheduling policy.
    ///
    /// # Errors
    ///
    /// Returns a provenance failure unless `path` is an absolute, single-line path.
    pub fn new(path: impl Into<String>) -> Result<Self, PublicationError> {
        let path = path.into();
        if !path.starts_with('/')
            || path.ends_with('/')
            || path.contains(['\r', '\n', '\0'])
            || path
                .split('/')
                .skip(1)
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(PublicationError::new(
                FailureClass::Provenance,
                format!("{path:?} is not an absolute single-line store path"),
                Some(ExitCode::PREFLIGHT),
            ));
        }
        Ok(Self { path })
    }

    /// Returns the selected local path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// A structurally selective batch containing only paths owned by one calling unit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchPublicationRequest {
    sources: Vec<PublicationSource>,
    max_concurrency: NonZeroUsize,
    per_source_deadline: Duration,
    batch_deadline: Duration,
}

impl BatchPublicationRequest {
    /// Selects `owned_paths` from an available source set while rejecting ambiguity and absence.
    ///
    /// Selected sources retain `available` order. The index and publisher can therefore observe
    /// only this unit's owned paths, never the unselected source set.
    ///
    /// # Errors
    ///
    /// Returns a precondition failure if an available source is duplicated or an owned path is
    /// absent from `available`.
    pub fn select_owned<'a>(
        available: &[PublicationSource],
        owned_paths: impl IntoIterator<Item = &'a str>,
        max_concurrency: NonZeroUsize,
        per_source_deadline: Duration,
        batch_deadline: Duration,
    ) -> Result<Self, PublicationError> {
        let mut available_paths = BTreeSet::new();
        for source in available {
            if !available_paths.insert(source.path.as_str()) {
                return Err(PublicationError::new(
                    FailureClass::Precondition,
                    format!(
                        "{} appears more than once in available sources",
                        source.path
                    ),
                    Some(ExitCode::PREFLIGHT),
                ));
            }
        }
        let owned = owned_paths.into_iter().collect::<BTreeSet<_>>();
        if let Some(missing) = owned.difference(&available_paths).next() {
            return Err(PublicationError::new(
                FailureClass::Precondition,
                format!("{missing} is not available to this publication unit"),
                Some(ExitCode::PREFLIGHT),
            ));
        }
        Ok(Self {
            sources: available
                .iter()
                .filter(|source| owned.contains(source.path.as_str()))
                .cloned()
                .collect(),
            max_concurrency,
            per_source_deadline,
            batch_deadline,
        })
    }

    /// Returns selected sources in deterministic available-source order.
    #[must_use]
    pub fn sources(&self) -> &[PublicationSource] {
        &self.sources
    }
}

/// Result for one selected path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathPublicationResult {
    /// Store path that settled.
    pub path: String,
    /// Wall time spent after the worker selected the path.
    pub duration: Duration,
    /// Upload receipt or stable classified failure.
    pub result: Result<PublicationReceipt, PublicationError>,
}

/// Deterministically ordered results for every selected path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchPublicationResult {
    /// One result per selected source, in request order.
    pub paths: Vec<PathPublicationResult>,
}

/// Whether publication created objects or found complete metadata already present.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationReceipt {
    /// The narinfo already existed, so no local path bytes were read.
    Existing,
    /// The NAR and narinfo were stored successfully.
    Uploaded,
}

/// Stable failure category independent of any cache provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureClass {
    /// Local store or filesystem read failed.
    Read,
    /// Signature production or validation failed.
    Trust,
    /// Object-store probing or writing failed.
    Write,
    /// A configured time budget was exhausted.
    Timeout,
    /// Serialized bytes disagreed with authoritative store metadata.
    Integrity,
    /// A path or its store provenance was invalid.
    Provenance,
    /// A bounded-publication precondition was not met.
    Precondition,
    /// Cancellation won before the next irreversible publication step.
    Cancelled,
}

/// A classified publication failure with an optional process exit status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationError {
    /// Stable failure category.
    pub class: FailureClass,
    /// Human-readable adapter-safe context.
    pub message: String,
    /// Suggested process status when one is meaningful.
    pub exit_code: Option<ExitCode>,
}

impl PublicationError {
    fn new(class: FailureClass, message: impl Into<String>, exit_code: Option<ExitCode>) -> Self {
        Self {
            class,
            message: message.into(),
            exit_code,
        }
    }
}

impl fmt::Display for PublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PublicationError {}

#[derive(Clone, Copy)]
enum DeadlineKind {
    Batch,
    Source,
}

/// Cooperative cancellation and deadline control passed to every external adapter call.
///
/// Adapters must use [`Self::remaining`] to cap each blocking wait and call [`Self::check`]
/// before and after I/O. This is a cooperative contract: it bounds conforming adapters but cannot
/// hard-preempt arbitrary synchronous code that ignores the control.
pub struct PublicationControl<'a> {
    started: Instant,
    budget: Duration,
    kind: DeadlineKind,
    cancellation: &'a Cancellation,
}

impl PublicationControl<'_> {
    /// Returns the duration left before the deadline, saturating at zero.
    ///
    /// Cancellation is reported by [`Self::check`], so adapters should call it before waiting.
    #[must_use]
    pub fn remaining(&self) -> Duration {
        self.budget.saturating_sub(self.started.elapsed())
    }

    /// Checks cancellation first and then the configured deadline.
    ///
    /// # Errors
    ///
    /// Returns a typed cancelled or timeout failure when work must stop.
    pub fn check(&self) -> Result<(), PublicationError> {
        if let Some(signal) = self.cancellation.signal() {
            return Err(cancelled(signal));
        }
        if self.remaining().is_zero() {
            return Err(match self.kind {
                DeadlineKind::Batch => batch_timeout(self.budget),
                DeadlineKind::Source => source_timeout(self.budget),
            });
        }
        Ok(())
    }
}

impl<'a> PublicationControl<'a> {
    const fn new(
        started: Instant,
        budget: Duration,
        kind: DeadlineKind,
        cancellation: &'a Cancellation,
    ) -> Self {
        Self {
            started,
            budget,
            kind,
            cancellation,
        }
    }
}

/// Concurrent publisher over caller-supplied store, signer, and object-store adapters.
pub struct BinaryCachePublisher<'a> {
    index: &'a dyn StorePathIndex,
    signer: &'a dyn CacheSigner,
    store: &'a dyn CacheObjectStore,
    max_nar_bytes: u64,
}

impl<'a> BinaryCachePublisher<'a> {
    /// Creates a publisher with a two-GiB per-path in-memory ceiling.
    #[must_use]
    pub const fn new(
        index: &'a dyn StorePathIndex,
        signer: &'a dyn CacheSigner,
        store: &'a dyn CacheObjectStore,
    ) -> Self {
        Self {
            index,
            signer,
            store,
            max_nar_bytes: DEFAULT_MAX_NAR_BYTES,
        }
    }

    /// Replaces the per-path archive ceiling.
    #[must_use]
    pub const fn with_max_nar_bytes(mut self, limit: u64) -> Self {
        self.max_nar_bytes = limit;
        self
    }

    /// Publishes every selected path with bounded memory, concurrency, and time budgets.
    ///
    /// The NAR is serialized once into a bounded buffer, and those exact buffered bytes are both
    /// hashed and uploaded. A trust, integrity, provenance, precondition, or cancellation failure
    /// prevents queued paths from starting; already-running paths may settle independently.
    ///
    /// # Errors
    ///
    /// Returns a batch-level read failure when the selected local paths cannot be indexed. All
    /// path-local failures are returned in the corresponding path result.
    pub fn publish_batch(
        &self,
        request: &BatchPublicationRequest,
        cancellation: &Cancellation,
    ) -> Result<BatchPublicationResult, PublicationError> {
        if request.sources.is_empty() {
            return Ok(BatchPublicationResult { paths: Vec::new() });
        }
        let batch_started = Instant::now();
        let batch_control = PublicationControl::new(
            batch_started,
            request.batch_deadline,
            DeadlineKind::Batch,
            cancellation,
        );
        if let Err(error) = batch_control.check() {
            return Ok(repeat_failure(request, &error));
        }
        let paths = request
            .sources
            .iter()
            .map(|source| source.path.clone())
            .collect::<Vec<_>>();
        let indexed = self.index.info(&paths, &batch_control);
        batch_control.check()?;
        let info = indexed
            .map_err(|error| classify_adapter(FailureClass::Read, "query local store", error))?;

        let next = AtomicUsize::new(0);
        let outcomes = Mutex::new(BTreeMap::new());
        let halt: Mutex<Option<PublicationError>> = Mutex::new(None);
        let workers = request.max_concurrency.get().min(request.sources.len());
        thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let index = next.fetch_add(1, Ordering::SeqCst);
                        let Some(source) = request.sources.get(index) else {
                            return;
                        };
                        let started = Instant::now();
                        let batch_remaining = request
                            .batch_deadline
                            .saturating_sub(batch_started.elapsed());
                        let batch_expired = batch_remaining.is_zero();
                        let result = if let Some(error) = halted(&halt) {
                            Err(error)
                        } else if batch_expired {
                            Err(batch_timeout(request.batch_deadline))
                        } else {
                            let control = PublicationControl::new(
                                started,
                                request.per_source_deadline.min(batch_remaining),
                                DeadlineKind::Source,
                                cancellation,
                            );
                            self.publish_one(source, &info, &control)
                        };
                        if let Err(error) = &result
                            && (batch_expired || stops_queue(error.class))
                        {
                            halt.lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .get_or_insert_with(|| error.clone());
                        }
                        outcomes
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(
                                source.path.clone(),
                                PathPublicationResult {
                                    path: source.path.clone(),
                                    duration: started.elapsed(),
                                    result,
                                },
                            );
                    }
                });
            }
        });

        let mut outcomes = outcomes
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        Ok(BatchPublicationResult {
            paths: request
                .sources
                .iter()
                .filter_map(|source| outcomes.remove(&source.path))
                .collect(),
        })
    }

    fn publish_one(
        &self,
        source: &PublicationSource,
        info: &BTreeMap<String, StorePathInfo>,
        control: &PublicationControl<'_>,
    ) -> Result<PublicationReceipt, PublicationError> {
        let hash_part = store_hash_part(&source.path)?;
        let Some(info) = info.get(&source.path) else {
            return Err(PublicationError::new(
                FailureClass::Provenance,
                format!("{} is not present in the local store", source.path),
                Some(ExitCode::PREFLIGHT),
            ));
        };
        let mut references = info.references.clone();
        references.sort();
        references.dedup();
        validate_store_metadata(&references, info.deriver.as_deref())?;
        control.check()?;
        let narinfo_key = format!("{hash_part}.narinfo");
        let existing = self
            .store
            .contains(&narinfo_key, control)
            .map_err(|error| classify_adapter(FailureClass::Write, "probe cache object", error))?;
        control.check()?;
        if existing {
            return Ok(PublicationReceipt::Existing);
        }

        let archive = self.serialize(source, control)?;
        let nar_hash = archive.hash;
        let nar_size = archive.size;
        if nar_hash != info.nar_hash || nar_size != info.nar_size {
            return Err(PublicationError::new(
                FailureClass::Integrity,
                format!(
                    "{} serialized as {nar_hash} ({nar_size} bytes), but the store records {} ({} bytes)",
                    source.path, info.nar_hash, info.nar_size
                ),
                Some(ExitCode::FAILURE),
            ));
        }

        control.check()?;
        let signature = self
            .signer
            .sign(
                &nar::fingerprint(&source.path, &nar_hash, nar_size, &references),
                control,
            )
            .map_err(|error| classify_adapter(FailureClass::Trust, "sign cache object", error))?;
        control.check()?;
        let nar_key = format!("nar/{}.nar", nar_hash.trim_start_matches("sha256:"));
        let narinfo = NarInfo::new(crate::nar::NarInfoInput {
            store_path: source.path.clone(),
            url: nar_key.clone(),
            nar_hash,
            nar_size,
            references,
            deriver: info.deriver.clone(),
            signature,
        })
        .map_err(|error| {
            PublicationError::new(
                FailureClass::Trust,
                format!("build signed cache metadata: {error}"),
                Some(ExitCode::PREFLIGHT),
            )
        })?;

        control.check()?;
        self.store
            .put(&nar_key, &archive.bytes, NAR_CONTENT_TYPE, control)
            .map_err(|error| {
                classify_adapter(FailureClass::Write, "upload cache archive", error)
            })?;
        control.check()?;
        let metadata = narinfo.to_string();
        let committed = control.cancellation.commit_if_not_cancelled(|| {
            self.store.put(
                &narinfo_key,
                metadata.as_bytes(),
                NARINFO_CONTENT_TYPE,
                control,
            )
        });
        match committed {
            Some(Ok(())) => Ok(PublicationReceipt::Uploaded),
            Some(Err(error)) => Err(classify_adapter(
                FailureClass::Write,
                "upload cache metadata",
                error,
            )),
            None => Err(cancelled(control.cancellation.signal().unwrap_or_default())),
        }
    }

    fn serialize(
        &self,
        source: &PublicationSource,
        control: &PublicationControl<'_>,
    ) -> Result<Archive, PublicationError> {
        let mut body = ControlledBuffer::new(self.max_nar_bytes, control);
        let mut hashing = HashingWriter::new(&mut body);
        let serialized = nar::write_nar(Path::new(&source.path), &mut hashing);
        let (hash, size) = hashing.finish();
        if let Err(error) = serialized {
            if let Some(control_error) = body.failure.take() {
                return Err(control_error);
            }
            if body.overflowed {
                return Err(PublicationError::new(
                    FailureClass::Precondition,
                    format!(
                        "{} exceeds the {} byte per-path publication limit",
                        source.path, self.max_nar_bytes
                    ),
                    Some(ExitCode::PREFLIGHT),
                ));
            }
            return Err(PublicationError::new(
                FailureClass::Read,
                format!("serialize {} as a Nix archive: {error}", source.path),
                Some(ExitCode::IO),
            ));
        }
        Ok(Archive {
            bytes: body.bytes,
            hash,
            size,
        })
    }
}

struct Archive {
    bytes: Vec<u8>,
    hash: String,
    size: u64,
}

struct ControlledBuffer<'a> {
    bytes: Vec<u8>,
    limit: u64,
    overflowed: bool,
    failure: Option<PublicationError>,
    control: &'a PublicationControl<'a>,
}

impl<'a> ControlledBuffer<'a> {
    const fn new(limit: u64, control: &'a PublicationControl<'a>) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            overflowed: false,
            failure: None,
            control,
        }
    }
}

impl io::Write for ControlledBuffer<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if let Err(error) = self.control.check() {
            self.failure = Some(error);
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "publication control interrupted serialization",
            ));
        }
        if self.bytes.len().saturating_add(buffer.len()) as u64 > self.limit {
            self.overflowed = true;
            return Err(io::Error::other(
                "archive exceeds the publication size limit",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn repeat_failure(
    request: &BatchPublicationRequest,
    error: &PublicationError,
) -> BatchPublicationResult {
    BatchPublicationResult {
        paths: request
            .sources
            .iter()
            .map(|source| PathPublicationResult {
                path: source.path.clone(),
                duration: Duration::ZERO,
                result: Err(error.clone()),
            })
            .collect(),
    }
}

fn batch_timeout(budget: Duration) -> PublicationError {
    PublicationError::new(
        FailureClass::Timeout,
        format!(
            "cache publication exceeded its {} ms whole-batch budget",
            budget.as_millis()
        ),
        None,
    )
}

fn source_timeout(budget: Duration) -> PublicationError {
    PublicationError::new(
        FailureClass::Timeout,
        format!(
            "cache publication exceeded its {} ms per-path budget",
            budget.as_millis()
        ),
        None,
    )
}

fn cancelled(signal: i32) -> PublicationError {
    PublicationError::new(
        FailureClass::Cancelled,
        format!("cache publication cancelled by signal {signal}"),
        Some(ExitCode::from_signal(signal)),
    )
}

fn halted(halt: &Mutex<Option<PublicationError>>) -> Option<PublicationError> {
    halt.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

const fn stops_queue(class: FailureClass) -> bool {
    matches!(
        class,
        FailureClass::Trust
            | FailureClass::Integrity
            | FailureClass::Provenance
            | FailureClass::Precondition
            | FailureClass::Cancelled
    )
}

fn classify_adapter(class: FailureClass, action: &str, error: AdapterError) -> PublicationError {
    match error {
        AdapterError::Backend(error) => PublicationError::new(
            class,
            format!("{action}: {}", error.message),
            Some(error.exit_code),
        ),
        AdapterError::Control(error) => error,
    }
}

fn store_hash_part(store_path: &str) -> Result<&str, PublicationError> {
    let hash = store_path
        .rsplit_once('/')
        .map(|(_, name)| name)
        .and_then(|name| name.split_once('-'))
        .filter(|(_, name)| !name.is_empty())
        .map(|(hash, _)| hash)
        .filter(|hash| hash.len() == 32 && hash.bytes().all(|byte| NIX_BASE32.contains(&byte)));
    hash.ok_or_else(|| {
        PublicationError::new(
            FailureClass::Provenance,
            format!("{store_path} is not a store path with a canonical 32-character hash part"),
            Some(ExitCode::PREFLIGHT),
        )
    })
}

fn validate_store_metadata(
    references: &[String],
    deriver: Option<&str>,
) -> Result<(), PublicationError> {
    for value in references.iter().map(String::as_str).chain(deriver) {
        if !nar::is_canonical_nix_store_path(value) {
            return Err(PublicationError::new(
                FailureClass::Integrity,
                "local store metadata contains a noncanonical /nix/store path",
                Some(ExitCode::FAILURE),
            ));
        }
    }
    Ok(())
}
