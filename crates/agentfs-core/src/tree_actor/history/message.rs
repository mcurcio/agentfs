//! Every message a tree-view commit carries, written and read here only
//! (docs/target-architecture/git-model.md §3 and §5, docs/trees.md). A trailer common to every
//! message is one edit in [`Message::render`]'s shared part.

use agentfs_contracts::name::{Label, WriterLabel};
use agentfs_contracts::pod::{PodContext, WriterAt};
use agentfs_contracts::volume::VolumeIdentity;

use crate::tree_actor::journal::RowRecord;
use crate::tree_actor::rows::Chain;
use crate::tree_actor::tree::EventNumber;
use crate::tree_actor::{Error, Result, TreeId, referee};

const WRITER: &str = "Agentfs-Writer: ";
const CAUSE: &str = "Agentfs-Cause: ";
const TREE: &str = "Agentfs-Tree: ";
const EVENT: &str = "Agentfs-Event: ";
const VOLUME: &str = "Agentfs-Volume: ";
const VOLUME_ID: &str = "Agentfs-Volume-Id: ";
const NAMESPACE: &str = "Agentfs-Namespace: ";
const POD: &str = "Agentfs-Pod: ";
const POD_UID: &str = "Agentfs-Pod-Uid: ";
const NODE: &str = "Agentfs-Node: ";
const OPEN: &str = "Agentfs-Open: ";
const COLLISIONS: &str = "Agentfs-Collisions: ";
/// The cause a direct save's commit carries, written by `render` and read by `direct_event`.
const DIRECT: &str = "Direct";
const ENVELOPE: &str = "Agentfs-Format: envelope-1";
const KIND: &str = "Agentfs-Kind: ";
const BY: &str = "Agentfs-By: ";
/// An event commit's format: no file-commit reader takes one for a save.
const ROW_FORMAT: &str = "Agentfs-Format: row-1";

/// A direct save's subject word: the call that saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) enum Action {
    Create,
    Rename,
    Unlink,
    /// A write-open's close.
    Direct,
}

impl Action {
    fn word(self) -> &'static str {
        match self {
            Self::Create => "Create",
            Self::Rename => "Rename",
            Self::Unlink => "Unlink",
            Self::Direct => "Direct",
        }
    }
}

/// One tree-view commit's message.
pub(in crate::tree_actor) enum Message<'a> {
    /// Writer `agentfs` with the history service's pod context, tree `root`, event 0, and no
    /// `Agentfs-Format`: its absence marks the Root commit.
    Root { service: &'a PodContext },
    /// "{action} by {writer}"; `Agentfs-Cause: Direct`; `Agentfs-Open` when the save closed an
    /// open.
    Direct {
        action: Action,
        by: &'a WriterAt,
        tree: TreeId,
        event: EventNumber,
        open: Option<u64>,
    },
    /// "Landing by {writer}": `landed` is the queued commit's writer and pod context, read by
    /// [`writer_at`] (git-model §3). `Agentfs-Collisions` when any.
    Landing {
        landed: &'a WriterAt,
        tree: TreeId,
        event: EventNumber,
        collisions: &'a [referee::Collision],
    },
    /// Writer `agentfs` with the history service's pod context.
    Sync {
        service: &'a PodContext,
        tree: TreeId,
        event: EventNumber,
    },
    /// An event commit on a row chain: "{kind} {detail}"; `Agentfs-Kind`,
    /// `Agentfs-By`, the chain's tree (none for the volume's), the step, the
    /// volume, the principal's pod context, `Agentfs-Format: row-1`. No
    /// `Agentfs-Writer` or `Agentfs-Cause`: no file-commit reader takes it
    /// for a save.
    Row {
        row: &'a RowRecord,
        chain: Chain,
        event: EventNumber,
    },
}

