#![forbid(unsafe_code)]

//! Policy-free primitives for publishing canonical Nix binary-cache objects.

pub mod nar;
pub mod publish;

pub use nar::{
    HashingWriter, NarInfo, NarInfoError, NarInfoInput, fingerprint, nix_base32, write_nar,
};
pub use publish::{
    AdapterError, AdapterResult, ArchiveCodec, BatchPublicationRequest, BatchPublicationResult,
    BinaryCachePublisher, CacheObjectStore, CacheSigner, EncodedArchive, FailureClass,
    PathPublicationResult, PublicationControl, PublicationError, PublicationReceipt,
    PublicationSource, StorePathIndex, StorePathInfo,
};

#[cfg(test)]
#[path = "nar_test.rs"]
mod nar_test;

#[cfg(test)]
#[path = "publish_test.rs"]
mod publish_test;
