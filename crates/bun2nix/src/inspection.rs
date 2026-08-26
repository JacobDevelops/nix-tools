use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::{
    Error, Lockfile, Result,
    lockfile::{is_local_resolution, package_info, package_resolution},
};

/// Deterministic, machine-readable dependency plan derived from `bun.lock`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LockfileInspection {
    /// Bun textual lockfile version.
    pub lockfile_version: u8,
    /// Runtime-only transitive resolutions for every named workspace.
    pub production_dependency_closures: BTreeMap<String, Vec<String>>,
    /// Build-and-test transitive resolutions for every named workspace.
    pub check_dependency_closures: BTreeMap<String, Vec<String>>,
    /// Interactive-development resolutions, including root tooling.
    pub development_dependency_closures: BTreeMap<String, Vec<String>>,
    /// Lockfile-backed operating-system and CPU constraints by raw resolution.
    pub platform_constraints: BTreeMap<String, PlatformConstraints>,
    /// Every raw local workspace/file/folder/link/root resolution.
    pub workspace_packages: Vec<String>,
}

/// Platform restrictions attached to a Bun package tuple.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlatformConstraints {
    /// Allowed or negated npm operating-system identifiers, or unrestricted.
    pub os: Option<Vec<String>>,
    /// Allowed or negated npm CPU identifiers, or unrestricted.
    pub cpu: Option<Vec<String>>,
}

/// Parses and inspects a Bun lockfile without fetching any external sources.
///
/// # Errors
///
/// Returns an error when the lockfile, dependency graph, or package metadata is
/// invalid or internally inconsistent.
pub fn inspect_lockfile(contents: &str) -> Result<LockfileInspection> {
    inspect(&Lockfile::parse(contents)?)
}

pub(crate) fn inspect(lockfile: &Lockfile) -> Result<LockfileInspection> {
    let production_dependency_closures = lockfile.production_dependency_closures()?;
    let check_dependency_closures = lockfile.check_dependency_closures()?;
    let development_dependency_closures = lockfile.development_dependency_closures()?;

    let mut platform_constraints = BTreeMap::new();
    let mut workspace_packages = BTreeSet::new();
    for (key, entry) in &lockfile.packages {
        let resolution = package_resolution(key, entry)?;
        let info = package_info(entry)?;
        let constraints = PlatformConstraints {
            os: info.os(),
            cpu: info.cpu(),
        };
        if let Some(existing) =
            platform_constraints.insert(resolution.to_owned(), constraints.clone())
            && existing != constraints
        {
            return Err(Error::ConflictingResolution(resolution.to_owned()));
        }
        if is_local_resolution(resolution) {
            workspace_packages.insert(resolution.to_owned());
        }
    }

    Ok(LockfileInspection {
        lockfile_version: lockfile.version(),
        production_dependency_closures,
        check_dependency_closures,
        development_dependency_closures,
        platform_constraints,
        workspace_packages: workspace_packages.into_iter().collect(),
    })
}