/// Each pod trailer's key and its field, in trailer order.
fn pod_fields(at: &PodContext) -> [(&'static str, Option<&Label>); 4] {
    [
        (NAMESPACE, at.namespace.as_ref()),
        (POD, at.pod.as_ref()),
        (POD_UID, at.pod_uid.as_ref()),
        (NODE, at.node.as_ref()),
    ]
}

impl Message<'_> {
    /// The message's bytes: subject, blank line, Writer, Cause, Tree, Event, Volume, Volume-Id,
    /// each present pod field, Format, then Open or Collisions, each line ending in '\n'.
    ///
    /// # Errors
    ///
    /// The collisions' JSON only.
    pub(in crate::tree_actor) fn render(&self, volume: &VolumeIdentity) -> Result<String> {
        let agentfs = WriterLabel::agentfs();
        let (subject, writer, at, cause, tree, event) = match self {
            Self::Root { service } => (
                "Root".to_owned(),
                &agentfs,
                *service,
                "Root",
                TreeId::Root,
                None,
            ),
            Self::Direct {
                action,
                by,
                tree,
                event,
                ..
            } => (
                format!("{} by {}", action.word(), by.writer),
                &by.writer,
                &by.at,
                DIRECT,
                *tree,
                Some(*event),
            ),
            Self::Landing {
                landed,
                tree,
                event,
                ..
            } => (
                format!("Landing by {}", landed.writer),
                &landed.writer,
                &landed.at,
                "Landing",
                *tree,
                Some(*event),
            ),
            Self::Sync {
                service,
                tree,
                event,
            } => (
                "Sync".to_owned(),
                &agentfs,
                *service,
                "Sync",
                *tree,
                Some(*event),
            ),
            Self::Row { row, chain, event } => {
                return Ok(render_row(row, *chain, *event, volume));
            }
        };
        let mut message = format!(
            "{subject}\n\n{WRITER}{writer}\n{CAUSE}{cause}\n{TREE}{tree}\n{EVENT}{}\n",
            event.map_or(0, EventNumber::get),
        );
        push_volume_and_pod(&mut message, volume, at);
        if event.is_some() {
            message.push_str(ENVELOPE);
            message.push('\n');
        }
        match self {
            Self::Direct {
                open: Some(open), ..
            } => message.push_str(&format!("{OPEN}{open}\n")),
            Self::Landing { collisions, .. } if !collisions.is_empty() => message.push_str(
                &format!("{COLLISIONS}{}\n", serde_json::to_string(collisions)?),
            ),
            _ => {}
        }
        Ok(message)
    }
}

/// The trailers every message shares after its event: the volume, its id, and
/// each present pod field.
fn push_volume_and_pod(message: &mut String, volume: &VolumeIdentity, at: &PodContext) {
    message.push_str(&format!(
        "{VOLUME}{}\n{VOLUME_ID}{}\n",
        volume.name,
        volume.id.0.hyphenated()
    ));
    for (key, value) in pod_fields(at) {
        if let Some(value) = value {
            message.push_str(&format!("{key}{value}\n"));
        }
    }
}

/// An event commit's message (git-model §4): subject, Kind, By, the chain's
/// tree, Event, the shared trailers, then the row format.
fn render_row(
    row: &RowRecord,
    chain: Chain,
    event: EventNumber,
    volume: &VolumeIdentity,
) -> String {
    let subject = if row.detail.is_empty() {
        row.kind.clone()
    } else {
        format!("{} {}", row.kind, row.detail)
    };
    let mut message = format!("{subject}\n\n{KIND}{}\n{BY}{}\n", row.kind, row.by);
    if let Some(tree) = chain.tree() {
        message.push_str(&format!("{TREE}{tree}\n"));
    }
    message.push_str(&format!("{EVENT}{event}\n"));
    push_volume_and_pod(&mut message, volume, &row.at);
    message.push_str(ROW_FORMAT);
    message.push('\n');
    message
}

fn lines(message: &[u8]) -> Option<std::str::Lines<'_>> {
    std::str::from_utf8(message).ok().map(str::lines)
}

