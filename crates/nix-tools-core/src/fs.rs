//! Bounded no-follow reads and descriptor-bound atomic file publication.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions as CapOpenOptions};
#[cfg(unix)]
use cap_std::fs::{
    OpenOptionsExt as CapOpenOptionsExt, Permissions as CapPermissions,
    PermissionsExt as CapPermissionsExt,
};

use crate::outcome::{Error, Result};
use crate::process::Cancellation;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const TEMPORARY_INFIX: &str = ".nix-tools-tmp-";

/// Post-commit durability advisories from an atomic publication.
///
/// An `Ok(Publication)` means the new file is already authoritative. Advisories report failures
/// after that commit point, such as inability to synchronize the parent directory.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Publication {
    /// Non-fatal durability gaps observed after publication became visible.
    pub advisories: Vec<String>,
}

/// Result of a cancellation-aware atomic publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GuardedPublication {
    /// The new contents became authoritative.
    Published(Publication),
    /// Cancellation won before the atomic rename; the destination was not changed.
    Aborted,
}

/// Filesystem boundary used by repository-specific applications.
pub trait FileSystem: Send + Sync {
    /// Reads a file without a size bound.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the path cannot be read in full.
    fn read(&self, path: &Path) -> Result<Vec<u8>>;

    /// Reads no more than `limit` bytes from a file.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the file cannot be read or exceeds `limit`.
    fn read_bounded(&self, path: &Path, limit: usize) -> Result<Vec<u8>>;

    /// Reads a regular file beneath `root` without following any relative path symlink.
    ///
    /// # Errors
    ///
    /// Returns a preflight error for escaping paths, symlinks, or non-files, and an I/O error for
    /// read failures or content exceeding `limit`.
    fn read_bounded_nofollow(&self, root: &Path, relative: &Path, limit: usize) -> Result<Vec<u8>>;

    /// Checks path existence without following a final symlink.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when metadata cannot be inspected for a reason other than absence.
    fn exists(&self, path: &Path) -> Result<bool>;

    /// Atomically replaces one file using a temporary in the same opened parent directory.
    ///
    /// # Errors
    ///
    /// Returns an error only before the replacement becomes visible. Post-commit sync failures are
    /// returned as [`Publication::advisories`].
    fn write_atomic(&self, path: &Path, contents: &[u8], mode: u32) -> Result<Publication>;

    /// Atomically replaces one file only if cancellation has not won the commit gate.
    ///
    /// # Errors
    ///
    /// Returns an error only before the replacement becomes visible. Cancellation before the
    /// rename returns [`GuardedPublication::Aborted`].
    fn write_atomic_guarded(
        &self,
        path: &Path,
        contents: &[u8],
        mode: u32,
        cancellation: &Cancellation,
    ) -> Result<GuardedPublication>;
}

/// Standard capability-based filesystem implementation.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdFileSystem;

impl FileSystem for StdFileSystem {
    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        fs::read(path).map_err(|error| Error::io(format!("read {}: {error}", path.display())))
    }

    fn read_bounded(&self, path: &Path, limit: usize) -> Result<Vec<u8>> {
        let file = File::open(path)
            .map_err(|error| Error::io(format!("open {}: {error}", path.display())))?;
        read_bounded_from(file, limit, path)
    }

    fn read_bounded_nofollow(&self, root: &Path, relative: &Path, limit: usize) -> Result<Vec<u8>> {
        let parts = relative
            .components()
            .map(|component| match component {
                Component::Normal(part) => Ok(part.to_owned()),
                _ => Err(Error::preflight(format!(
                    "path must remain beneath {}",
                    root.display()
                ))),
            })
            .collect::<Result<Vec<_>>>()?;
        let (name, parents) = parts
            .split_last()
            .ok_or_else(|| Error::preflight("path must name a file"))?;
        let mut directory = Dir::open_ambient_dir(root, ambient_authority())
            .map_err(|error| Error::io(format!("open root {}: {error}", root.display())))?;
        for parent in parents {
            directory = directory.open_dir_nofollow(parent).map_err(|error| {
                Error::preflight(format!(
                    "parent of {} must be a real directory beneath the root: {error}",
                    relative.display()
                ))
            })?;
        }
        let mut options = CapOpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = directory.open_with(name, &options).map_err(|error| {
            Error::preflight(format!(
                "{} must be a regular file and not a symlink: {error}",
                relative.display()
            ))
        })?;
        let metadata = file
            .metadata()
            .map_err(|error| Error::io(format!("inspect {}: {error}", relative.display())))?;
        if !metadata.is_file() {
            return Err(Error::preflight(format!(
                "{} is not a regular file",
                relative.display()
            )));
        }
        read_bounded_from(file, limit, relative)
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(Error::io(format!("inspect {}: {error}", path.display()))),
        }
    }

    fn write_atomic(&self, path: &Path, contents: &[u8], mode: u32) -> Result<Publication> {
        match atomic_write_guarded_with(
            path,
            contents,
            mode,
            sync_directory,
            &Cancellation::default(),
            || {},
        )
        .map_err(|error| Error::io(format!("atomically write {}: {error}", path.display())))?
        {
            GuardedPublication::Published(publication) => Ok(publication),
            GuardedPublication::Aborted => {
                unreachable!("a fresh cancellation token cannot abort publication")
            }
        }
    }

    fn write_atomic_guarded(
        &self,
        path: &Path,
        contents: &[u8],
        mode: u32,
        cancellation: &Cancellation,
    ) -> Result<GuardedPublication> {
        atomic_write_guarded_with(path, contents, mode, sync_directory, cancellation, || {})
            .map_err(|error| Error::io(format!("atomically write {}: {error}", path.display())))
    }
}

