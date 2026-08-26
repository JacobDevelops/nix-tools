use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
};

use serde::Deserialize;
use serde_json::Value;

use crate::{
    Error, Result,
    resolution::{split_package_spec, workspace_path},
};

type DependencyMap = BTreeMap<String, String>;
type PackageMap = BTreeMap<String, Vec<Value>>;

/// Parsed textual Bun lockfile.
#[derive(Clone, Debug)]
pub struct Lockfile {
    version: u8,
    pub(crate) workspaces: BTreeMap<String, Workspace>,
    pub(crate) packages: PackageMap,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
struct RawLockfile {
    lockfile_version: u64,
    #[serde(default, rename = "configVersion")]
    _config_version: Option<u64>,
    #[serde(default)]
    workspaces: BTreeMap<String, Workspace>,
    #[serde(default, rename = "trustedDependencies")]
    _trusted_dependencies: Vec<String>,
    #[serde(default)]
    patched_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "overrides")]
    _overrides: BTreeMap<String, Value>,
    #[serde(default, rename = "catalog")]
    _catalog: BTreeMap<String, String>,
    #[serde(default, rename = "catalogs")]
    _catalogs: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    packages: PackageMap,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct PackageInfo {
    dependencies: DependencyMap,
    dev_dependencies: DependencyMap,
    optional_dependencies: DependencyMap,
    peer_dependencies: DependencyMap,
    optional_peers: BTreeSet<String>,
    os: Option<StringOrList>,
    cpu: Option<StringOrList>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum StringOrList {
    String(String),
    List(Vec<String>),
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Workspace {
    name: Option<String>,
    dependencies: DependencyMap,
    dev_dependencies: DependencyMap,
    optional_dependencies: DependencyMap,
    peer_dependencies: DependencyMap,
    optional_peers: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct Dependency {
    name: String,
    optional: bool,
}

impl Lockfile {
    /// Parses a JSONC `bun.lock` and validates its textual lockfile version.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid JSONC, an absent JSON value, a schema
    /// mismatch, or a lockfile version newer than 3.
    pub fn parse(contents: &str) -> Result<Self> {
        let value =
            jsonc_parser::parse_to_serde_value(contents, &jsonc_parser::ParseOptions::default())?
                .ok_or(Error::EmptyLockfile)?;
        let raw: RawLockfile = serde_json::from_value(value)?;
        if !raw.patched_dependencies.is_empty() {
            return Err(Error::UnsupportedSemantics("patchedDependencies"));
        }
        let Ok(version) = u8::try_from(raw.lockfile_version) else {
            return Err(Error::UnsupportedLockfileVersion(raw.lockfile_version));
        };
        if version > 3 {
            return Err(Error::UnsupportedLockfileVersion(u64::from(version)));
        }
        if raw.packages.values().any(|entry| {
            entry
                .first()
                .and_then(Value::as_str)
                .is_some_and(|resolution| resolution.contains("@link:"))
        }) {
            return Err(Error::UnsupportedSemantics("link dependencies"));
        }

        Ok(Self {
            version,
            workspaces: raw.workspaces,
            packages: raw.packages,
        })
    }

    /// Returns the Bun textual lockfile version.
    #[must_use]
    pub const fn version(&self) -> u8 {
        self.version
    }

    /// Computes each non-root named workspace's sorted transitive resolution set.
    ///
    /// Local sources are traversed because they can declare registry dependencies,
    /// but their own resolutions are omitted because Bun reads them from the source
    /// workspace rather than its global install cache.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate workspace names, missing required graph
    /// nodes, or malformed package tuples and metadata.
    pub fn dependency_closures(&self) -> Result<BTreeMap<String, Vec<String>>> {
        self.check_dependency_closures()
    }

    /// Computes runtime-only dependency closures for each named workspace.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid dependency graph.
    pub fn production_dependency_closures(&self) -> Result<BTreeMap<String, Vec<String>>> {
        self.closures(ClosureKind::Production)
    }

    /// Computes dependency closures used to build and test each named workspace.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid dependency graph.
    pub fn check_dependency_closures(&self) -> Result<BTreeMap<String, Vec<String>>> {
        self.closures(ClosureKind::Check)
    }

    /// Computes interactive-development closures, including root tooling.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid dependency graph.
    pub fn development_dependency_closures(&self) -> Result<BTreeMap<String, Vec<String>>> {
        self.closures(ClosureKind::Development)
    }

    fn closures(&self, kind: ClosureKind) -> Result<BTreeMap<String, Vec<String>>> {
        let mut workspace_paths_by_name = BTreeMap::new();
        for (path, workspace) in &self.workspaces {
            let Some(name) = &workspace.name else {
                continue;
            };
            if workspace_paths_by_name
                .insert(name.clone(), path.clone())
                .is_some()
            {
                return Err(Error::DuplicateWorkspaceName(name.clone()));
            }
        }

        let mut closures = BTreeMap::new();
        for (name, path) in &workspace_paths_by_name {
            let mut graph = WorkspaceGraph::new(self, &workspace_paths_by_name);
            graph.visit_workspace(path, kind != ClosureKind::Production)?;
            if kind == ClosureKind::Development && self.workspaces.contains_key("") {
                graph.visit_workspace("", true)?;
            }
            closures.insert(name.clone(), graph.selected.into_iter().collect());
        }
        Ok(closures)
    }

    pub(crate) fn resolve_path_tarball(&self, key: &str, resolution: &str) -> Result<String> {
        let (package_name, spec) =
            split_package_spec(resolution).ok_or_else(|| Error::InvalidPackage {
                key: key.to_owned(),
                reason: format!("local tarball resolution {resolution} has no package specifier"),
            })?;
        let mut declaring_workspaces = self
            .workspaces
            .iter()
            .filter(|(_, workspace)| workspace.dependency_spec(package_name) == Some(spec))
            .map(|(path, _)| path.clone())
            .collect::<BTreeSet<_>>();
        let suffix = format!("/{package_name}");
        if let Some(parent) = key.strip_suffix(&suffix)
            && let Some((path, _)) = self
                .workspaces
                .iter()
                .find(|(_, workspace)| workspace.name.as_deref() == Some(parent))
        {
            declaring_workspaces.insert(path.clone());
        }
        if declaring_workspaces.is_empty() {
            return Err(Error::InvalidPackage {
                key: key.to_owned(),
                reason: format!("local tarball resolution {resolution} has no declaring workspace"),
            });
        }

        let resolved = declaring_workspaces
            .iter()
            .map(|workspace| resolve_repo_path(workspace, spec))
            .collect::<std::result::Result<BTreeSet<_>, _>>()
            .map_err(|reason| Error::InvalidPackage {
                key: key.to_owned(),
                reason,
            })?;
        if resolved.len() != 1 {
            return Err(Error::InvalidPackage {
                key: key.to_owned(),
                reason: format!(
                    "local tarball resolution {resolution} is ambiguous across workspaces"
                ),
            });
        }
        Ok(resolved.into_iter().next().expect("one resolved path"))
    }
}

impl PackageInfo {
    fn dependency_entries(&self) -> Vec<Dependency> {
        dependencies(
            &self.dependencies,
            &self.optional_dependencies,
            &self.peer_dependencies,
            &self.optional_peers,
        )
    }

    pub(crate) fn os(&self) -> Option<Vec<String>> {
        self.os.as_ref().map(StringOrList::sorted_values)
    }

    pub(crate) fn cpu(&self) -> Option<Vec<String>> {
        self.cpu.as_ref().map(StringOrList::sorted_values)
    }
}

impl StringOrList {
    fn sorted_values(&self) -> Vec<String> {
        let mut values = match self {
            Self::String(value) => vec![value.clone()],
            Self::List(values) => values.clone(),
        };
        values.sort();
        values.dedup();
        values
    }
}

impl Workspace {
    fn dependency_entries(&self, include_development: bool) -> Vec<Dependency> {
        let mut entries = dependencies(
            &self.dependencies,
            &self.optional_dependencies,
            &self.peer_dependencies,
            &self.optional_peers,
        );
        if include_development {
            entries.extend(self.dev_dependencies.keys().map(|name| Dependency {
                name: name.clone(),
                optional: false,
            }));
        }
        entries
    }

    fn dependency_spec(&self, name: &str) -> Option<&str> {
        self.dependencies
            .get(name)
            .or_else(|| self.dev_dependencies.get(name))
            .or_else(|| self.optional_dependencies.get(name))
            .or_else(|| self.peer_dependencies.get(name))
            .map(String::as_str)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClosureKind {
    Production,
    Check,
    Development,
}

fn resolve_repo_path(workspace: &str, spec: &str) -> std::result::Result<String, String> {
    if workspace.contains('\\') || spec.contains('\\') {
        return Err(format!(
            "local tarball path {spec} is not a portable repository path"
        ));
    }
    let mut segments = Vec::new();
    for component in Path::new(workspace)
        .components()
        .chain(Path::new(spec).components())
    {
        match component {
            Component::CurDir => {}
            Component::Normal(segment) => segments.push(
                segment
                    .to_str()
                    .ok_or_else(|| format!("local tarball path {spec} is not UTF-8"))?,
            ),
            Component::ParentDir if segments.pop().is_some() => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("local tarball path {spec} escapes the repository"));
            }
        }
    }
    if segments.is_empty() {
        return Err(format!("local tarball path {spec} does not name a file"));
    }
    Ok(segments.join("/"))
}

fn dependencies(
    required: &DependencyMap,
    optional: &DependencyMap,
    peers: &DependencyMap,
    optional_peers: &BTreeSet<String>,
) -> Vec<Dependency> {
    required
        .keys()
        .map(|name| Dependency {
            name: name.clone(),
            optional: false,
        })
        .chain(optional.keys().map(|name| Dependency {
            name: name.clone(),
            optional: true,
        }))
        .chain(peers.keys().map(|name| Dependency {
            name: name.clone(),
            optional: optional_peers.contains(name),
        }))
        .collect()
}

pub(crate) fn package_info(entry: &[Value]) -> Result<PackageInfo> {
    entry
        .get(2)
        .filter(|value| value.is_object())
        .or_else(|| entry.get(1).filter(|value| value.is_object()))
        .map_or_else(
            || Ok(PackageInfo::default()),
            |value| serde_json::from_value(value.clone()).map_err(Error::from),
        )
}

pub(crate) fn package_resolution<'a>(key: &str, entry: &'a [Value]) -> Result<&'a str> {
    entry
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| Error::MissingPackageResolution {
            key: key.to_owned(),
        })
}

