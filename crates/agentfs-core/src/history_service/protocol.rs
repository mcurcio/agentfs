//! The wire protocol between fronts and the history service (design §14).
//!
//! **Frames**: a `u32` big-endian length, then that many bytes of JSON, at
//! most [`MAX_FRAME`] (the journal's own bound on a record). Timeouts are
//! the stream's: callers set read and write timeouts before a frame.
//!
//! **Protocol v1**, synchronous, one request in flight per connection:
//!
//! 1. The front sends [`Hello`]; the service answers [`Reply::Welcome`] or
//!    [`Reply::Refused`] and closes, or [`Reply::Busy`] and closes when it
//!    serves as many connections as it can.
//! 2. Then any number of [`Request`]s, each answered by one [`Reply`]:
//!    - `Tracked{paths}`: whether each path is tracked now, for the rules
//!      of an ignored or nested write. Answered from memory, no fsync.
//!    - `Record{draft}`: the service decides again, journals the draft
//!      with the front's origin, and answers `Recorded` only once it is
//!      durable.
//!    - `Ping`: liveness and round-trip time.
//!
//! The service closes a connection it cannot parse. Nothing is ever resent:
//! a front that loses its connection mid-request reports the boundary as
//! failed (design §3.5).

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::checks::Refusal;
use crate::nested::NestedKind;
use crate::paths::RepoPath;
use crate::record::{Draft, Origin};
use crate::rules::{RuleSource, SkipReason};

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// The largest frame accepted: the journal's bound on one record.
pub const MAX_FRAME: usize = 64 << 20;

/// The first frame on a connection, from the front.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub v: u32,
    /// The volume the front believes it is a front of.
    pub volume: String,
    pub front: FrontInfo,
}

/// Who a front is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontInfo {
    /// Its pod's metadata, journaled with each of its records (design §9).
    pub origin: Origin,
    /// The pending slot it leases ([`super::FrontLease`]).
    pub slot: u16,
    /// Random per front process, so the service can count fronts that hold
    /// several connections.
    pub instance: u64,
    pub pid: u32,
    /// The front's agentfs version, for the service's log.
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Request {
    Tracked { paths: Vec<RepoPath> },
    Record { draft: Box<Draft> },
    Ping,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Welcome {
        volume: String,
        branch: String,
        service_pid: u32,
    },
    Tracked {
        tracked: Vec<bool>,
    },
    /// The draft is durable as `seq` (or was skipped whole, `seq` absent).
    Recorded {
        seq: Option<u64>,
        skipped: Vec<WireSkip>,
    },
    Pong,
    /// The service will not serve this front; the connection is closed.
    Refused {
        reason: String,
    },
    /// The service serves as many connections as it can; the connection
    /// is closed. Not an outage: the front may try again.
    Busy {
        reason: String,
    },
    /// The request failed; nothing was journaled.
    Error {
        message: String,
    },
}

/// A change the service decided not to record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSkip {
    pub path: RepoPath,
    pub reason: WireSkipReason,
}