fn atomic_write_guarded_with(
    path: &Path,
    contents: &[u8],
    mode: u32,
    sync_parent: impl FnOnce(&Dir) -> io::Result<()>,
    cancellation: &Cancellation,
    before_rename: impl FnOnce(),
) -> io::Result<GuardedPublication> {
    if cancellation.signal().is_some() {
        return Ok(GuardedPublication::Aborted);
    }
    let parent_path = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "output path has no parent directory",
        )
    })?;
    let parent_path = if parent_path.is_absolute() {
        parent_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(parent_path)
    };
    let parent = open_absolute_dir_nofollow(&parent_path)?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "output path has no file name")
    })?;
    let (temporary, mut file) = create_temporary_at(&parent, name, mode)?;
    let prepared = (|| {
        file.write_all(contents)?;
        #[cfg(unix)]
        file.set_permissions(CapPermissions::from_mode(mode))?;
        file.sync_all()?;
        Ok::<(), io::Error>(())
    })();
    drop(file);
    if let Err(error) = prepared {
        let _ = remove_at_if_exists(&parent, &temporary);
        return Err(error);
    }
    let Some(rename_result) = cancellation.commit_if_not_cancelled(|| {
        before_rename();
        let result = parent.rename(&temporary, &parent, name);
        if result.is_err() {
            let _ = remove_at_if_exists(&parent, &temporary);
        }
        result
    }) else {
        let _ = remove_at_if_exists(&parent, &temporary);
        return Ok(GuardedPublication::Aborted);
    };
    rename_result?;
    let advisories = sync_parent(&parent).err().map_or_else(Vec::new, |error| {
        vec![format!(
            "published {} but could not sync parent directory: {error}",
            path.display()
        )]
    });
    Ok(GuardedPublication::Published(Publication { advisories }))
}

fn open_absolute_dir_nofollow(path: &Path) -> io::Result<Dir> {
    let mut directory = Dir::open_ambient_dir(Path::new("/"), ambient_authority())?;
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => directory = directory.open_dir_nofollow(part)?,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "parent must be an absolute normalized path without symlinks",
                ));
            }
        }
    }
    Ok(directory)
}

fn create_temporary_at(
    parent: &Dir,
    file_name: &OsStr,
    mode: u32,
) -> io::Result<(OsString, cap_std::fs::File)> {
    for _ in 0..100 {
        let temporary = unique_temporary_name(file_name);
        let mut options = CapOpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        #[cfg(unix)]
        options.mode(mode);
        match parent.open_with(&temporary, &options) {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary file",
    ))
}

fn unique_temporary_name(file_name: &OsStr) -> OsString {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(
        ".{}{TEMPORARY_INFIX}{}-{sequence}",
        file_name.to_string_lossy(),
        std::process::id()
    )
    .into()
}

fn remove_at_if_exists(parent: &Dir, name: &OsStr) -> io::Result<()> {
    match parent.remove_file(name) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn read_bounded_from(source: impl Read, limit: usize, subject: &Path) -> Result<Vec<u8>> {
    let mut contents = Vec::with_capacity(limit.min(64 * 1024));
    let read_limit = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    source
        .take(read_limit)
        .read_to_end(&mut contents)
        .map_err(|error| Error::io(format!("read {}: {error}", subject.display())))?;
    if contents.len() > limit {
        return Err(Error::io(format!(
            "read {}: exceeds {limit} bytes",
            subject.display()
        )));
    }
    Ok(contents)
}

fn sync_directory(parent: &Dir) -> io::Result<()> {
    parent.open(".")?.sync_all()
}

#[cfg(test)]
#[path = "fs_test.rs"]
mod fs_test;
