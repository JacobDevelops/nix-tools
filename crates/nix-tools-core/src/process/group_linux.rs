use std::io;
use std::os::fd::OwnedFd;

pub(super) fn group_members(group: u32) -> io::Result<Vec<u32>> {
    let mut members = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let stat = match std::fs::read(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if live_in_group(&stat, group)? {
            members.push(pid);
        }
    }
    Ok(members)
}

fn live_in_group(stat: &[u8], group: u32) -> io::Result<bool> {
    let end = stat
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or_else(|| io::Error::other("malformed process status"))?;
    let fields = std::str::from_utf8(&stat[end + 1..]).map_err(io::Error::other)?;
    let mut fields = fields.split_ascii_whitespace();
    let state = fields
        .next()
        .ok_or_else(|| io::Error::other("missing process state"))?;
    let process_group = fields
        .nth(1)
        .ok_or_else(|| io::Error::other("missing process group"))?
        .parse::<u32>()
        .map_err(io::Error::other)?;
    Ok(!matches!(state, "Z" | "X" | "x") && process_group == group)
}

pub(super) fn watch(pid: u32) -> io::Result<Option<OwnedFd>> {
    let pid = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| io::Error::other("invalid process group member"))?;
    match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) {
        Ok(fd) => Ok(Some(fd)),
        Err(error)
            if [
                rustix::io::Errno::SRCH,
                rustix::io::Errno::NOSYS,
                rustix::io::Errno::INVAL,
                rustix::io::Errno::PERM,
            ]
            .contains(&error) =>
        {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
#[path = "group_linux_test.rs"]
mod tests;
