//! Canonical Nix archive and binary-cache metadata encoding.
//!
//! The archive encoding follows Nix's `nix-archive-1` format. Compatibility vectors are retained
//! from the source implementation and were originally generated with `nix nar pack` and
//! `nix hash path`.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use sha2::{Digest as _, Sha256};
use thiserror::Error;

const NIX_BASE32: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";
const READ_CHUNK: usize = 256 * 1024;

/// Serializes `path` into `writer` using Nix's canonical `nix-archive-1` representation.
///
/// Symlinks are encoded by their target without being followed, directory entries are ordered by
/// raw name bytes, and only regular files, directories, and symbolic links are representable.
///
/// # Errors
///
/// Returns an I/O error when the tree changes during serialization, cannot be read, cannot be
/// written, or contains an unsupported file type.
pub fn write_nar(path: &Path, writer: &mut dyn Write) -> io::Result<()> {
    write_padded(writer, b"nix-archive-1")?;
    write_node(path, writer)
}

/// A writer that hashes exactly the bytes successfully forwarded to its destination.
pub struct HashingWriter<'a> {
    inner: &'a mut dyn Write,
    digest: Sha256,
    written: u64,
}

impl<'a> HashingWriter<'a> {
    /// Wraps `inner` with a SHA-256 digest and byte counter.
    #[must_use]
    pub fn new(inner: &'a mut dyn Write) -> Self {
        Self {
            inner,
            digest: Sha256::new(),
            written: 0,
        }
    }

    /// Returns the Nix-formatted SHA-256 hash and exact count of forwarded bytes.
    #[must_use]
    pub fn finish(self) -> (String, u64) {
        (
            format!("sha256:{}", nix_base32(&self.digest.finalize())),
            self.written,
        )
    }
}

impl Write for HashingWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.digest.update(&buffer[..written]);
        self.written = self.written.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Encodes bytes using Nix's base32 alphabet and least-significant-bit-first ordering.
#[must_use]
pub fn nix_base32(bytes: &[u8]) -> String {
    let bits = bytes.len() * 8;
    let length = bits / 5 + usize::from(!bits.is_multiple_of(5));
    let mut encoded = String::with_capacity(length);
    for index in (0..length).rev() {
        let bit = index * 5;
        let byte = bit / 8;
        let offset = bit % 8;
        let low = u32::from(bytes[byte]) >> offset;
        let high = bytes
            .get(byte + 1)
            .map_or(0, |next| u32::from(*next) << (8 - offset));
        encoded.push(char::from(NIX_BASE32[((low | high) & 0x1f) as usize]));
    }
    encoded
}

/// Builds the exact string covered by a binary-cache signature.
///
/// Callers must supply references in the canonical order recorded by the store.
#[must_use]
pub fn fingerprint(
    store_path: &str,
    nar_hash: &str,
    nar_size: u64,
    references: &[String],
) -> String {
    format!(
        "1;{store_path};{nar_hash};{nar_size};{}",
        references.join(",")
    )
}

/// A canonical uncompressed Nix binary-cache metadata object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NarInfo {
    store_path: String,
    url: String,
    nar_hash: String,
    nar_size: u64,
    references: Vec<String>,
    deriver: Option<String>,
    signature: String,
}

/// Fields validated when constructing a [`NarInfo`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NarInfoInput {
    /// Store path described by the metadata.
    pub store_path: String,
    /// Relative cache object URL containing the NAR.
    pub url: String,
    /// Canonical `sha256:` NAR hash.
    pub nar_hash: String,
    /// Uncompressed NAR size.
    pub nar_size: u64,
    /// Referenced store paths.
    pub references: Vec<String>,
    /// Deriving store path when known.
    pub deriver: Option<String>,
    /// Key name and Ed25519 signature.
    pub signature: String,
}