/// `Agentfs-Tree`'s first line: None if absent or the message is not UTF-8.
///
/// # Errors
///
/// [`crate::tree_actor::Error::Format`] if it names no tree.
pub(in crate::tree_actor) fn tree_of(message: &[u8]) -> Result<Option<TreeId>> {
    lines(message)
        .and_then(|mut lines| lines.find_map(|l| l.strip_prefix(TREE)))
        .map(|text| Ok(text.parse()?))
        .transpose()
}

/// The first `Agentfs-Writer` and the first of each pod trailer: `None` if no writer trailer or
/// the message is not UTF-8.
///
/// # Errors
///
/// [`Error::Format`] for a writer or pod value that does not parse.
pub(in crate::tree_actor) fn writer_at(message: &[u8]) -> Result<Option<WriterAt>> {
    let Some(all) = lines(message) else {
        return Ok(None);
    };
    let first = |key: &str| all.clone().find_map(|line| line.strip_prefix(key));
    let Some(writer) = first(WRITER) else {
        return Ok(None);
    };
    let label = |key: &str| {
        first(key)
            .map(|text| Label::parse(text).map_err(|_| Error::Format))
            .transpose()
    };
    Ok(Some(WriterAt {
        writer: WriterLabel::parse(writer).map_err(|_| Error::Format)?,
        at: PodContext {
            namespace: label(NAMESPACE)?,
            pod: label(POD)?,
            pod_uid: label(POD_UID)?,
            node: label(NODE)?,
        },
    }))
}

/// The event of an `Agentfs-Cause: Direct` commit (its first `Agentfs-Event`); None for any
/// other, or when that event is no number.
pub(in crate::tree_actor) fn direct_event(message: &[u8]) -> Option<EventNumber> {
    let mut all = lines(message)?;
    if !all
        .clone()
        .any(|line| line.strip_prefix(CAUSE) == Some(DIRECT))
    {
        return None;
    }
    all.find_map(|line| line.strip_prefix(EVENT))
        .and_then(|s| s.parse::<u64>().ok())
        .map(EventNumber::new)
}

/// Every `Agentfs-Collisions` line's entries, in order; empty if none or not UTF-8.
///
/// # Errors
///
/// A line whose JSON is no list of collisions.
pub(in crate::tree_actor) fn collisions(message: &[u8]) -> Result<Vec<referee::Collision>> {
    let mut all = Vec::new();
    for json in lines(message)
        .into_iter()
        .flatten()
        .filter_map(|line| line.strip_prefix(COLLISIONS))
    {
        all.extend(serde_json::from_str::<Vec<referee::Collision>>(json)?);
    }
    Ok(all)
}

/// Whether the commit is an envelope (`Agentfs-Format: envelope-1`): every commit but the Root.
pub(in crate::tree_actor) fn is_envelope(message: &[u8]) -> bool {
    lines(message).is_some_and(|mut lines| lines.any(|line| line == ENVELOPE))
}

#[cfg(test)]
mod tests {
    use agentfs_contracts::name::VolumeName;
    use agentfs_contracts::volume::VolumeId;

    use super::*;
    use crate::tree_actor::referee::{Collision, CollisionKind, Version};
    use crate::tree_actor::tree::MergeFileIdentity;

    const VOLUME_TEXT: &str =
        "Agentfs-Volume: t\nAgentfs-Volume-Id: 00000000-0000-0000-0000-00000000002a\n";
    const POD_TEXT: &str =
        "Agentfs-Namespace: ns\nAgentfs-Pod: p-1\nAgentfs-Pod-Uid: u-1\nAgentfs-Node: n1\n";

    fn volume() -> VolumeIdentity {
        VolumeIdentity {
            name: VolumeName::parse("t").unwrap(),
            id: VolumeId(uuid::Uuid::from_u128(42)),
        }
    }

    fn pod() -> PodContext {
        let label = |text| Some(Label::parse(text).unwrap());
        PodContext {
            namespace: label("ns"),
            pod: label("p-1"),
            pod_uid: label("u-1"),
            node: label("n1"),
        }
    }

