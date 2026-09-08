use std::io;
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

use nix::poll::{PollFd, PollFlags, poll};

use super::{group_members, watch};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn group_members_excludes_exited_children_before_reaping() -> io::Result<()> {
    let mut child = ChildGuard(
        Command::new("/bin/sh")
            .args(["-c", "read marker"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?,
    );
    let pid = child.0.id();
    assert_eq!(group_members(pid)?, vec![pid]);
    let event = watch(pid)?.expect("live child watch");
    drop(child.0.stdin.take());
    let mut descriptors = [PollFd::new(event.as_fd(), PollFlags::POLLIN)];
    assert!(poll(&mut descriptors, 2000_u16).map_err(io::Error::other)? > 0);
    assert!(group_members(pid)?.is_empty());
    child.0.wait()?;
    assert!(watch(pid)?.is_none());
    Ok(())
}

#[test]
fn an_already_exited_process_needs_no_exit_watch() -> io::Result<()> {
    let mut child = ChildGuard(Command::new("/bin/sh").args(["-c", "exit 0"]).spawn()?);
    child.0.wait()?;
    assert!(watch(child.0.id())?.is_none());
    Ok(())
}