impl NarInfo {
    /// Validates and constructs a canonical uncompressed narinfo.
    ///
    /// References are sorted and deduplicated so both the displayed metadata and its signature
    /// input have a deterministic order.
    ///
    /// # Errors
    ///
    /// Returns an error for line injection, an absolute or traversing object URL, malformed
    /// hashes, or malformed store-path fields.
    pub fn new(input: NarInfoInput) -> Result<Self, NarInfoError> {
        let NarInfoInput {
            store_path,
            url,
            nar_hash,
            nar_size,
            mut references,
            deriver,
            signature,
        } = input;
        validate_store_path_field("store_path", &store_path)?;
        validate_relative_url(&url)?;
        validate_nar_hash(&nar_hash)?;
        for reference in &references {
            validate_nix_store_path_field("references", reference)?;
        }
        if let Some(deriver) = deriver.as_deref() {
            validate_nix_store_path_field("deriver", deriver)?;
        }
        validate_single_line("signature", &signature)?;
        if !is_valid_signature(&signature) {
            return Err(NarInfoError::new(
                "signature",
                "must contain a key name and canonical base64 Ed25519 signature",
            ));
        }
        references.sort();
        references.dedup();
        Ok(Self {
            store_path,
            url,
            nar_hash,
            nar_size,
            references,
            deriver,
            signature,
        })
    }

    /// Returns references in the canonical order used by this narinfo.
    #[must_use]
    pub fn references(&self) -> &[String] {
        &self.references
    }
}

impl fmt::Display for NarInfo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "StorePath: {}", self.store_path)?;
        writeln!(formatter, "URL: {}", self.url)?;
        writeln!(formatter, "Compression: none")?;
        writeln!(formatter, "FileHash: {}", self.nar_hash)?;
        writeln!(formatter, "FileSize: {}", self.nar_size)?;
        writeln!(formatter, "NarHash: {}", self.nar_hash)?;
        writeln!(formatter, "NarSize: {}", self.nar_size)?;
        writeln!(
            formatter,
            "References: {}",
            self.references
                .iter()
                .map(|reference| base_name(reference))
                .collect::<Vec<_>>()
                .join(" ")
        )?;
        if let Some(deriver) = self.deriver.as_deref() {
            writeln!(formatter, "Deriver: {}", base_name(deriver))?;
        }
        writeln!(formatter, "Sig: {}", self.signature)
    }
}

/// A validation failure in one narinfo field.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid narinfo {field}: {reason}")]
pub struct NarInfoError {
    field: &'static str,
    reason: &'static str,
}

impl NarInfoError {
    const fn new(field: &'static str, reason: &'static str) -> Self {
        Self { field, reason }
    }

    /// Returns the stable field name that failed validation.
    #[must_use]
    pub const fn field(&self) -> &'static str {
        self.field
    }
}

fn validate_single_line(field: &'static str, value: &str) -> Result<(), NarInfoError> {
    if value.contains(['\r', '\n', '\0']) {
        Err(NarInfoError::new(field, "must be a single text line"))
    } else {
        Ok(())
    }
}

fn validate_store_path_field(field: &'static str, value: &str) -> Result<(), NarInfoError> {
    validate_single_line(field, value)?;
    if !value.starts_with('/')
        || value.ends_with('/')
        || value
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(NarInfoError::new(field, "must be an absolute store path"));
    }
    if value.rsplit('/').next().is_none_or(str::is_empty) {
        return Err(NarInfoError::new(field, "must have a base name"));
    }
    Ok(())
}

fn validate_relative_url(url: &str) -> Result<(), NarInfoError> {
    validate_single_line("url", url)?;
    if url.is_empty()
        || url.starts_with('/')
        || url.contains("://")
        || url
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(NarInfoError::new(
            "url",
            "must be a non-traversing relative object key",
        ));
    }
    Ok(())
}

fn validate_nar_hash(hash: &str) -> Result<(), NarInfoError> {
    let Some(encoded) = hash.strip_prefix("sha256:") else {
        return Err(NarInfoError::new("nar_hash", "must use SHA-256"));
    };
    if encoded.len() != 52
        || !matches!(encoded.as_bytes().first(), Some(b'0' | b'1'))
        || !encoded.bytes().all(|byte| NIX_BASE32.contains(&byte))
    {
        return Err(NarInfoError::new(
            "nar_hash",
            "must contain a canonical Nix base32 SHA-256 digest",
        ));
    }
    Ok(())
}