pub(crate) fn is_local_resolution(resolution: &str) -> bool {
    ["@workspace:", "@file:", "@folder:", "@link:", "@root:"]
        .iter()
        .any(|marker| resolution.contains(marker))
}

struct WorkspaceGraph<'a> {
    lockfile: &'a Lockfile,
    workspace_paths_by_name: &'a BTreeMap<String, String>,
    selected: BTreeSet<String>,
    visited_packages: BTreeSet<String>,
    visited_workspaces: BTreeSet<String>,
    nested_keys_by_dependency: BTreeMap<String, Vec<String>>,
}

impl<'a> WorkspaceGraph<'a> {
    fn new(lockfile: &'a Lockfile, workspace_paths_by_name: &'a BTreeMap<String, String>) -> Self {
        Self {
            lockfile,
            workspace_paths_by_name,
            selected: BTreeSet::new(),
            visited_packages: BTreeSet::new(),
            visited_workspaces: BTreeSet::new(),
            nested_keys_by_dependency: BTreeMap::new(),
        }
    }

    fn visit_workspace(&mut self, path: &str, include_development: bool) -> Result<()> {
        if !self.visited_workspaces.insert(path.to_owned()) {
            return Ok(());
        }
        let workspace = self
            .lockfile
            .workspaces
            .get(path)
            .ok_or_else(|| Error::MissingWorkspace(path.to_owned()))?
            .clone();
        let context = workspace.name.clone().unwrap_or_else(|| path.to_owned());
        if self.lockfile.packages.contains_key(&context) {
            self.visited_packages.insert(context.clone());
        }
        for dependency in workspace.dependency_entries(include_development) {
            self.visit_dependency(&context, dependency)?;
        }
        Ok(())
    }