/// The [`SkipReason`]s the service's own decision can produce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum WireSkipReason {
    NeverRecord,
    Ignored {
        /// The `.gitignore` the pattern is in, or absent for
        /// `info/exclude`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<RepoPath>,
        pattern: String,
    },
    Nested {
        repo: RepoPath,
        nested: WireNestedKind,
    },
    Untracked,
    SharedMemory,
    /// The path left the backing tree before the record was journaled.
    Gone,
    GroupRefused {
        member: RepoPath,
    },
    /// A member of an atomic group the rules skip, so the group is refused
    /// ([`Refusal::GroupMemberSkipped`]).
    GroupMemberSkipped {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireNestedKind {
    GitDir,
    GitFile,
    GitSymlink,
    Bare,
}

impl From<NestedKind> for WireNestedKind {
    fn from(kind: NestedKind) -> Self {
        match kind {
            NestedKind::GitDir => Self::GitDir,
            NestedKind::GitFile => Self::GitFile,
            NestedKind::GitSymlink => Self::GitSymlink,
            NestedKind::Bare => Self::Bare,
        }
    }
}

impl From<WireNestedKind> for NestedKind {
    fn from(kind: WireNestedKind) -> Self {
        match kind {
            WireNestedKind::GitDir => Self::GitDir,
            WireNestedKind::GitFile => Self::GitFile,
            WireNestedKind::GitSymlink => Self::GitSymlink,
            WireNestedKind::Bare => Self::Bare,
        }
    }
}

impl WireSkipReason {
    /// The wire form of `reason`, if the service can send it.
    pub fn from_reason(reason: &SkipReason) -> Option<Self> {
        Some(match reason {
            SkipReason::NeverRecord => Self::NeverRecord,
            SkipReason::Ignored { source, pattern } => Self::Ignored {
                file: match source {
                    RuleSource::OperatorExclude => None,
                    RuleSource::TreeIgnore(file) => Some(file.clone()),
                },
                pattern: pattern.clone(),
            },
            SkipReason::Nested { repo, kind } => Self::Nested {
                repo: repo.clone(),
                nested: (*kind).into(),
            },
            SkipReason::Untracked => Self::Untracked,
            SkipReason::SharedMemory => Self::SharedMemory,
            SkipReason::Gone => Self::Gone,
            SkipReason::GroupRefused { member } => Self::GroupRefused {
                member: member.clone(),
            },
            SkipReason::Refused(Refusal::GroupMemberSkipped { reason }) => {
                Self::GroupMemberSkipped {
                    reason: reason.clone(),
                }
            }
            SkipReason::Refused(_) => return None,
        })
    }

    pub fn into_reason(self) -> SkipReason {
        match self {
            Self::NeverRecord => SkipReason::NeverRecord,
            Self::Ignored { file, pattern } => SkipReason::Ignored {
                source: file.map_or(RuleSource::OperatorExclude, RuleSource::TreeIgnore),
                pattern,
            },
            Self::Nested { repo, nested } => SkipReason::Nested {
                repo,
                kind: nested.into(),
            },
            Self::Untracked => SkipReason::Untracked,
            Self::SharedMemory => SkipReason::SharedMemory,
            Self::Gone => SkipReason::Gone,
            Self::GroupRefused { member } => SkipReason::GroupRefused { member },
            Self::GroupMemberSkipped { reason } => {
                SkipReason::Refused(Refusal::GroupMemberSkipped { reason })
            }
        }
    }
}

/// Why a frame could not be read.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The peer closed the connection before the frame began.
    #[error("the connection was closed")]
    Closed,
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("frame of {0} bytes is over the {MAX_FRAME}-byte limit")]
    TooLarge(usize),
    #[error("malformed frame: {0}")]
    Malformed(String),
}

impl FrameError {
    /// Whether the read gave up on a timeout.
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Io(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
    }
}

/// Writes `msg` as one frame.
pub fn write_frame<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "frame of {} bytes is over the {MAX_FRAME}-byte limit",
                body.len()
            ),
        ));
    }
    let len = u32::try_from(body.len()).map_err(io::Error::other)?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&body);
    w.write_all(&frame)?;
    w.flush()
}

/// Reads one frame's bytes.
pub fn read_frame_bytes(r: &mut impl Read) -> Result<Vec<u8>, FrameError> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < len.len() {
        match r.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Err(FrameError::Closed),
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    Ok(body)
}

/// Reads one frame as a `T`.
pub fn read_frame<T: DeserializeOwned>(r: &mut impl Read) -> Result<T, FrameError> {
    let body = read_frame_bytes(r)?;
    serde_json::from_slice(&body).map_err(|e| FrameError::Malformed(e.to_string()))
}

