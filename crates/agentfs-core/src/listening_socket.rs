//! [`Listener`], the one way agentfs listens on a Unix socket: the control
//! socket of a daemon (design §2.2, §15), the history service's socket
//! (§14) and agentfs-csi's socket (§14.2).
//!
//! **Shut down, not only closed.** A listening socket is an open file, and
//! a child process spawned by any thread of this process shares it from its
//! fork until its exec closes it. `SOCK_CLOEXEC`, which std and tokio set on
//! every socket they open, closes it only at that exec, so it does not
//! shorten the window. Closed only, a listener would go on taking
//! connections for as long as such a child takes to reach its exec, and
//! each would wait for a reply no one sends: a client would hang where it
//! should hear that nothing runs. A server that stops accepting but keeps
//! its listener open does the same in-process: tonic holds its incoming
//! stream while its connections drain. So a listener is stopped with
//! `shutdown(SHUT_RDWR)`, which refuses every later connect at once,
//! whatever shares the socket, and wakes an accept waiting on it. Dropping
//! a [`Listener`], an [`AsyncListener`] or a [`Stopper`] stops it, as a
//! [`crate::file_lock::FileLock`] unlocks before it closes (§2.2).
//!
//! Once stopped, [`Listener::accept`] answers `None` and an
//! [`AsyncListener`] ends, so an accept loop ends with it rather than spin
//! on the error a shut-down socket gives.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

/// Whether a socket was stopped, shared by its listener and stoppers.
#[derive(Debug, Default)]
struct Stopped(AtomicBool);

