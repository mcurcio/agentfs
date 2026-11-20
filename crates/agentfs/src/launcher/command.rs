//! `mount -- COMMAND` (docs/trees.md, "Running a command in the mount"):
//! the command runs once every view is up, with the mount's stdio and
//! environment; its exit is a trigger of the mount's loop, as a stop is, and
//! the mount drains. A stop while it runs sends it SIGTERM first. Prior
//! art: `dbus-run-session`, `ssh-agent COMMAND`, `flock COMMAND`.

use std::ffi::OsString;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::mpsc;

use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};

use super::MountTrigger;
use super::serving::Trigger;

/// The command, running. Signalled through a pidfd, so a signal never
/// reaches another process that took its pid once it was reaped.
pub(super) struct Running {
    pidfd: OwnedFd,
}

impl Running {
    /// Sends it SIGTERM. A command that has already exited is no error.
    pub(super) fn terminate(&self) {
        match pidfd_send_signal(&self.pidfd, Signal::TERM) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            Err(e) => eprintln!("warning: signalling the command: {e}"),
        }
    }
}

/// Spawns `argv` and a thread that waits for it and sends
/// [`MountTrigger::CommandExited`] on `sender`.
///
/// # Errors
///
/// The spawn's error (a missing program, say).
pub(super) fn spawn(
    argv: &[OsString],
    sender: mpsc::Sender<Trigger<MountTrigger>>,
) -> io::Result<Running> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no command given"))?;
    let mut child = std::process::Command::new(program).args(args).spawn()?;
    // Opened before the waiter can reap it: the pid is still the child's.
    let pid = Pid::from_child(&child);
    let pidfd = match pidfd_open(pid, PidfdFlags::empty()) {
        Ok(pidfd) => pidfd,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e.into());
        }
    };
    std::thread::Builder::new()
        .name("mount-command".into())
        .spawn(move || {
            let status = child.wait();
            // The receiver goes only once the loop has ended.
            let _ = sender.send(Trigger::Own(MountTrigger::CommandExited(status)));
        })?;
    Ok(Running { pidfd })
}

/// The exit status a mount answers for its command's: its code, or 128 plus
/// the signal that ended it (as a shell reports it).
pub(super) fn exit_code(status: &io::Result<ExitStatus>) -> u8 {
    match status {
        Ok(status) => match (status.code(), status.signal()) {
            (Some(code), _) => u8::try_from(code & 0xff).unwrap_or(1),
            (None, Some(signal)) => u8::try_from((128 + signal) & 0xff).unwrap_or(1),
            (None, None) => 1,
        },
        Err(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signal_is_128_plus_its_number() {
        assert_eq!(exit_code(&Ok(ExitStatus::from_raw(3 << 8))), 3);
        assert_eq!(exit_code(&Ok(ExitStatus::from_raw(15))), 143);
        assert_eq!(exit_code(&Ok(ExitStatus::from_raw(0))), 0);
    }
}