fn validate_nix_store_path_field(field: &'static str, value: &str) -> Result<(), NarInfoError> {
    validate_store_path_field(field, value)?;
    if !is_canonical_nix_store_path(value) {
        return Err(NarInfoError::new(
            field,
            "must be a canonical /nix/store path",
        ));
    }
    Ok(())
}

pub(crate) fn is_canonical_nix_store_path(value: &str) -> bool {
    let Some(name) = value.strip_prefix("/nix/store/") else {
        return false;
    };
    let Some((hash, name)) = name.split_once('-') else {
        return false;
    };
    hash.len() == 32
        && hash.bytes().all(|byte| NIX_BASE32.contains(&byte))
        && !name.is_empty()
        && name.len() <= 211
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.' | b'_' | b'?' | b'=')
        })
}

fn is_valid_signature(signature: &str) -> bool {
    let Some((name, value)) = signature.split_once(':') else {
        return false;
    };
    let value = value.as_bytes();
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b':')
        && value.len() == 88
        && value[..86]
            .iter()
            .copied()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
        && matches!(value[85], b'A' | b'Q' | b'g' | b'w')
        && value.ends_with(b"==")
}

fn base_name(store_path: &str) -> &str {
    store_path
        .rsplit_once('/')
        .map_or(store_path, |(_, name)| name)
}

fn write_node(path: &Path, writer: &mut dyn Write) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    write_padded(writer, b"(")?;
    write_padded(writer, b"type")?;
    if metadata.is_symlink() {
        write_padded(writer, b"symlink")?;
        write_padded(writer, b"target")?;
        write_padded(writer, fs::read_link(path)?.as_os_str().as_encoded_bytes())?;
    } else if metadata.is_dir() {
        write_padded(writer, b"directory")?;
        write_directory(path, writer)?;
    } else if metadata.is_file() {
        write_padded(writer, b"regular")?;
        if metadata.permissions().mode() & 0o100 != 0 {
            write_padded(writer, b"executable")?;
            write_padded(writer, b"")?;
        }
        write_padded(writer, b"contents")?;
        write_contents(path, metadata.len(), writer)?;
    } else {
        return Err(io::Error::other(format!(
            "{} is not a regular file, directory, or symbolic link",
            path.display()
        )));
    }
    write_padded(writer, b")")
}

fn write_directory(path: &Path, writer: &mut dyn Write) -> io::Result<()> {
    let mut names = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    names.sort_unstable();
    for name in names {
        write_padded(writer, b"entry")?;
        write_padded(writer, b"(")?;
        write_padded(writer, b"name")?;
        write_padded(writer, name.as_encoded_bytes())?;
        write_padded(writer, b"node")?;
        write_node(&path.join(&name), writer)?;
        write_padded(writer, b")")?;
    }
    Ok(())
}

fn write_contents(path: &Path, declared: u64, writer: &mut dyn Write) -> io::Result<()> {
    write_length(writer, declared)?;
    let mut file = File::open(path)?;
    let mut buffer = vec![0_u8; READ_CHUNK];
    let mut copied = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        copied = copied.saturating_add(read as u64);
        if copied > declared {
            return Err(io::Error::other(format!(
                "{} grew while it was being archived",
                path.display()
            )));
        }
        writer.write_all(&buffer[..read])?;
    }
    if copied != declared {
        return Err(io::Error::other(format!(
            "{} shrank while it was being archived",
            path.display()
        )));
    }
    write_padding(writer, declared)
}

fn write_padded(writer: &mut dyn Write, value: &[u8]) -> io::Result<()> {
    write_length(writer, value.len() as u64)?;
    writer.write_all(value)?;
    write_padding(writer, value.len() as u64)
}

fn write_length(writer: &mut dyn Write, length: u64) -> io::Result<()> {
    writer.write_all(&length.to_le_bytes())
}

fn write_padding(writer: &mut dyn Write, length: u64) -> io::Result<()> {
    let padding = usize::try_from((8 - (length % 8)) % 8).unwrap_or_default();
    writer.write_all(&[0_u8; 8][..padding])
}
