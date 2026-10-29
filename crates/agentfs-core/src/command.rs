//! Running an operator's program (design §11.5 "Delivery", §12; beads
//! agentfs-mia.2, mia.5): the one runner behind fork delivery's command
//! method ([`crate::fork::deliver`]) and the patch queue's `patch-accept`
//! hook ([`crate::patch::accept`]).
//!
//! The program runs without a shell (`argv[0]` with `argv[1..]` as its
//! arguments, literally), in an empty environment but `PATH` (this
//! process's), in its own process group, with `stdin` written to its
//! standard input from a thread of its own (a program that does not read
//! it never stalls the runner). Its standard output goes to this
//! process's standard error, never its standard output. At the deadline
//! the whole process group is killed, so nothing it started outlives it.

use std::io::{self, Read as _, Write as _};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How a program that ran ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finished {
    /// It exited with this status.
    Code(i32),
    /// A signal killed it.
    Signal(i32),
}

/// Why a program did not run to its end.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// It could not be started (missing, not executable, an empty argv).
    #[error("could not start: {0}")]
    NotStarted(#[source] io::Error),
    /// It outlived its deadline and was killed, with its process group.
    #[error("killed after {0:?}")]
    TimedOut(Duration),
    /// Waiting for it failed.
    #[error("waiting for it: {0}")]
    Wait(#[source] io::Error),
}

/// Runs `argv` with `stdin` on its standard input, waiting at most
/// `timeout` (see the module docs). Its standard error is this process's.
pub fn run_with_stdin(
    argv: &[String],
    stdin: &[u8],
    timeout: Duration,
) -> Result<Finished, RunError> {
    run(argv, stdin, timeout, false).map(|(finished, _)| finished)
}

/// [`run_with_stdin`], with the program's standard error captured instead
/// of passed through: what it wrote there before it ended (read until it
/// closes the stream or the deadline passes).
pub fn run_capturing_stderr(
    argv: &[String],
    stdin: &[u8],
    timeout: Duration,
) -> Result<(Finished, Vec<u8>), RunError> {
    run(argv, stdin, timeout, true)
}

fn run(
    argv: &[String],
    stdin: &[u8],
    timeout: Duration,
    capture: bool,
) -> Result<(Finished, Vec<u8>), RunError> {
    let Some((program, args)) = argv.split_first() else {
        return Err(RunError::NotStarted(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no program to run",
        )));
    };
    let deadline = Instant::now() + timeout;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env_clear()
        .stdin(Stdio::piped())
        // Its output is the runner's log, never the runner's stdout.
        .stdout(stderr_stdio())
        .stderr(if capture {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        // Its own process group, so a timeout kills what it started.
        .process_group(0);
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    let mut child = cmd.spawn().map_err(RunError::NotStarted)?;
    if let Some(mut pipe) = child.stdin.take() {
        let input = stdin.to_vec();
        // A program that does not read its input is its own affair; the
        // thread ends when the pipe closes.
        std::thread::spawn(move || {
            let _ = pipe.write_all(&input);
        });
    }
    let stderr = child.stderr.take().map(|mut pipe| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            let _ = tx.send(bytes);
        });
        rx
    });
    let status = wait(&mut child, deadline, timeout)?;
    // What it wrote, unless something it left behind holds the stream
    // open past the deadline.
    let captured = stderr
        .and_then(|rx| {
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .ok()
        })
        .unwrap_or_default();
    let finished = match status.code() {
        Some(code) => Finished::Code(code),
        None => Finished::Signal(status.signal().unwrap_or(0)),
    };
    Ok((finished, captured))
}

/// Waits for `child` until `deadline`; past it, kills its process group.
fn wait(child: &mut Child, deadline: Instant, timeout: Duration) -> Result<ExitStatus, RunError> {
    loop {
        if let Some(status) = child.try_wait().map_err(RunError::Wait)? {
            return Ok(status);
        }
        let now = Instant::now();
        if now >= deadline {
            let pid = rustix::process::Pid::from_child(child);
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            let _ = child.kill();
            let _ = child.wait();
            return Err(RunError::TimedOut(timeout));
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(20)));
    }
}

/// This process's stderr, for a child's stdout; null if it cannot be
/// duplicated.
fn stderr_stdio() -> Stdio {
    use std::os::fd::AsFd as _;
    io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map_or_else(|_| Stdio::null(), Stdio::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        ["/bin/sh", "-c", script, "sh"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    const LONG: Duration = Duration::from_secs(30);

    #[test]
    fn exit_codes_and_signals_are_reported() {
        assert_eq!(
            run_with_stdin(&sh("exit 0"), b"", LONG).unwrap(),
            Finished::Code(0)
        );
        assert_eq!(
            run_with_stdin(&sh("exit 75"), b"", LONG).unwrap(),
            Finished::Code(75)
        );
        assert_eq!(
            run_with_stdin(&sh("kill -TERM $$"), b"", LONG).unwrap(),
            Finished::Signal(15)
        );
    }

    #[test]
    fn stdin_reaches_it_and_stderr_is_captured() {
        let (finished, stderr) = run_capturing_stderr(
            &sh("read line; echo \"got $line\" >&2; exit 1"),
            b"hello\n",
            LONG,
        )
        .unwrap();
        assert_eq!(finished, Finished::Code(1));
        assert_eq!(String::from_utf8_lossy(&stderr), "got hello\n");
    }

    /// A large input it never reads does not stall the runner.
    #[test]
    fn unread_input_never_stalls() {
        let big = vec![b'x'; 1 << 20];
        assert_eq!(
            run_with_stdin(&sh("exit 3"), &big, LONG).unwrap(),
            Finished::Code(3)
        );
    }

    #[test]
    fn only_path_is_in_its_environment() {
        let (finished, stderr) = run_capturing_stderr(&sh("env >&2"), b"", LONG).unwrap();
        assert_eq!(finished, Finished::Code(0));
        let env = String::from_utf8_lossy(&stderr).into_owned();
        assert!(env.lines().any(|l| l.starts_with("PATH=")), "{env}");
        assert!(!env.lines().any(|l| l.starts_with("HOME=")), "{env}");
    }

    /// At the deadline the process group dies, a child it started too.
    #[test]
    fn a_timeout_kills_the_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let script = format!("(sleep 1; touch '{}') & sleep 30", marker.display());
        let started = Instant::now();
        let err = run_with_stdin(&sh(&script), b"", Duration::from_millis(200)).unwrap_err();
        assert!(matches!(err, RunError::TimedOut(_)), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!marker.exists(), "a child outlived the deadline");
    }

    #[test]
    fn a_missing_program_does_not_start() {
        let argv = vec!["/nonexistent/agentfs-hook".to_owned()];
        assert!(matches!(
            run_with_stdin(&argv, b"", LONG).unwrap_err(),
            RunError::NotStarted(_)
        ));
        assert!(matches!(
            run_with_stdin(&[], b"", LONG).unwrap_err(),
            RunError::NotStarted(_)
        ));
    }
}