/// Parses a [`Hello`], checking its version first so a front of another
/// version gets a clear refusal rather than a parse error.
pub fn parse_hello(body: &[u8]) -> Result<Hello, String> {
    #[derive(Deserialize)]
    struct Version {
        v: u32,
    }
    let version: Version =
        serde_json::from_slice(body).map_err(|e| format!("malformed hello: {e}"))?;
    if version.v != PROTOCOL_VERSION {
        return Err(format!(
            "protocol version {} is not supported (this history service speaks \
             {PROTOCOL_VERSION}); run the same agentfs version in fronts and service",
            version.v
        ));
    }
    serde_json::from_slice(body).map_err(|e| format!("malformed hello: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{Caller, Change, Trigger};

    fn path(p: &str) -> RepoPath {
        RepoPath::new(p).unwrap()
    }

    fn hello() -> Hello {
        Hello {
            v: PROTOCOL_VERSION,
            volume: "vol".into(),
            front: FrontInfo {
                origin: Origin {
                    namespace: "ns".into(),
                    pod: "pod-a".into(),
                    pod_uid: "uid".into(),
                    node: "n1".into(),
                    replica: "pod-a".into(),
                },
                slot: 3,
                instance: 42,
                pid: 7,
                version: "0.1.0".into(),
            },
        }
    }

    #[test]
    fn frames_round_trip() {
        let draft = Draft {
            trigger: Trigger::Close,
            caller: Caller {
                uid: 1,
                gid: 2,
                pid: 3,
            },
            time_ms: 9,
            changes: vec![Change::Remove { path: path("a/b") }],
            operation: None,
            atomic: true,
            origin: None,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &hello()).unwrap();
        for r in [
            Request::Tracked {
                paths: vec![path("x")],
            },
            Request::Record {
                draft: Box::new(draft.clone()),
            },
            Request::Ping,
        ] {
            write_frame(&mut buf, &r).unwrap();
        }
        let mut r = &buf[..];
        assert_eq!(
            parse_hello(&read_frame_bytes(&mut r).unwrap()).unwrap(),
            hello()
        );
        assert!(matches!(
            read_frame::<Request>(&mut r).unwrap(),
            Request::Tracked { .. }
        ));
        assert_eq!(
            read_frame::<Request>(&mut r).unwrap(),
            Request::Record {
                draft: Box::new(draft)
            }
        );
        assert_eq!(read_frame::<Request>(&mut r).unwrap(), Request::Ping);
        assert!(matches!(
            read_frame::<Request>(&mut r),
            Err(FrameError::Closed)
        ));

        for reply in [
            Reply::Welcome {
                volume: "v".into(),
                branch: "b".into(),
                service_pid: 1,
            },
            Reply::Tracked {
                tracked: vec![true, false],
            },
            Reply::Recorded {
                seq: Some(4),
                skipped: vec![WireSkip {
                    path: path("p"),
                    reason: WireSkipReason::Untracked,
                }],
            },
            Reply::Pong,
            Reply::Refused { reason: "r".into() },
            Reply::Busy { reason: "b".into() },
            Reply::Error {
                message: "m".into(),
            },
        ] {
            let mut buf = Vec::new();
            write_frame(&mut buf, &reply).unwrap();
            assert_eq!(read_frame::<Reply>(&mut &buf[..]).unwrap(), reply);
        }
    }

    #[test]
    fn bad_frames_are_rejected() {
        let huge = (MAX_FRAME as u32 + 1).to_be_bytes();
        assert!(matches!(
            read_frame::<Reply>(&mut &huge[..]),
            Err(FrameError::TooLarge(_))
        ));
        let truncated = [0, 0, 0, 10, b'{'];
        let err = read_frame::<Reply>(&mut &truncated[..]).unwrap_err();
        assert!(
            matches!(&err, FrameError::Io(e) if e.kind() == io::ErrorKind::UnexpectedEof),
            "{err:?}"
        );
        let short_len = [0, 0];
        assert!(matches!(
            read_frame::<Reply>(&mut &short_len[..]),
            Err(FrameError::Io(_))
        ));
        let mut garbage = 7u32.to_be_bytes().to_vec();
        garbage.extend_from_slice(b"garbage");
        assert!(matches!(
            read_frame::<Reply>(&mut &garbage[..]),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn hello_versions_are_checked_first() {
        let mut v2 = serde_json::to_value(hello()).unwrap();
        v2["v"] = 2.into();
        v2["front"] = serde_json::json!("whatever a v2 front sends");
        let err = parse_hello(&serde_json::to_vec(&v2).unwrap()).unwrap_err();
        assert!(err.contains("protocol version 2 is not supported"), "{err}");
        assert!(parse_hello(b"[]").unwrap_err().contains("malformed"));
    }

    #[test]
    fn skip_reasons_survive_the_wire() {
        let reasons = [
            SkipReason::NeverRecord,
            SkipReason::Ignored {
                source: RuleSource::OperatorExclude,
                pattern: "*.o".into(),
            },
            SkipReason::Ignored {
                source: RuleSource::TreeIgnore(path("d/.gitignore")),
                pattern: "x".into(),
            },
            SkipReason::Nested {
                repo: path("r"),
                kind: NestedKind::GitFile,
            },
            SkipReason::Untracked,
            SkipReason::SharedMemory,
            SkipReason::Gone,
            SkipReason::GroupRefused { member: path("m") },
            SkipReason::Refused(Refusal::GroupMemberSkipped { reason: "r".into() }),
        ];
        for reason in reasons {
            let wire = WireSkipReason::from_reason(&reason).unwrap();
            let json = serde_json::to_string(&wire).unwrap();
            let back: WireSkipReason = serde_json::from_str(&json).unwrap();
            assert_eq!(back.into_reason(), reason, "{json}");
        }
        assert!(
            WireSkipReason::from_reason(&SkipReason::Refused(Refusal::MaxSize {
                bytes: 1,
                limit: 0
            }))
            .is_none()
        );
    }
}
