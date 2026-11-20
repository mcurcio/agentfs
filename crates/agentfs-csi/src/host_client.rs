//! The plugin's side of a host's control channel (docs/csi.md): the
//! parent's end of `agentfs host`'s standard input, one JSON object per line
//! ([`agentfs_contracts::host`], protocol v2).
//!
//! Two tasks own the stream: a writer, which gives each call an id (from 1,
//! never used again) and writes its request in an envelope, and a reader,
//! which hands each reply to the call its id names, in whatever order the
//! host answers. Calls do not wait for each other. A caller the kubelet's
//! deadline cancels drops only its own wait: its id stays pending until
//! its reply arrives, and that reply is discarded. A reply with an id no
//! call has is a broken protocol: every waiter gets [`HostGone`] and the
//! stream closes.
//!
//! The stream closes, and the host sees EOF, when its owner calls
//! [`HostClient::close`] (the supervisor, when it stops the host or the
//! plugin shuts down), whatever clones other tasks still hold; or once
//! every clone is dropped.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agentfs_contracts::host::{Envelope, HOST_PROTOCOL, Hello, Reply, Request};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};

/// The host closed its end, wrote something that is no reply, a write to
/// it failed, or its owner closed the channel.
#[derive(Debug, Clone, thiserror::Error)]
#[error("the volume's host is gone")]
pub struct HostGone;

/// Why [`HostClient::start`] has no client.
#[derive(Debug, Clone, thiserror::Error)]
pub enum StartError {
    /// No hello: the host ended, or wrote something else.
    #[error(transparent)]
    Gone(#[from] HostGone),
    /// The host speaks another protocol: it was refused, and its stream
    /// closed. Permanent until the plugin or the binary is replaced.
    #[error(
        "the host speaks host protocol v{host}, this plugin v{plugin}: run an agentfs binary of \
         the plugin's release"
    )]
    Version { host: u32, plugin: u32 },
}

/// What the writer task is asked: a call, or to close the stream.
enum Call {
    Request {
        request: Request,
        reply: oneshot::Sender<Result<Reply, HostGone>>,
    },
    Close,
}

/// A handle on the tasks that own one host's control channel.
#[derive(Clone)]
pub struct HostClient {
    calls: mpsc::UnboundedSender<Call>,
}

impl std::fmt::Debug for HostClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostClient").finish_non_exhaustive()
    }
}

/// The calls written and not yet answered, by id. `None` once the stream
/// is gone: a call then fails at once.
type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Result<Reply, HostGone>>>>>>;

/// Every waiter gets [`HostGone`], and no call is taken any more.
fn fail_all(pending: &Pending) {
    let waiters = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    for (_, waiter) in waiters.into_iter().flatten() {
        let _ = waiter.send(Err(HostGone));
    }
}