    fn by(writer: &str, at: PodContext) -> WriterAt {
        WriterAt {
            writer: WriterLabel::parse(writer).unwrap(),
            at,
        }
    }

    fn root_text() -> String {
        format!(
            "Root\n\nAgentfs-Writer: agentfs\nAgentfs-Cause: Root\nAgentfs-Tree: root\nAgentfs-Event: 0\n{VOLUME_TEXT}"
        )
    }

    fn render(message: &Message<'_>) -> String {
        message.render(&volume()).unwrap()
    }

    fn collision() -> Collision {
        Collision {
            lost: Version::Name {
                file: MergeFileIdentity(3),
                version: EventNumber::new(9),
            },
            winner: Version::Line {
                file: MergeFileIdentity(3),
                version: "10:2".parse().unwrap(),
            },
            kind: CollisionKind::Line,
        }
    }

    fn direct(open: Option<u64>) -> String {
        render(&Message::Direct {
            action: Action::Rename,
            by: &by("w x", PodContext::default()),
            tree: TreeId::Fork(7),
            event: EventNumber::new(42),
            open,
        })
    }

    #[test]
    fn each_message_renders_its_literal_bytes() {
        assert_eq!(
            render(&Message::Root {
                service: &PodContext::default()
            }),
            root_text()
        );
        assert_eq!(
            render(&Message::Root { service: &pod() }),
            format!("{}{POD_TEXT}", root_text())
        );
        assert_eq!(
            direct(None),
            format!(
                "Rename by w x\n\nAgentfs-Writer: w x\nAgentfs-Cause: Direct\nAgentfs-Tree: fork/7\nAgentfs-Event: 42\n{VOLUME_TEXT}Agentfs-Format: envelope-1\n"
            )
        );
        assert_eq!(
            direct(Some(5)),
            format!(
                "Rename by w x\n\nAgentfs-Writer: w x\nAgentfs-Cause: Direct\nAgentfs-Tree: fork/7\nAgentfs-Event: 42\n{VOLUME_TEXT}Agentfs-Format: envelope-1\nAgentfs-Open: 5\n"
            )
        );
        let partial = PodContext {
            pod: Some(Label::parse("p-1").unwrap()),
            ..PodContext::default()
        };
        assert_eq!(
            render(&Message::Direct {
                action: Action::Direct,
                by: &by("w", partial),
                tree: TreeId::Root,
                event: EventNumber::new(3),
                open: Some(2),
            }),
            format!(
                "Direct by w\n\nAgentfs-Writer: w\nAgentfs-Cause: Direct\nAgentfs-Tree: root\nAgentfs-Event: 3\n{VOLUME_TEXT}Agentfs-Pod: p-1\nAgentfs-Format: envelope-1\nAgentfs-Open: 2\n"
            )
        );
        for (action, word) in [
            (Action::Create, "Create"),
            (Action::Rename, "Rename"),
            (Action::Unlink, "Unlink"),
            (Action::Direct, "Direct"),
        ] {
            let text = render(&Message::Direct {
                action,
                by: &by("w", PodContext::default()),
                tree: TreeId::Root,
                event: EventNumber::new(1),
                open: None,
            });
            assert!(text.starts_with(&format!("{word} by w\n\n")), "{text}");
        }
        let landing = |at: PodContext, collisions: &[Collision]| {
            render(&Message::Landing {
                landed: &by("c3", at),
                tree: TreeId::Root,
                event: EventNumber::new(8),
                collisions,
            })
        };
        assert_eq!(
            landing(PodContext::default(), &[]),
            format!(
                "Landing by c3\n\nAgentfs-Writer: c3\nAgentfs-Cause: Landing\nAgentfs-Tree: root\nAgentfs-Event: 8\n{VOLUME_TEXT}Agentfs-Format: envelope-1\n"
            )
        );
        assert_eq!(
            landing(pod(), &[collision()]),
            format!(
                "Landing by c3\n\nAgentfs-Writer: c3\nAgentfs-Cause: Landing\nAgentfs-Tree: root\nAgentfs-Event: 8\n{VOLUME_TEXT}{POD_TEXT}Agentfs-Format: envelope-1\nAgentfs-Collisions: [{{\"lost\":{{\"Name\":{{\"file\":3,\"version\":9}}}},\"winner\":{{\"Line\":{{\"file\":3,\"version\":\"10:2\"}}}},\"kind\":\"Line\"}}]\n"
            )
        );
        for (service, pods) in [(PodContext::default(), ""), (pod(), POD_TEXT)] {
            assert_eq!(
                render(&Message::Sync {
                    service: &service,
                    tree: TreeId::Fork(70),
                    event: EventNumber::new(11),
                }),
                format!(
                    "Sync\n\nAgentfs-Writer: agentfs\nAgentfs-Cause: Sync\nAgentfs-Tree: fork/70\nAgentfs-Event: 11\n{VOLUME_TEXT}{pods}Agentfs-Format: envelope-1\n"
                )
            );
        }
    }

