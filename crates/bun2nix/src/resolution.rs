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
    !spec.contains("://")
        && Path::new(spec)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("tgz"))
}

pub(crate) fn workspace_path(resolution: &str) -> Option<&str> {
    let marker = "@workspace:";
    resolution
        .find(marker)
        .map(|position| &resolution[position + marker.len()..])
}
