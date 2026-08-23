use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::pq::kill::{SignalSender, Target};
use crate::pq::proc;

struct ProcessHandle { fd: Option<OwnedFd>, start_time: Option<u64> }

pub struct RealSender { handles: RefCell<HashMap<i32, ProcessHandle>> }

impl RealSender {
    pub fn new(targets: &[Target]) -> Self {
        let handles = targets.iter().map(|target| {
            let identity_matches = target.start_time.is_some()
                && proc::start_time(target.pid) == target.start_time;
            let raw = if identity_matches {
                unsafe { libc::syscall(libc::SYS_pidfd_open, target.pid, 0) as i32 }
            } else { -1 };
            let fd = (raw >= 0 && proc::start_time(target.pid) == target.start_time)
                .then(|| unsafe { OwnedFd::from_raw_fd(raw) });
            if raw >= 0 && fd.is_none() { unsafe { libc::close(raw); } }
            (target.pid, ProcessHandle { fd, start_time: target.start_time })
        }).collect();
        Self { handles: RefCell::new(handles) }
    }

    fn same_process(handle: &ProcessHandle, pid: i32) -> bool {
        handle.start_time.is_some_and(|expected| proc::start_time(pid) == Some(expected))
    }
}

impl SignalSender for RealSender {
    fn signal(&self, pid: i32, sig: i32) -> bool {
        let handles = self.handles.borrow();
        let Some(handle) = handles.get(&pid) else { return false };
        if let Some(fd) = &handle.fd {
            unsafe { libc::syscall(libc::SYS_pidfd_send_signal, fd.as_raw_fd(), sig, 0, 0) == 0 }
        } else {
            Self::same_process(handle, pid) && unsafe { libc::kill(pid, sig) == 0 }
        }
    }

    fn alive(&self, pid: i32) -> bool {
        let handles = self.handles.borrow();
        let Some(handle) = handles.get(&pid) else { return false };
        if let Some(fd) = &handle.fd {
            let mut pollfd = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            unsafe { libc::poll(&mut pollfd, 1, 0) == 0 }
        } else {
            Self::same_process(handle, pid) && unsafe { libc::kill(pid, 0) == 0 }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(pid: i32, start_time: Option<u64>) -> Target {
        Target { pid, identity: "test".into(), cmd: "test".into(), uid: 0, sock: None, start_time }
    }

    #[test]
    fn sender_tracks_a_live_process_without_signaling_it() {
        let pid = std::process::id() as i32;
        let start = proc::start_time(pid).unwrap();
        let sender = RealSender::new(&[target(pid, Some(start))]);
        assert!(sender.alive(pid));
        assert!(sender.signal(pid, 0));
        assert!(!sender.alive(pid + 1_000_000));
        assert!(!sender.signal(pid + 1_000_000, 0));
    }

    #[test]
    fn sender_rejects_reused_or_unverifiable_process_identity() {
        let pid = std::process::id() as i32;
        let sender = RealSender::new(&[target(pid, None), target(pid + 1_000_000, Some(1))]);
        assert!(!sender.alive(pid));
        assert!(!sender.signal(pid, 0));
    }
}
