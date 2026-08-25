use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use crate::{
    Error, Result,
    resolution::{is_path_tarball_spec, split_package_spec},
};

mod wyhash;

/// Computes the directory name Bun uses for a package in its install cache.
///
/// `resolution` may be a raw first tuple element from `bun.lock` or the legacy
/// synthetic `tarball:`, `github:`, and `git:` identifiers emitted by bun2nix.
///
/// # Errors
///
/// Returns [`Error::InvalidCacheEntryName`] when the resolution or registry
/// would place the entry outside the cache directory.
pub fn cache_entry_name(resolution: &str, registry: Option<&str>) -> Result<String> {
    let name = if let Some(url) = resolution.strip_prefix("tarball:") {
        tarball_name(url)
    } else if let Some(reference) = resolution.strip_prefix("github:") {
        format!("@GH@{reference}@@@1")
    } else if let Some(revision) = resolution.strip_prefix("git:") {
        format!("@G@{revision}")
    } else if let Some((_, spec)) = split_package_spec(resolution) {
        if spec.starts_with("http://") || spec.starts_with("https://") || is_path_tarball_spec(spec)
        {
            tarball_name(spec)
        } else if let Some(reference) = spec.strip_prefix("github:") {
            let normalized = reference.replace(['/', '#'], "-");
            format!("@GH@{normalized}@@@1")
        } else if let Some(reference) = spec.strip_prefix("git+") {
            let (_, revision) = reference
                .rsplit_once('#')
                .ok_or_else(|| Error::InvalidCacheEntryName(resolution.to_owned()))?;
            format!("@G@{revision}")
        } else {
            npm_name(resolution, registry)
        }
    } else {
        npm_name(resolution, registry)
    };

    validate_cache_name(&name)?;
    Ok(name)
}

/// Creates an absolute directory symlink at the package's Bun cache location.
///
/// The returned path is the cache entry itself. Existing entries are never
/// replaced, which makes duplicate or conflicting manifests fail loudly.
///
/// # Errors
///
/// Returns an error when `package` is not a directory, the cache name is
/// unsafe, or the directory and symlink operations fail.
pub fn create_cache_entry(
    out: &Path,
    resolution: &str,
    package: &Path,
    registry: Option<&str>,
) -> Result<PathBuf> {
    if !package.is_dir() {
        return Err(Error::PackagePathNotDirectory(package.to_path_buf()));
    }
    let package = fs::canonicalize(package)?;
    let entry = out.join(cache_entry_name(resolution, registry)?);
    let parent = entry
        .parent()
        .ok_or_else(|| Error::InvalidCacheEntryName(entry.display().to_string()))?;
    fs::create_dir_all(parent)?;
    create_directory_symlink(&package, &entry)?;
    Ok(entry)
}

fn npm_name(package: &str, registry: Option<&str>) -> String {
    let suffix = registry.map_or_else(|| "@@@1".to_owned(), |registry| format!("@@{registry}@@@1"));
    let Some((name, _)) = split_package_spec(package) else {
        return format!("{package}{suffix}");
    };
    let version = &package[name.len()..];

    if let Some(prerelease_start) = version.find('-') {
        let base = &version[..prerelease_start];
        let prerelease_and_build = &version[prerelease_start + 1..];
        if let Some(build_start) = prerelease_and_build.find('+') {
            let prerelease = &prerelease_and_build[..build_start];
            let build = &prerelease_and_build[build_start + 1..];
            return format!(
                "{name}{base}-{:016x}+{:016X}{suffix}",
                wyhash::hash(prerelease.as_bytes()),
                wyhash::hash(build.as_bytes())
            );
        }
        return format!(
            "{name}{base}-{:016x}{suffix}",
            wyhash::hash(prerelease_and_build.as_bytes())
        );
    }

    if let Some(build_start) = version.find('+') {
        let base = &version[..build_start];
        let build = &version[build_start + 1..];
        return format!(
            "{name}{base}+{:016X}{suffix}",
            wyhash::hash(build.as_bytes())
        );
    }

    format!("{package}{suffix}")
}

fn tarball_name(url: &str) -> String {
    format!("@T@{:016x}@@@1", wyhash::hash(url.as_bytes()))
}

fn validate_cache_name(name: &str) -> Result<()> {
    if name.is_empty()
        || Path::new(name)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::InvalidCacheEntryName(name.to_owned()));
    }
    Ok(())
}

#[cfg(unix)]
fn create_directory_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_directory_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}