impl HostClient {
    /// Reads [`Hello`] and checks its version, then serves calls from a
    /// writer and a reader task that own the stream.
    ///
    /// # Errors
    ///
    /// [`StartError::Gone`] when the stream ends or its first line is no
    /// `Hello`; [`StartError::Version`] when the hello names another
    /// protocol (the stream is closed).
    pub async fn start(stream: UnixStream) -> Result<(Self, Hello), StartError> {
        let (read, write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let hello: Hello = read_line(&mut lines).await?;
        if hello.v != HOST_PROTOCOL {
            let error = StartError::Version {
                host: hello.v,
                plugin: HOST_PROTOCOL,
            };
            eprintln!("the host of {}: {error}", hello.volume);
            return Err(error);
        }
        let (calls, queue) = mpsc::unbounded_channel();
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (broken, reader_ended) = oneshot::channel();
        let reader = tokio::spawn(read_replies(lines, pending.clone(), broken));
        tokio::spawn(write_calls(write, queue, pending, reader_ended, reader));
        Ok((Self { calls }, hello))
    }

    /// Sends `request` and waits for its reply. Cancel-safe: a dropped call
    /// drops only its own wait; its reply is read and discarded.
    ///
    /// # Errors
    ///
    /// [`HostGone`] when the host is gone, or the channel closed, before
    /// the reply.
    pub async fn call(&self, request: Request) -> Result<Reply, HostGone> {
        let (reply, answer) = oneshot::channel();
        self.calls
            .send(Call::Request { request, reply })
            .map_err(|_| HostGone)?;
        answer.await.map_err(|_| HostGone)?
    }

    /// Closes the stream, whatever clones remain: the host sees EOF, and
    /// every call in flight or to come gets [`HostGone`].
    pub fn close(&self) {
        let _ = self.calls.send(Call::Close);
    }
}

/// The next line, parsed.
async fn read_line<T: serde::de::DeserializeOwned>(
    lines: &mut Lines<BufReader<OwnedReadHalf>>,
) -> Result<T, HostGone> {
    let line = lines.next_line().await.ok().flatten().ok_or(HostGone)?;
    serde_json::from_str(&line).map_err(|e| {
        eprintln!("the host wrote a line that is not in its protocol ({e}): {line}");
        HostGone
    })
}

/// The writer: each call given the next id, made pending, and written,
/// until the owner closes the channel, every client is gone, a write
/// fails, or the reader has ended. Dropping the write half at the end is
/// the host's EOF.
async fn write_calls(
    mut write: OwnedWriteHalf,
    mut queue: mpsc::UnboundedReceiver<Call>,
    pending: Pending,
    mut reader_ended: oneshot::Receiver<()>,
    reader: tokio::task::JoinHandle<()>,
) {
    let mut next: u64 = 1;
    loop {
        let call = tokio::select! {
            call = queue.recv() => call,
            _ = &mut reader_ended => None,
        };
        let Some(Call::Request { request, reply }) = call else {
            break;
        };
        let id = next;
        next += 1;
        {
            let mut guard = pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(waiting) = guard.as_mut() else {
                let _ = reply.send(Err(HostGone));
                break;
            };
            waiting.insert(id, reply);
        }
        let line = serde_json::to_vec(&Envelope { id, body: request }).map(|mut line| {
            line.push(b'\n');
            line
        });
        let written = match line {
            Ok(line) => write.write_all(&line).await.is_ok(),
            Err(_) => false,
        };
        if !written {
            break;
        }
    }
    drop(write);
    reader.abort();
    fail_all(&pending);
    // Calls still queued are dropped with the queue: each gets HostGone.
}

/// The reader: each reply handed to the call its id names, until the
/// stream ends or the host breaks the protocol. Dropping `broken` at the
/// end stops the writer.
async fn read_replies(
    mut lines: Lines<BufReader<OwnedReadHalf>>,
    pending: Pending,
    broken: oneshot::Sender<()>,
) {
    while let Ok(Envelope { id, body }) = read_line::<Envelope<Reply>>(&mut lines).await {
        let waiter = {
            let mut guard = pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(waiting) = guard.as_mut() else { break };
            waiting.remove(&id)
        };
        let Some(waiter) = waiter else {
            eprintln!("the host answered id {id}, which no call has: the protocol is broken");
            break;
        };
        // A caller that gave up has dropped its receiver: discarded.
        let _ = waiter.send(Ok(body));
    }
    fail_all(&pending);
    drop(broken);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use agentfs_contracts::name::VolumeName;

    use super::*;

    fn hello_line(v: u32) -> String {
        format!(r#"{{"v":{v},"volume":"t","id":"01234567-89ab-4def-8123-456789abcdef"}}"#)
    }

    /// A fake host on the other end: writes its hello, then sends each
    /// request it reads to the test, and writes each line the test hands
    /// it, in the test's order.
    struct Fake {
        requests: mpsc::UnboundedReceiver<Envelope<Request>>,
        lines: mpsc::UnboundedSender<String>,
    }

    impl Fake {
        async fn request(&mut self) -> Envelope<Request> {
            self.requests.recv().await.unwrap()
        }

        fn reply(&self, id: u64, body: Reply) {
            self.lines
                .send(serde_json::to_string(&Envelope { id, body }).unwrap())
                .unwrap();
        }
    }

    async fn fake() -> (HostClient, Fake) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (requests_tx, requests) = mpsc::unbounded_channel();
        let (lines, mut lines_rx) = mpsc::unbounded_channel::<String>();
        let (read, mut write) = theirs.into_split();
        tokio::spawn(async move {
            let mut line = hello_line(HOST_PROTOCOL);
            line.push('\n');
            write.write_all(line.as_bytes()).await.unwrap();
            while let Some(mut line) = lines_rx.recv().await {
                line.push('\n');
                if write.write_all(line.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
        tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let request = serde_json::from_str(&line).unwrap();
                if requests_tx.send(request).is_err() {
                    return;
                }
            }
            // EOF: the client closed its end.
        });
        let (client, got) = HostClient::start(ours).await.unwrap();
        assert_eq!(got.v, HOST_PROTOCOL);
        assert_eq!(got.volume, VolumeName::parse("t").unwrap());
        (client, Fake { requests, lines })
    }

    fn close(at: &str) -> Request {
        Request::Close {
            at: PathBuf::from(at),
        }
    }

    fn spawn_call(
        client: &HostClient,
        request: Request,
    ) -> tokio::task::JoinHandle<Result<Reply, HostGone>> {
        let client = client.clone();
        tokio::spawn(async move { client.call(request).await })
    }

    /// Two calls in flight, answered in the other order: each reply
    /// reaches its own caller. Ids start at 1.
    #[tokio::test]
    async fn replies_out_of_order_reach_their_callers() {
        let (client, mut fake) = fake().await;
        let first = spawn_call(&client, close("/a"));
        let a = fake.request().await;
        let second = spawn_call(&client, Request::Reconcile);
        let b = fake.request().await;
        assert_eq!((a.id, a.body), (1, close("/a")));
        assert_eq!((b.id, b.body), (2, Request::Reconcile));
        fake.reply(2, Reply::Reconciled { ended: 3 });
        // The timeout only guards a hang: a reply routed to another call.
        let reply = tokio::time::timeout(std::time::Duration::from_secs(10), second)
            .await
            .expect("the second call got no reply");
        assert_eq!(reply.unwrap().unwrap(), Reply::Reconciled { ended: 3 });
        assert!(!first.is_finished());
        fake.reply(1, Reply::Closed);
        assert_eq!(first.await.unwrap().unwrap(), Reply::Closed);
    }

    /// A call cancelled while its request is with the host drops only its
    /// wait: its id stays pending, so its late reply is discarded, not
    /// taken for a broken protocol, and the other calls go on.
    #[tokio::test]
    async fn a_cancelled_call_does_not_disturb_the_others() {
        let (client, mut fake) = fake().await;
        let cancelled = spawn_call(&client, close("/a"));
        let a = fake.request().await;
        let other = spawn_call(&client, Request::Reconcile);
        let b = fake.request().await;
        cancelled.abort();
        assert!(cancelled.await.unwrap_err().is_cancelled());
        fake.reply(a.id, Reply::Closed);
        fake.reply(b.id, Reply::Reconciled { ended: 0 });
        assert_eq!(
            other.await.unwrap().unwrap(),
            Reply::Reconciled { ended: 0 }
        );
        let next = spawn_call(&client, close("/c"));
        let c = fake.request().await;
        assert_eq!(c.id, 3, "ids are never used again");
        fake.reply(c.id, Reply::Closed);
        assert_eq!(next.await.unwrap().unwrap(), Reply::Closed);
    }

    /// A reply whose id no call has: every waiter gets HostGone, and the
    /// stream closes (the fake host sees EOF).
    #[tokio::test]
    async fn an_unknown_reply_id_fails_every_waiter() {
        let (client, mut fake) = fake().await;
        let one = spawn_call(&client, close("/a"));
        fake.request().await;
        let two = spawn_call(&client, close("/b"));
        fake.request().await;
        fake.reply(99, Reply::Closed);
        assert!(one.await.unwrap().is_err());
        assert!(two.await.unwrap().is_err());
        assert!(fake.requests.recv().await.is_none(), "the fake saw EOF");
        assert!(client.call(Request::Reconcile).await.is_err());
    }

    /// `close` ends the stream while clones remain and a call waits: the
    /// waiter gets HostGone and the host sees EOF.
    #[tokio::test]
    async fn close_ends_the_stream_whatever_clones_remain() {
        let (client, mut fake) = fake().await;
        let other = client.clone();
        let waiting = spawn_call(&client, close("/a"));
        fake.request().await;
        client.close();
        assert!(waiting.await.unwrap().is_err());
        assert!(fake.requests.recv().await.is_none(), "the fake saw EOF");
        assert!(other.call(Request::Reconcile).await.is_err());
    }

    #[tokio::test]
    async fn every_client_dropped_is_eof_and_a_gone_host_is_host_gone() {
        let (client, mut first) = fake().await;
        drop(client);
        assert!(first.requests.recv().await.is_none(), "the fake saw EOF");

        let (ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        assert!(matches!(
            HostClient::start(ours).await,
            Err(StartError::Gone(HostGone))
        ));

        let (client, second) = fake().await;
        drop(second);
        // The fake's writer ends: the stream's read side sees EOF.
        assert!(client.call(Request::Reconcile).await.is_err());
        assert!(client.call(Request::Reconcile).await.is_err());
    }

    /// A hello of another version is refused, naming both, and the stream
    /// is closed.
    #[tokio::test]
    async fn a_hello_of_another_version_is_refused() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (read, mut write) = theirs.into_split();
        let mut line = hello_line(HOST_PROTOCOL + 1);
        line.push('\n');
        write.write_all(line.as_bytes()).await.unwrap();
        let error = HostClient::start(ours).await.unwrap_err();
        assert!(
            matches!(error, StartError::Version { host, plugin } if host == HOST_PROTOCOL + 1 && plugin == HOST_PROTOCOL),
            "{error:?}"
        );
        assert!(
            error
                .to_string()
                .contains(&format!("v{}", HOST_PROTOCOL + 1))
        );
        let mut lines = BufReader::new(read).lines();
        assert_eq!(lines.next_line().await.unwrap(), None, "EOF");
    }
}
