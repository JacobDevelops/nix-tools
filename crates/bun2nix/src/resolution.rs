use std::path::Path;

pub(crate) fn split_package_spec(resolution: &str) -> Option<(&str, &str)> {
    let delimiter = if resolution.starts_with('@') {
        let slash = resolution.find('/')?;
        resolution[slash + 1..]
            .find('@')
            .map(|offset| slash + 1 + offset)?
    } else {
        resolution.find('@')?
    };
    Some((&resolution[..delimiter], &resolution[delimiter + 1..]))
}

pub(crate) fn is_path_tarball_spec(spec: &str) -> bool {
    if spec.contains("://") {
        return false;
    }
    let path = Path::new(spec);
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        return false;
    };
    extension.eq_ignore_ascii_case("tgz")
        || extension.eq_ignore_ascii_case("tar")
        || (extension.eq_ignore_ascii_case("gz")
            && path
                .file_stem()
                .and_then(|stem| Path::new(stem).extension())
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("tar")))
}

pub(crate) fn workspace_path(resolution: &str) -> Option<&str> {
    let marker = "@workspace:";
    resolution
        .find(marker)
        .map(|position| &resolution[position + marker.len()..])
}
