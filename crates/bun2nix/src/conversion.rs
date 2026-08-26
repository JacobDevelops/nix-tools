use std::{
    collections::BTreeMap,
    fmt::Write as _,
    path::{Component, Path},
    process::Command,
};

use serde::Deserialize;

use crate::{
    Error, Lockfile, Result,
    lockfile::{is_local_resolution, package_info, package_resolution},
    resolution::{is_path_tarball_spec, split_package_spec},
};

const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";

#[derive(Deserialize)]
struct PrefetchOutput {
    hash: String,
}

/// Options controlling the generated Nix expression.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConvertOptions {
    /// Relative prefix from the generated `bun.nix` to local package sources.
    pub copy_prefix: String,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            copy_prefix: ".".to_owned(),
        }
    }
}

/// Supplies Nix hashes for direct tarball and source-control dependencies.
pub trait Prefetcher {
    /// Fetches `source` and returns its SRI Nix hash.
    ///
    /// # Errors
    ///
    /// Returns an error when the source cannot be fetched or hashed.
    fn prefetch(&self, source: &str) -> Result<String>;
}

/// Prefetcher backed by `nix flake prefetch`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NixPrefetcher;

impl Prefetcher for NixPrefetcher {
    fn prefetch(&self, source: &str) -> Result<String> {
        let mut command = Command::new("nix");
        command.args(["--extra-experimental-features", "nix-command flakes"]);
        if source.starts_with("http://") || source.starts_with("https://") {
            command.args(["store", "prefetch-file", "--json", "--unpack", source]);
        } else {
            command.args(["flake", "prefetch", source, "--json"]);
        }
        let output = command.output()?;
        if !output.status.success() {
            return Err(Error::PrefetchFailed {
                locator: source.to_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }

        serde_json::from_slice::<PrefetchOutput>(&output.stdout)
            .map(|result| result.hash)
            .map_err(|error| Error::InvalidPrefetchResponse {
                locator: source.to_owned(),
                reason: error.to_string(),
            })
    }
}

/// Converts a JSONC Bun lockfile to a structured `bun.nix` expression.
///
/// Direct tarball and source-control dependencies are prefetched with Nix.
///
/// # Errors
///
/// Returns an error when parsing, graph resolution, package conversion, or an
/// external Nix prefetch fails.
pub fn convert_lockfile(contents: &str, options: &ConvertOptions) -> Result<String> {
    convert_lockfile_with_prefetcher(contents, options, &NixPrefetcher)
}

/// Converts a JSONC Bun lockfile with an injected external-source prefetcher.
///
/// # Errors
///
/// Returns an error when parsing, graph resolution, package conversion, or the
/// supplied prefetcher fails.
pub fn convert_lockfile_with_prefetcher<P: Prefetcher + ?Sized>(
    contents: &str,
    options: &ConvertOptions,
    prefetcher: &P,
) -> Result<String> {
    let lockfile = Lockfile::parse(contents)?;
    let production_closures = lockfile.production_dependency_closures()?;
    let check_closures = lockfile.check_dependency_closures()?;
    let development_closures = lockfile.development_dependency_closures()?;
    let packages = converted_packages(&lockfile, options, prefetcher)?;
    Ok(render(
        lockfile.version(),
        &packages,
        &production_closures,
        &check_closures,
        &development_closures,
    ))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConvertedPackage {
    source: Source,
    metadata: PackageMetadata,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PackageMetadata {
    kind: &'static str,
    local: bool,
    os: Option<Vec<String>>,
    cpu: Option<Vec<String>>,
    registry: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Source {
    FetchUrl {
        url: String,
        hash: String,
        name: Option<String>,
    },
    FetchGit {
        url: String,
        rev: String,
        hash: String,
    },
    FetchGitHub {
        owner: String,
        repo: String,
        rev: String,
        hash: String,
    },
    FetchTarball {
        url: String,
        hash: String,
    },
    CopyToStore {
        path: String,
    },
}

fn converted_packages<P: Prefetcher + ?Sized>(
    lockfile: &Lockfile,
    options: &ConvertOptions,
    prefetcher: &P,
) -> Result<BTreeMap<String, ConvertedPackage>> {
    let mut packages = BTreeMap::new();
    for (key, entry) in &lockfile.packages {
        let resolution = package_resolution(key, entry)?;
        let info = package_info(entry)?;
        let package =
            convert_package(lockfile, key, resolution, entry, options, prefetcher, &info)?;
        if let Some(existing) = packages.insert(resolution.to_owned(), package.clone())
            && existing != package
        {
            return Err(Error::ConflictingResolution(resolution.to_owned()));
        }
    }
    Ok(packages)
}

fn convert_package<P: Prefetcher + ?Sized>(
    lockfile: &Lockfile,
    key: &str,
    resolution: &str,
    entry: &[serde_json::Value],
    options: &ConvertOptions,
    prefetcher: &P,
    info: &crate::lockfile::PackageInfo,
) -> Result<ConvertedPackage> {
    let (source, kind, registry) = if is_local_resolution(resolution) {
        let (kind, path) = local_source(resolution).ok_or_else(|| Error::InvalidPackage {
            key: key.to_owned(),
            reason: format!("invalid local resolution {resolution}"),
        })?;
        if !valid_local_path(kind, path) {
            return Err(Error::InvalidPackage {
                key: key.to_owned(),
                reason: format!("local resolution {resolution} escapes the source root"),
            });
        }
        (
            Source::CopyToStore {
                path: prefixed_path(&options.copy_prefix, path),
            },
            kind,
            None,
        )
    } else {
        convert_remote_source(lockfile, key, resolution, entry, options, prefetcher)?
    };

    Ok(ConvertedPackage {
        source,
        metadata: PackageMetadata {
            kind,
            local: matches!(kind, "workspace" | "file" | "folder" | "link" | "root"),
            os: info.os(),
            cpu: info.cpu(),
            registry,
        },
    })
}

fn convert_remote_source<P: Prefetcher + ?Sized>(
    lockfile: &Lockfile,
    key: &str,
    resolution: &str,
    entry: &[serde_json::Value],
    options: &ConvertOptions,
    prefetcher: &P,
) -> Result<(Source, &'static str, Option<String>)> {
    let (_, spec) = split_package_spec(resolution).ok_or_else(|| Error::InvalidPackage {
        key: key.to_owned(),
        reason: format!("resolution {resolution} has no package specifier"),
    })?;
    if spec.starts_with("http://") || spec.starts_with("https://") {
        if url_has_credentials(spec) {
            return Err(Error::InvalidPackage {
                key: key.to_owned(),
                reason: "source URL embeds credentials".to_owned(),
            });
        }
        let hash = prefetcher.prefetch(spec)?;
        return Ok((
            Source::FetchTarball {
                url: spec.to_owned(),
                hash,
            },
            "tarball",
            None,
        ));
    }
    if is_path_tarball_spec(spec) {
        return Ok((
            Source::CopyToStore {
                path: prefixed_path(
                    &options.copy_prefix,
                    &lockfile.resolve_path_tarball(key, resolution)?,
                ),
            },
            "tarball",
            None,
        ));
    }
    if let Some(reference) = spec.strip_prefix("github:") {
        return convert_github_source(key, resolution, reference, prefetcher);
    }
    if let Some(reference) = spec.strip_prefix("git+") {
        return convert_git_source(key, resolution, reference, prefetcher);
    }
    convert_npm_source(key, resolution, entry)
}

fn convert_github_source<P: Prefetcher + ?Sized>(
    key: &str,
    resolution: &str,
    reference: &str,
    prefetcher: &P,
) -> Result<(Source, &'static str, Option<String>)> {
    let (repository, rev) = reference
        .split_once('#')
        .ok_or_else(|| Error::InvalidPackage {
            key: key.to_owned(),
            reason: format!("GitHub resolution {resolution} has no revision"),
        })?;
    let (owner, repo) = repository
        .split_once('/')
        .ok_or_else(|| Error::InvalidPackage {
            key: key.to_owned(),
            reason: format!("GitHub resolution {resolution} has no owner/repository"),
        })?;
    let hash = prefetcher.prefetch(&format!(
        "https://github.com/{repository}/archive/{rev}.tar.gz"
    ))?;
    Ok((
        Source::FetchGitHub {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            rev: rev.to_owned(),
            hash,
        },
        "github",
        None,
    ))
}

fn convert_git_source<P: Prefetcher + ?Sized>(
    key: &str,
    resolution: &str,
    reference: &str,
    prefetcher: &P,
) -> Result<(Source, &'static str, Option<String>)> {
    let (url, rev) = reference
        .split_once('#')
        .ok_or_else(|| Error::InvalidPackage {
            key: key.to_owned(),
            reason: format!("Git resolution {resolution} has no revision"),
        })?;
    if (url.starts_with("http://") || url.starts_with("https://")) && url_has_credentials(url) {
        return Err(Error::InvalidPackage {
            key: key.to_owned(),
            reason: "Git source URL embeds credentials".to_owned(),
        });
    }
    let hash = prefetcher.prefetch(&format!("git+{url}?rev={rev}"))?;
    Ok((
        Source::FetchGit {
            url: url.to_owned(),
            rev: rev.to_owned(),
            hash,
        },
        "git",
        None,
    ))
}

fn convert_npm_source(
    key: &str,
    resolution: &str,
    entry: &[serde_json::Value],
) -> Result<(Source, &'static str, Option<String>)> {
    let hash = entry
        .last()
        .and_then(serde_json::Value::as_str)
        .filter(|hash| hash.starts_with("sha256-") || hash.starts_with("sha512-"))
        .ok_or_else(|| Error::InvalidPackage {
            key: key.to_owned(),
            reason: format!("registry resolution {resolution} has no integrity hash"),
        })?;
    let explicit_url = entry
        .get(1)
        .and_then(serde_json::Value::as_str)
        .filter(|url| !url.is_empty());
    if explicit_url.is_some_and(url_has_credentials) {
        return Err(Error::InvalidPackage {
            key: key.to_owned(),
            reason: "registry URL embeds credentials".to_owned(),
        });
    }
    let url = explicit_url.map_or_else(|| npm_url(resolution), |url| Ok(url.to_owned()))?;
    let registry = explicit_url.and_then(registry_host);
    if registry
        .as_ref()
        .is_some_and(|registry| registry.len() > 32)
    {
        return Err(Error::InvalidPackage {
            key: key.to_owned(),
            reason: "long private registry hostname requires Bun's registry URL hash, which bun.lock does not preserve".to_owned(),
        });
    }
    let name = registry
        .as_ref()
        .map(|_| npm_tarball_name(resolution))
        .transpose()?;
    Ok((
        Source::FetchUrl {
            url,
            hash: hash.to_owned(),
            name,
        },
        "npm",
        registry,
    ))
}

fn local_source(resolution: &str) -> Option<(&'static str, &str)> {
    [
        ("workspace", "@workspace:"),
        ("file", "@file:"),
        ("folder", "@folder:"),
        ("link", "@link:"),
        ("root", "@root:"),
    ]
    .into_iter()
    .find_map(|(kind, marker)| {
        resolution
            .find(marker)
            .map(|position| (kind, &resolution[position + marker.len()..]))
    })
}

fn valid_local_path(kind: &str, path: &str) -> bool {
    if path.is_empty() {
        return kind == "root";
    }
    Path::new(path)
        .components()
        .all(|component| matches!(component, Component::CurDir | Component::Normal(_)))
}

fn prefixed_path(prefix: &str, path: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    let path = path.trim_start_matches("./");
    format!("{prefix}/{path}")
        .trim_start_matches("./")
        .to_owned()
}

fn npm_url(resolution: &str) -> Result<String> {
    let (name, version) = split_package_spec(resolution).ok_or_else(|| Error::InvalidPackage {
        key: resolution.to_owned(),
        reason: "registry resolution has no version".to_owned(),
    })?;
    let basename = name.rsplit('/').next().unwrap_or(name);
    Ok(format!(
        "{DEFAULT_REGISTRY}/{name}/-/{basename}-{version}.tgz"
    ))
}

fn npm_tarball_name(resolution: &str) -> Result<String> {
    let (name, version) = split_package_spec(resolution).ok_or_else(|| Error::InvalidPackage {
        key: resolution.to_owned(),
        reason: "registry resolution has no version".to_owned(),
    })?;
    Ok(format!(
        "{}-{version}.tgz",
        name.rsplit('/').next().unwrap_or(name)
    ))
}

fn registry_host(url: &str) -> Option<String> {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = without_scheme.split('/').next()?;
    let host_and_port = authority.rsplit('@').next()?;
    let host = if let Some(bracketed) = host_and_port.strip_prefix('[') {
        bracketed.split_once(']').map(|(host, _)| host)?
    } else {
        host_and_port.split(':').next()?
    };
    (host != "registry.npmjs.org").then(|| host.to_owned())
}

fn url_has_credentials(url: &str) -> bool {
    url.split_once("://")
        .and_then(|(_, remainder)| remainder.split('/').next())
        .is_some_and(|authority| authority.contains('@'))
}

fn render(
    version: u8,
    packages: &BTreeMap<String, ConvertedPackage>,
    production_closures: &BTreeMap<String, Vec<String>>,
    check_closures: &BTreeMap<String, Vec<String>>,
    development_closures: &BTreeMap<String, Vec<String>>,
) -> String {
    let mut output = String::from(
        "# Autogenerated by `bun2nix`; editing manually is not recommended\n\
         # Package sources and lock metadata for composable Bun cache derivations\n\
         {\n\
           copyPathToStore,\n\
           fetchFromGitHub,\n\
           fetchgit,\n\
           fetchurl,\n\
           ...\n\
         }:\n\
         {\n\
           packages = {\n",
    );
    for (resolution, package) in packages {
        render_source(&mut output, resolution, &package.source);
    }
    output.push_str("  };\n  metadata = {\n");
    writeln!(output, "    lockfileVersion = {version};").expect("writing to String cannot fail");
    output.push_str("    workspacePackages = [");
    for (resolution, package) in packages {
        if package.metadata.local {
            write!(output, " \"{}\"", nix_escape(resolution))
                .expect("writing to String cannot fail");
        }
    }
    output.push_str(" ];\n");
    render_closures(
        &mut output,
        "productionDependencyClosures",
        production_closures,
    );
    render_closures(&mut output, "checkDependencyClosures", check_closures);
    render_closures(
        &mut output,
        "developmentDependencyClosures",
        development_closures,
    );
    output.push_str("    packages = {\n");
    for (resolution, package) in packages {
        writeln!(output, "      \"{}\" = {{", nix_escape(resolution))
            .expect("writing to String cannot fail");
        writeln!(output, "        source = \"{}\";", package.metadata.kind)
            .expect("writing to String cannot fail");
        writeln!(output, "        local = {};", package.metadata.local)
            .expect("writing to String cannot fail");
        render_optional_list(&mut output, "os", package.metadata.os.as_deref());
        render_optional_list(&mut output, "cpu", package.metadata.cpu.as_deref());
        match &package.metadata.registry {
            Some(registry) => writeln!(output, "        registry = \"{}\";", nix_escape(registry))
                .expect("writing to String cannot fail"),
            None => output.push_str("        registry = null;\n"),
        }
        output.push_str("      };\n");
    }
    output.push_str("    };\n  };\n}\n");
    output
}

fn render_closures(output: &mut String, name: &str, closures: &BTreeMap<String, Vec<String>>) {
    writeln!(output, "    {name} = {{").expect("writing to String cannot fail");
    for (workspace, resolutions) in closures {
        write!(output, "      \"{}\" = [", nix_escape(workspace))
            .expect("writing to String cannot fail");
        for resolution in resolutions {
            write!(output, " \"{}\"", nix_escape(resolution))
                .expect("writing to String cannot fail");
        }
        output.push_str(" ];\n");
    }
    output.push_str("    };\n");
}

fn render_source(output: &mut String, resolution: &str, source: &Source) {
    write!(output, "    \"{}\" = ", nix_escape(resolution)).expect("writing to String cannot fail");
    match source {
        Source::FetchUrl { url, hash, name } => {
            output.push_str("fetchurl {\n");
            writeln!(output, "      url = \"{}\";", nix_escape(url))
                .expect("writing to String cannot fail");
            writeln!(output, "      hash = \"{}\";", nix_escape(hash))
                .expect("writing to String cannot fail");
            if let Some(name) = name {
                writeln!(output, "      name = \"{}\";", nix_escape(name))
                    .expect("writing to String cannot fail");
            }
            output.push_str("    };\n");
        }
        Source::FetchGit { url, rev, hash } => {
            output.push_str("fetchgit {\n");
            writeln!(output, "      url = \"{}\";", nix_escape(url))
                .expect("writing to String cannot fail");
            writeln!(output, "      rev = \"{}\";", nix_escape(rev))
                .expect("writing to String cannot fail");
            writeln!(output, "      hash = \"{}\";", nix_escape(hash))
                .expect("writing to String cannot fail");
            output.push_str("    };\n");
        }
        Source::FetchGitHub {
            owner,
            repo,
            rev,
            hash,
        } => {
            output.push_str("fetchFromGitHub {\n");
            writeln!(output, "      owner = \"{}\";", nix_escape(owner))
                .expect("writing to String cannot fail");
            writeln!(output, "      repo = \"{}\";", nix_escape(repo))
                .expect("writing to String cannot fail");
            writeln!(output, "      rev = \"{}\";", nix_escape(rev))
                .expect("writing to String cannot fail");
            writeln!(output, "      hash = \"{}\";", nix_escape(hash))
                .expect("writing to String cannot fail");
            output.push_str("    };\n");
        }
        Source::FetchTarball { url, hash } => {
            output.push_str("builtins.fetchTarball {\n");
            writeln!(output, "      url = \"{}\";", nix_escape(url))
                .expect("writing to String cannot fail");
            writeln!(output, "      sha256 = \"{}\";", nix_escape(hash))
                .expect("writing to String cannot fail");
            output.push_str("    };\n");
        }
        Source::CopyToStore { path } => {
            writeln!(output, "copyPathToStore (./. + \"/{}\");", nix_escape(path))
                .expect("writing to String cannot fail");
        }
    }
}

fn render_optional_list(output: &mut String, name: &str, values: Option<&[String]>) {
    let Some(values) = values else {
        writeln!(output, "        {name} = null;").expect("writing to String cannot fail");
        return;
    };
    write!(output, "        {name} = [").expect("writing to String cannot fail");
    for value in values {
        write!(output, " \"{}\"", nix_escape(value)).expect("writing to String cannot fail");
    }
    output.push_str(" ];\n");
}

fn nix_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace("${", "\\${")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}
