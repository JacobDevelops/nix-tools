use std::io;
use std::os::fd::OwnedFd;

use libproc::bsd_info::BSDInfo;
use libproc::proc_pid::pidinfo;
use libproc::processes::{ProcFilter, pids_by_type};
use nix::errno::Errno;
use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};

pub(super) fn group_members(pgid: u32) -> io::Result<Vec<u32>> {
    let mut members = pids_by_type(ProcFilter::ByProgramGroup { pgrpid: pgid })?;
    members.retain(|pid| {
        i32::try_from(*pid).is_ok_and(|pid| {
            // Missing metadata cannot prove a process dead; retain it for conservative cleanup.
            pidinfo::<BSDInfo>(pid, 0).map_or(true, |info| {
                info.pbi_status != nix::libc::SZOMB && info.pbi_pgid == pgid
            })
        })
    });
    members.sort_unstable();
    members.dedup();
    Ok(members)
}

pub(super) fn watch(pid: u32) -> io::Result<Option<OwnedFd>> {
    let queue = Kqueue::new().map_err(io::Error::other)?;
    let event = KEvent::new(
        usize::try_from(pid).map_err(io::Error::other)?,
        EventFilter::EVFILT_PROC,
        EvFlags::EV_ADD | EvFlags::EV_ONESHOT,
        FilterFlag::NOTE_EXIT,
        0,
        0,
    );
    match queue.kevent(&[event], &mut [], None) {
        Ok(_) => {}
        Err(Errno::ESRCH) => return Ok(None),
        Err(error) => return Err(io::Error::other(error)),
    }
    let pid = i32::try_from(pid).map_err(io::Error::other)?;
    if pidinfo::<BSDInfo>(pid, 0).is_ok_and(|info| info.pbi_status == nix::libc::SZOMB) {
        return Ok(None);
    }
    Ok(Some(queue.into()))
}

#[cfg(test)]
#[path = "group_macos_test.rs"]
mod tests;
