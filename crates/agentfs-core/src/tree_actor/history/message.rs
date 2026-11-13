//! Every message a tree-view commit carries, written and read here only
//! (docs/target-architecture/git-model.md §5). A trailer common to every message is one edit in
//! [`Message::render`]'s shared part.

use crate::tree_actor::journal::parse_tree;
use crate::tree_actor::tree::EventNumber;
use crate::tree_actor::{Result, TreeId, referee};

const TREE: &str = "Agentfs-Tree: ";
const EVENT: &str = "Agentfs-Event: ";
const COLLISIONS: &str = "Agentfs-Collisions: ";
const DIRECT: &str = "Agentfs-Cause: Direct";
const ENVELOPE: &str = "Agentfs-Format: envelope-1";

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
    /// Tree `root`, event 0, and no `Agentfs-Format`: its absence marks the Root commit.
    Root,
    /// "{action} by {writer}"; `Agentfs-Cause: Direct`; `Agentfs-Open` when the save closed an
    /// open.
    Direct {
        action: Action,
        writer: &'a str,
        tree: TreeId,
        event: EventNumber,
        open: Option<u64>,
    },
    /// `Agentfs-Collisions` when any.
    Landing {
        tree: TreeId,
        event: EventNumber,
        collisions: &'a [referee::Collision],
    },
    Sync {
        tree: TreeId,
        event: EventNumber,
    },
}

impl Message<'_> {
    /// The message's bytes: subject, blank line, Writer, Cause, Tree, Event, Format, then Open
    /// or Collisions, each line ending in '\n'.
    ///
    /// # Errors
    ///
    /// The collisions' JSON only.
    pub(in crate::tree_actor) fn render(&self) -> Result<String> {
        let (subject, writer, cause, tree, event) = match self {
            Self::Root => ("Root".to_owned(), "agentfs", "Root", TreeId::Root, None),
            Self::Direct {
                action,
                writer,
                tree,
                event,
                ..
            } => (
                format!("{} by {writer}", action.word()),
                *writer,
                "Direct",
                *tree,
                Some(*event),
            ),
            Self::Landing { tree, event, .. } => (
                "Landing".to_owned(),
                "agentfs",
                "Landing",
                *tree,
                Some(*event),
            ),
            Self::Sync { tree, event } => {
                ("Sync".to_owned(), "agentfs", "Sync", *tree, Some(*event))
            }
        };
        let mut message = format!(
            "{subject}\n\nAgentfs-Writer: {writer}\nAgentfs-Cause: {cause}\n{TREE}{tree}\n{EVENT}{}\n",
            event.map_or(0, EventNumber::get)
        );
        if event.is_some() {
            message.push_str(ENVELOPE);
            message.push('\n');
        }
        match self {
            Self::Direct {
                open: Some(open), ..
            } => message.push_str(&format!("Agentfs-Open: {open}\n")),
            Self::Landing { collisions, .. } if !collisions.is_empty() => message.push_str(
                &format!("{COLLISIONS}{}\n", serde_json::to_string(collisions)?),
            ),
            _ => {}
        }
        Ok(message)
    }
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
        .map(parse_tree)
        .transpose()
}