impl Stopped {
    fn get(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Stops a [`Listener`], or the [`AsyncListener`] it became, from
/// elsewhere: the thread that owns a server while another runs its accept
/// loop. Dropping it stops the listener too. Each listener holds one, which
/// is how dropping the listener stops it.
#[derive(Debug)]
pub struct Stopper {
    /// The socket, through a descriptor of the stopper's own.
    socket: OwnedFd,
    stopped: Arc<Stopped>,
}

impl Stopper {
    fn of(socket: BorrowedFd<'_>, stopped: &Arc<Stopped>) -> io::Result<Self> {
        Ok(Self {
            socket: socket.try_clone_to_owned()?,
            stopped: stopped.clone(),
        })
    }

    /// Refuses every later connect, whatever shares the socket, and wakes
    /// an accept waiting on it, which then answers that it stopped. The
    /// mark comes first, so the woken accept sees it.
    pub fn stop(&self) -> io::Result<()> {
        self.stopped.0.store(true, Ordering::SeqCst);
        // SAFETY: the descriptor is open for as long as `self` is.
        if unsafe { libc::shutdown(self.socket.as_raw_fd(), libc::SHUT_RDWR) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

impl Drop for Stopper {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// A listening Unix socket, shut down when it is stopped or dropped.
#[derive(Debug)]
pub struct Listener {
    /// Declared first, so dropped first: the socket is shut down before
    /// the listener's own descriptor closes.
    guard: Stopper,
    socket: UnixListener,
}

impl Listener {
    /// Listens at `path`, which must not exist.
    pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
        #[allow(
            clippy::disallowed_methods,
            reason = "the one home of agentfs's listening sockets"
        )]
        let socket = UnixListener::bind(path)?;
        Ok(Self {
            guard: Stopper::of(socket.as_fd(), &Arc::default())?,
            socket,
        })
    }

    /// Waits for the next connection; `None` once the listener is stopped.
    pub fn accept(&self) -> io::Result<Option<UnixStream>> {
        let accepted = self.socket.accept();
        if self.guard.stopped.get() {
            return Ok(None);
        }
        accepted.map(|(stream, _)| Some(stream))
    }

    /// See [`Stopper::stop`].
    pub fn stop(&self) -> io::Result<()> {
        self.guard.stop()
    }

    /// A handle that stops this listener from elsewhere, or when dropped.
    pub fn stopper(&self) -> io::Result<Stopper> {
        Stopper::of(self.socket.as_fd(), &self.guard.stopped)
    }

    /// The listener for tokio, registered with the runtime this is called
    /// in. Its stoppers carry over.
    pub fn into_async(self) -> io::Result<AsyncListener> {
        self.socket.set_nonblocking(true)?;
        #[allow(
            clippy::disallowed_methods,
            reason = "the one home of agentfs's listening sockets"
        )]
        let socket = tokio::net::UnixListener::from_std(self.socket)?;
        Ok(AsyncListener {
            guard: self.guard,
            socket,
        })
    }
}

/// A [`Listener`] for tokio: a stream of connections, as tonic's
/// `serve_with_incoming` takes one, that ends once it is stopped. Dropping
/// it stops it.
#[derive(Debug)]
pub struct AsyncListener {
    /// Dropped first, as in [`Listener`].
    guard: Stopper,
    socket: tokio::net::UnixListener,
}

impl AsyncListener {
    /// See [`Listener::stopper`].
    pub fn stopper(&self) -> io::Result<Stopper> {
        Stopper::of(self.socket.as_fd(), &self.guard.stopped)
    }
}

impl tokio_stream::Stream for AsyncListener {
    type Item = io::Result<tokio::net::UnixStream>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.guard.stopped.get() {
            return Poll::Ready(None);
        }
        match self.socket.poll_accept(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(_) if self.guard.stopped.get() => Poll::Ready(None),
            Poll::Ready(accepted) => Poll::Ready(Some(accepted.map(|(stream, _)| stream))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn bound() -> (tempfile::TempDir, std::path::PathBuf, Listener) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s");
        let listener = Listener::bind(&path).unwrap();
        (tmp, path, listener)
    }

    fn refused(path: &Path) -> bool {
        UnixStream::connect(path).unwrap_err().kind() == io::ErrorKind::ConnectionRefused
    }

    /// What a child forked by another thread holds until its exec: the same
    /// socket, through a descriptor of its own.
    fn forked(listener: &Listener) -> OwnedFd {
        listener.socket.as_fd().try_clone_to_owned().unwrap()
    }

    #[test]
    fn a_dropped_listener_refuses_connects_while_a_child_shares_it() {
        let (_tmp, path, listener) = bound();
        let forked = forked(&listener);
        UnixStream::connect(&path).unwrap();
        drop(listener);
        assert!(refused(&path));
        drop(forked);
    }

    #[test]
    fn a_stopper_stops_the_listener_and_wakes_its_accept() {
        let (_tmp, path, listener) = bound();
        let forked = forked(&listener);
        let stopper = listener.stopper().unwrap();
        let accepting = std::thread::spawn(move || {
            let first = listener.accept().unwrap();
            let second = listener.accept().unwrap();
            (first.is_some(), second.is_none())
        });
        let _client = UnixStream::connect(&path).unwrap();
        // Most likely blocked in its second accept by now; either way the
        // stop ends it.
        std::thread::sleep(Duration::from_millis(50));
        stopper.stop().unwrap();
        assert_eq!(accepting.join().unwrap(), (true, true));
        assert!(refused(&path));
        drop(forked);
    }

    #[test]
    fn a_dropped_stopper_stops_the_listener() {
        let (_tmp, path, listener) = bound();
        drop(listener.stopper().unwrap());
        // Refused first: an accept on a listener still running would wait.
        assert!(refused(&path));
        assert!(listener.accept().unwrap().is_none());
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap()
    }

    #[test]
    fn an_async_listener_ends_when_stopped_and_refuses_after() {
        use tokio_stream::StreamExt;
        let (_tmp, path, listener) = bound();
        let forked = forked(&listener);
        runtime().block_on(async {
            let mut incoming = listener.into_async().unwrap();
            let stopper = incoming.stopper().unwrap();
            let _client = tokio::net::UnixStream::connect(&path).await.unwrap();
            assert!(incoming.next().await.unwrap().is_ok());
            let next = tokio::spawn(async move { incoming.next().await.is_none() });
            tokio::task::yield_now().await;
            stopper.stop().unwrap();
            assert!(next.await.unwrap());
        });
        assert!(refused(&path));
        drop(forked);
    }

    #[test]
    fn a_dropped_async_listener_refuses_connects_while_a_child_shares_it() {
        let (_tmp, path, listener) = bound();
        let forked = forked(&listener);
        runtime().block_on(async { drop(listener.into_async().unwrap()) });
        assert!(refused(&path));
        drop(forked);
    }
}