    #[test]
    fn a_row_message_is_its_literal_text() {
        let row = |detail: &str, at: PodContext| RowRecord {
            chain: String::new(),
            row: "{}".to_owned(),
            kind: "ForkCreated".to_owned(),
            detail: detail.to_owned(),
            by: "launcher cli".to_owned(),
            at,
        };
        let fork = row("", PodContext::default());
        assert_eq!(
            render(&Message::Row {
                row: &fork,
                chain: Chain::Trees(agentfs_contracts::tree::ForkId(7)),
                event: EventNumber::new(7),
            }),
            format!(
                "ForkCreated\n\nAgentfs-Kind: ForkCreated\nAgentfs-By: launcher cli\nAgentfs-Tree: fork/7\nAgentfs-Event: 7\n{VOLUME_TEXT}Agentfs-Format: row-1\n"
            )
        );
        let detailed = row("of root", pod());
        assert_eq!(
            render(&Message::Row {
                row: &detailed,
                chain: Chain::Volume,
                event: EventNumber::new(12),
            }),
            format!(
                "ForkCreated of root\n\nAgentfs-Kind: ForkCreated\nAgentfs-By: launcher cli\nAgentfs-Event: 12\n{VOLUME_TEXT}{POD_TEXT}Agentfs-Format: row-1\n"
            )
        );
        let text = render(&Message::Row {
            row: &fork,
            chain: Chain::Clients(TreeId::Root),
            event: EventNumber::new(3),
        });
        assert!(text.contains("\nAgentfs-Tree: root\n"), "{text}");
        // No file-commit reader takes an event commit for a save.
        assert_eq!(writer_at(text.as_bytes()).unwrap(), None);
        assert_eq!(direct_event(text.as_bytes()), None);
        assert!(!is_envelope(text.as_bytes()));
    }

    #[test]
    fn writer_at_reads_back_what_render_writes() {
        let partial = PodContext {
            node: Some(Label::parse("n1").unwrap()),
            ..PodContext::default()
        };
        for at in [PodContext::default(), pod(), partial] {
            let writer = by("w x", at);
            let text = render(&Message::Direct {
                action: Action::Direct,
                by: &writer,
                tree: TreeId::Fork(1),
                event: EventNumber::new(2),
                open: None,
            });
            assert_eq!(writer_at(text.as_bytes()).unwrap(), Some(writer.clone()));
            let landed = render(&Message::Landing {
                landed: &writer,
                tree: TreeId::Root,
                event: EventNumber::new(3),
                collisions: &[],
            });
            assert_eq!(writer_at(landed.as_bytes()).unwrap(), Some(writer));
        }
        assert_eq!(
            writer_at(render(&Message::Root { service: &pod() }).as_bytes()).unwrap(),
            Some(by("agentfs", pod()))
        );
        assert_eq!(writer_at(b"no trailer\n").unwrap(), None);
        assert_eq!(writer_at(b"\xff\nAgentfs-Writer: w\n").unwrap(), None);
        assert!(matches!(
            writer_at(b"x\n\nAgentfs-Writer: w\nAgentfs-Pod: a b\n"),
            Err(Error::Format)
        ));
        assert!(matches!(
            writer_at(format!("x\n\nAgentfs-Writer: {}\n", "w".repeat(257)).as_bytes()),
            Err(Error::Format)
        ));
    }