/// The event of an `Agentfs-Cause: Direct` commit (its first `Agentfs-Event`); None for any
/// other, or when that event is no number.
pub(in crate::tree_actor) fn direct_event(message: &[u8]) -> Option<EventNumber> {
    let mut all = lines(message)?;
    if !all.clone().any(|line| line == DIRECT) {
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
    use super::*;
    use crate::tree_actor::Error;
    use crate::tree_actor::referee::{Collision, CollisionKind, Version};
    use crate::tree_actor::tree::MergeFileIdentity;

    const ROOT_TEXT: &str = "Root\n\nAgentfs-Writer: agentfs\nAgentfs-Cause: Root\nAgentfs-Tree: root\nAgentfs-Event: 0\n";

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
        Message::Direct {
            action: Action::Rename,
            writer: "w x",
            tree: TreeId::Fork(7),
            event: EventNumber::new(42),
            open,
        }
        .render()
        .unwrap()
    }

    #[test]
    fn each_message_renders_its_literal_bytes() {
        assert_eq!(Message::Root.render().unwrap(), ROOT_TEXT);
        assert_eq!(
            direct(None),
            "Rename by w x\n\nAgentfs-Writer: w x\nAgentfs-Cause: Direct\nAgentfs-Tree: fork/7\nAgentfs-Event: 42\nAgentfs-Format: envelope-1\n"
        );
        assert_eq!(
            direct(Some(5)),
            "Rename by w x\n\nAgentfs-Writer: w x\nAgentfs-Cause: Direct\nAgentfs-Tree: fork/7\nAgentfs-Event: 42\nAgentfs-Format: envelope-1\nAgentfs-Open: 5\n"
        );
        for (action, word) in [
            (Action::Create, "Create"),
            (Action::Rename, "Rename"),
            (Action::Unlink, "Unlink"),
            (Action::Direct, "Direct"),
        ] {
            let text = Message::Direct {
                action,
                writer: "w",
                tree: TreeId::Root,
                event: EventNumber::new(1),
                open: None,
            }
            .render()
            .unwrap();
            assert!(text.starts_with(&format!("{word} by w\n\n")), "{text}");
        }
        let landing = |collisions: &[Collision]| {
            Message::Landing {
                tree: TreeId::Root,
                event: EventNumber::new(8),
                collisions,
            }
            .render()
            .unwrap()
        };
        assert_eq!(
            landing(&[]),
            "Landing\n\nAgentfs-Writer: agentfs\nAgentfs-Cause: Landing\nAgentfs-Tree: root\nAgentfs-Event: 8\nAgentfs-Format: envelope-1\n"
        );
        assert_eq!(
            landing(&[collision()]),
            "Landing\n\nAgentfs-Writer: agentfs\nAgentfs-Cause: Landing\nAgentfs-Tree: root\nAgentfs-Event: 8\nAgentfs-Format: envelope-1\nAgentfs-Collisions: [{\"lost\":{\"Name\":{\"file\":3,\"version\":9}},\"winner\":{\"Line\":{\"file\":3,\"version\":\"10:2\"}},\"kind\":\"Line\"}]\n"
        );
        assert_eq!(
            Message::Sync {
                tree: TreeId::Fork(70),
                event: EventNumber::new(11),
            }
            .render()
            .unwrap(),
            "Sync\n\nAgentfs-Writer: agentfs\nAgentfs-Cause: Sync\nAgentfs-Tree: fork/70\nAgentfs-Event: 11\nAgentfs-Format: envelope-1\n"
        );
    }

    #[test]
    fn tree_of_reads_the_first_tree_trailer() {
        assert_eq!(tree_of(ROOT_TEXT.as_bytes()).unwrap(), Some(TreeId::Root));
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
        assert_eq!(direct_event(ROOT_TEXT.as_bytes()), None);
        let landing = Message::Landing {
            tree: TreeId::Root,
            event: EventNumber::new(8),
            collisions: &[],
        }
        .render()
        .unwrap();
        assert_eq!(direct_event(landing.as_bytes()), None);
        let sync = Message::Sync {
            tree: TreeId::Fork(7),
            event: EventNumber::new(8),
        }
        .render()
        .unwrap();
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
        let one = Message::Landing {
            tree: TreeId::Root,
            event: EventNumber::new(8),
            collisions: &[collision()],
        }
        .render()
        .unwrap();
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
        assert!(!is_envelope(ROOT_TEXT.as_bytes()));
        assert!(is_envelope(direct(None).as_bytes()));
        assert!(!is_envelope(b"\xff\nAgentfs-Format: envelope-1\n"));
        assert!(!is_envelope(b"x\n\nAgentfs-Format: envelope-1 \n"));
    }
}