    fn visit_dependency(&mut self, context: &str, dependency: Dependency) -> Result<()> {
        let workspace = self.workspace_paths_by_name.get(&dependency.name).cloned();
        let Some(key) = self.resolve_key(context, &dependency.name) else {
            if let Some(path) = workspace {
                return self.visit_workspace(&path, false);
            }
            if dependency.optional {
                return Ok(());
            }
            return Err(Error::MissingDependency {
                context: context.to_owned(),
                dependency: dependency.name,
            });
        };
        let entry = &self.lockfile.packages[&key];
        let resolution = package_resolution(&key, entry)?;
        if let Some(path) = workspace_path(resolution) {
            return self.visit_workspace(path, false);
        }
        if !self.visited_packages.insert(key.clone()) {
            return Ok(());
        }

        let resolution = resolution.to_owned();
        let info = package_info(entry)?;
        if !is_local_resolution(&resolution) {
            self.selected.insert(resolution);
        }
        for child in info.dependency_entries() {
            self.visit_dependency(&key, child)?;
        }
        Ok(())
    }

    fn resolve_key(&mut self, context: &str, dependency: &str) -> Option<String> {
        let candidates = self
            .nested_keys_by_dependency
            .entry(dependency.to_owned())
            .or_insert_with(|| {
                let suffix = format!("/{dependency}");
                self.lockfile
                    .packages
                    .keys()
                    .filter(|key| key.ends_with(&suffix))
                    .cloned()
                    .collect()
            });

        let mut nested = candidates
            .iter()
            .filter(|key| key.as_str() != context)
            .filter_map(|key| {
                let parent_length = key.len().checked_sub(dependency.len() + 1)?;
                let parent = &key[..parent_length];
                (self.lockfile.packages.contains_key(parent)
                    || self.workspace_paths_by_name.contains_key(parent))
                .then_some((key, parent))
            })
            .filter(|(_, parent)| {
                context == *parent
                    || context
                        .strip_prefix(*parent)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            })
            .collect::<Vec<_>>();
        nested.sort_by(|(left_key, left_parent), (right_key, right_parent)| {
            right_parent
                .len()
                .cmp(&left_parent.len())
                .then_with(|| left_key.cmp(right_key))
        });

        nested.first().map(|(key, _)| (*key).clone()).or_else(|| {
            self.lockfile
                .packages
                .contains_key(dependency)
                .then(|| dependency.to_owned())
        })
    }
}