    #[test]
    fn tree_of_reads_the_first_tree_trailer() {
        assert_eq!(tree_of(root_text().as_bytes()).unwrap(), Some(TreeId::Root));
        assert_eq!(
            tree_of(direct(None).as_bytes()).unwrap(),
            Some(TreeId::Fork(7))
        );
        assert_eq!(
            tree_of(b"x\n\nAgentfs-Tree: fork/1\nAgentfs-Tree: fork/2\n").unwrap(),
            Some(TreeId::Fork(1))
        );
        assert_eq!(tree_of(b"no trailer\n").unwrap(), None);
        assert_eq!(tree_of(b"\xff\nAgentfs-Tree: root\n").unwrap(), None);
        assert!(matches!(
            tree_of(b"Agentfs-Tree: main\n"),
            Err(Error::Format)
        ));
    }

    #[test]
    fn direct_event_reads_a_direct_saves_event_only() {
        assert_eq!(
            direct_event(direct(Some(5)).as_bytes()),
            Some(EventNumber::new(42))
        );
        assert_eq!(direct_event(root_text().as_bytes()), None);
        let landing = render(&Message::Landing {
            landed: &by("w", PodContext::default()),
            tree: TreeId::Root,
            event: EventNumber::new(8),
            collisions: &[],
        });
        assert_eq!(direct_event(landing.as_bytes()), None);
        let sync = render(&Message::Sync {
            service: &PodContext::default(),
            tree: TreeId::Fork(7),
            event: EventNumber::new(8),
        });
        assert_eq!(direct_event(sync.as_bytes()), None);
        assert_eq!(
            direct_event(b"Agentfs-Cause: Direct\nAgentfs-Event: 3\nAgentfs-Event: 4\n"),
            Some(EventNumber::new(3))
        );
        assert_eq!(
            direct_event(b"Agentfs-Cause: Direct\nAgentfs-Event: x\n"),
            None
        );
        assert_eq!(
            direct_event(b"\xff\nAgentfs-Cause: Direct\nAgentfs-Event: 3\n"),
            None
        );
    }

    #[test]
    fn collisions_reads_every_collisions_line_in_order() {
        let one = render(&Message::Landing {
            landed: &by("w", PodContext::default()),
            tree: TreeId::Root,
            event: EventNumber::new(8),
            collisions: &[collision()],
        });
        assert_eq!(collisions(one.as_bytes()).unwrap(), [collision()]);
        let mut second = collision();
        second.kind = CollisionKind::Whole;
        let two = format!(
            "{one}{COLLISIONS}{}\n",
            serde_json::to_string(&[second.clone()]).unwrap()
        );
        assert_eq!(collisions(two.as_bytes()).unwrap(), [collision(), second]);
        assert!(collisions(direct(None).as_bytes()).unwrap().is_empty());
        assert!(
            collisions(b"\xff\nAgentfs-Collisions: nonsense\n")
                .unwrap()
                .is_empty()
        );
        assert!(collisions(b"Agentfs-Collisions: nonsense\n").is_err());
    }

    #[test]
    fn every_message_but_the_roots_is_an_envelope() {
        assert!(!is_envelope(root_text().as_bytes()));
        assert!(is_envelope(direct(None).as_bytes()));
        assert!(!is_envelope(b"\xff\nAgentfs-Format: envelope-1\n"));
        assert!(!is_envelope(b"x\n\nAgentfs-Format: envelope-1 \n"));
    }
}
