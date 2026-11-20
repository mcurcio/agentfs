//! The plugin's side of a host's control channel (docs/csi.md): the
//! parent's end of `agentfs host`'s standard input, one JSON object per line
//! ([`agentfs_contracts::host`]).
//!
//! One task owns the stream, as an actor: it reads [`Hello`], then takes
//! each call in order, writes its request and reads its reply. A caller the
//! kubelet's deadline cancels only drops its wait; the task still reads that
//! reply and discards it, so the replies that follow stay in step. When
//! every [`HostClient`] clone is dropped the task closes the stream, and the
//! host sees EOF and drains.

use agentfs_contracts::host::{Hello, Reply, Request};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};

/// The host closed its end, wrote something that is no reply, or a write to
/// it failed.
#[derive(Debug, Clone, thiserror::Error)]
#[error("the volume's host is gone")]
pub struct HostGone;

/// One call: a request and where its reply goes.
struct Call {
    request: Request,
    reply: oneshot::Sender<Result<Reply, HostGone>>,
}

/// A handle on the task that owns one host's control channel.
#[derive(Clone)]
pub struct HostClient {
    calls: mpsc::UnboundedSender<Call>,
}

impl std::fmt::Debug for HostClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostClient").finish_non_exhaustive()
    }
}

impl HostClient {
    /// Reads [`Hello`], then serves calls in order from one task that owns
    /// the stream.
    ///
    /// # Errors
    ///
    /// [`HostGone`] when the stream ends, or its first line is no `Hello`.
    pub async fn start(stream: UnixStream) -> Result<(Self, Hello), HostGone> {
        let (read, write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let hello: Hello = read_line(&mut lines).await?;
        let (calls, queue) = mpsc::unbounded_channel();
        tokio::spawn(serve(lines, write, queue));
        Ok((Self { calls }, hello))
    }

    /// Sends `request` and waits for its reply. Cancel-safe: a dropped call
    /// still has its reply read, and discarded, by the task.
    ///
    /// # Errors
    ///
    /// [`HostGone`] when the host is gone before the reply.
    pub async fn call(&self, request: Request) -> Result<Reply, HostGone> {
        let (reply, answer) = oneshot::channel();
        self.calls
            .send(Call { request, reply })
            .map_err(|_| HostGone)?;
        answer.await.map_err(|_| HostGone)?
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

/// One request written and its reply read.
async fn exchange(
    lines: &mut Lines<BufReader<OwnedReadHalf>>,
    write: &mut OwnedWriteHalf,
    request: &Request,
) -> Result<Reply, HostGone> {
    let mut line = serde_json::to_vec(request).map_err(|_| HostGone)?;
    line.push(b'\n');
    write.write_all(&line).await.map_err(|_| HostGone)?;
    read_line(lines).await
}

/// The task: each call in order until every client is gone or the host is.
/// Dropping the halves at the end closes the stream.
async fn serve(
    mut lines: Lines<BufReader<OwnedReadHalf>>,
    mut write: OwnedWriteHalf,
    mut queue: mpsc::UnboundedReceiver<Call>,
) {
    while let Some(call) = queue.recv().await {
        let result = exchange(&mut lines, &mut write, &call.request).await;
        let gone = result.is_err();
        // A caller that gave up has dropped its receiver: discarded.
        let _ = call.reply.send(result);
        if gone {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use agentfs_contracts::host::HOST_PROTOCOL;
    use agentfs_contracts::name::VolumeName;

    use super::*;

    fn hello() -> Hello {
        let hello: Hello = serde_json::from_str(
            r#"{"v":1,"volume":"t","id":"01234567-89ab-4def-8123-456789abcdef"}"#,
        )
        .unwrap();
        assert_eq!(hello.v, HOST_PROTOCOL);
        assert_eq!(hello.volume, VolumeName::parse("t").unwrap());
        hello
    }

    /// A fake host on the other end: writes Hello, then for each request
    /// line sends the request to the test and writes the reply the test
    /// hands it.
    struct Fake {
        requests: mpsc::UnboundedReceiver<Request>,
        replies: mpsc::UnboundedSender<Reply>,
    }

    async fn fake() -> (HostClient, Fake) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (requests_tx, requests) = mpsc::unbounded_channel();
        let (replies, mut replies_rx) = mpsc::unbounded_channel::<Reply>();
        tokio::spawn(async move {
            let (read, mut write) = theirs.into_split();
            let mut line = serde_json::to_string(&hello()).unwrap();
            line.push('\n');
            write.write_all(line.as_bytes()).await.unwrap();
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let request: Request = serde_json::from_str(&line).unwrap();
                if requests_tx.send(request).is_err() {
                    return;
                }
                let Some(reply) = replies_rx.recv().await else {
                    return;
                };
                let mut line = serde_json::to_string(&reply).unwrap();
                line.push('\n');
                write.write_all(line.as_bytes()).await.unwrap();
            }
            // EOF: the client closed its end.
            drop(requests_tx);
        });
        let (client, got) = HostClient::start(ours).await.unwrap();
        assert_eq!(got, hello());
        (client, Fake { requests, replies })
    }

    fn close(at: &str) -> Request {
        Request::Close {
            at: PathBuf::from(at),
        }
    }

    #[tokio::test]
    async fn replies_come_in_order() {
        let (client, mut fake) = fake().await;
        fake.replies.send(Reply::Closed).unwrap();
        fake.replies.send(Reply::Reconciled { ended: 1 }).unwrap();
        assert_eq!(client.call(close("/a")).await.unwrap(), Reply::Closed);
        assert_eq!(
            client.call(Request::Reconcile).await.unwrap(),
            Reply::Reconciled { ended: 1 }
        );
        assert_eq!(fake.requests.recv().await.unwrap(), close("/a"));
        assert_eq!(fake.requests.recv().await.unwrap(), Request::Reconcile);
    }

    /// A call cancelled while its request is with the host: its reply is
    /// read and discarded, and the next call gets its own reply.
    #[tokio::test]
    async fn a_cancelled_call_does_not_shift_the_replies() {
        let (client, mut fake) = fake().await;
        let first = tokio::spawn({
            let client = client.clone();
            async move { client.call(close("/a")).await }
        });
        // The request reached the host: the call is mid-flight.
        assert_eq!(fake.requests.recv().await.unwrap(), close("/a"));
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let second = tokio::spawn({
            let client = client.clone();
            async move { client.call(Request::Reconcile).await }
        });
        fake.replies.send(Reply::Closed).unwrap();
        assert_eq!(fake.requests.recv().await.unwrap(), Request::Reconcile);
        fake.replies.send(Reply::Reconciled { ended: 0 }).unwrap();
        assert_eq!(
            second.await.unwrap().unwrap(),
            Reply::Reconciled { ended: 0 }
        );
    }

    #[tokio::test]
    async fn every_client_dropped_is_eof_and_a_gone_host_is_host_gone() {
        let (client, mut first) = fake().await;
        drop(client);
        assert_eq!(first.requests.recv().await, None, "the fake saw EOF");

        let (ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        assert!(HostClient::start(ours).await.is_err(), "no hello");

        let (client, second) = fake().await;
        drop(second);
        // The fake's task ends at its next request: the stream closes.
        assert!(client.call(Request::Reconcile).await.is_err());
        assert!(client.call(Request::Reconcile).await.is_err());
    }
}
