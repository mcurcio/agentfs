use agentfs_core::tree_reconcile::{Id, LineMap, pieces, render};
use serde::{Deserialize, Serialize};

/// A version: its writer and its place among the writer's versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
struct V(u64, u64);

impl std::fmt::Display for V {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.0, self.1)
    }
}

impl Id for V {
    fn at(writer: u64, index: u64) -> Self {
        Self(writer, index)
    }
    fn writer(&self) -> u64 {
        self.0
    }
    fn index(&self) -> u64 {
        self.1
    }
}

fn save(base: &LineMap<V>, old: &[u8], new: &[u8], writer: u64) -> LineMap<V> {
    let mut last = 0;
    base.align_bytes(old, new, |n| {
        let first = V(writer, last + 1);
        last += n;
        first
    })
    .unwrap()
}

/// The bytes a map's live lines read from `image`.
fn bytes(map: &LineMap<V>, image: &[u8]) -> Vec<u8> {
    let parts = pieces(map, &[(map, &image)]).unwrap();
    render(&[&image], &parts).unwrap()
}

#[test]
fn alignment_retains_identity_and_exact_binary_bytes() {
    let old: &[u8] = b"a\0b\nrepeat\nrepeat\ntail";
    let new: &[u8] = b"a\0b\ninsert\nrepeat\nrepeat\ntail!";
    let root = save(&LineMap::default(), b"", old, 1);
    let edited = save(&root, old, new, 2);
    assert_eq!(bytes(&edited, new), new);
    let (root, edited_lines) = (root.expand().lines, edited.expand().lines);
    assert_eq!(root[0], edited_lines[0]);
    assert_eq!(root[1], edited_lines[2]);
    assert_eq!(root[2], edited_lines[3]);
    assert_eq!(root[3].identity, edited_lines[4].identity);
    assert_eq!(
        edited.expand().ancestors[&edited_lines[4].version],
        Some(root[3].version)
    );
    assert_eq!(edited_lines[4].open, Some(5));
    let decoded = LineMap::decode(&edited.encode().unwrap()).unwrap();
    assert_eq!(decoded, edited);
}

/// Against an empty old image the alignment's hunks are git's, with no child:
/// each input's every line is new, one run.
#[test]
fn an_empty_old_image_aligns_as_git_does() {
    let inputs: [&[u8]; 9] = [
        b"",
        b"x",
        b"x\n",
        b"a\nb\nc\n",
        b"a\nb\nno newline",
        b"\n",
        b"\n\n\n",
        b"a\0b\nbinary\0\n\xff\xfe",
        b"repeat\nrepeat\nrepeat\n",
    ];
    for new in inputs {
        let ours = agentfs_core::tree_reconcile::histogram(b"", new).unwrap();
        let git = agentfs_align::histogram(b"", new).unwrap();
        assert_eq!(ours, git, "{new:?}");
        let map = save(&LineMap::default(), b"", new, 1);
        assert_eq!(bytes(&map, new), new);
        assert!(map.runs().len() <= 1, "{new:?}");
        assert!(map.ancestors().iter().all(|o| o.over.is_none()));
    }
}
