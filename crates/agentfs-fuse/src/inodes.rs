//! The inode table: FUSE node ids, the backing inodes behind them, and the
//! names they are known by (design §2.1).
//!
//! Recording is per inode and attributed to the inode's *current* paths, so
//! each node keeps its known links (parent node, name). A path is resolved
//! through the parent chain when it is needed, which makes a directory
//! rename move every path below it at once.
//!
//! - **Node ids** are allocated by agentfs: the root is 1, and each backing
//!   inode (device, inode number) gets the next id the first time it is
//!   seen. The id is kept for that backing inode for the life of the mount,
//!   even after the kernel forgets the node, so `st_ino` (which is the node
//!   id: fuser sends the attribute's inode number as the node id) is stable
//!   while mounted and hard links share it. It is not stable across
//!   remounts. The cost is one map entry per backing inode ever seen.
//! - **Lifetimes** follow the kernel's lookup count: a node is dropped when
//!   the kernel has forgotten it, no handle is open on it, and no other
//!   node's link names it as parent.
//! - **Links** are what this mount has seen: names the kernel looked up or
//!   created. A hard link made before the mount and never looked up is not
//!   known, so a change through one name is not attributed to it.

use std::collections::HashMap;

use crate::dirty::DirtyState;

/// The root's node id (`FUSE_ROOT_ID`).
pub const ROOT: u64 = 1;

/// Path resolution gives up past this many components: a corrupt parent
/// chain (a cycle) must not hang a FUSE thread.
const MAX_DEPTH: usize = 4096;

/// What kind of inode a node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    /// FIFOs, sockets and devices: passed through, never recorded.
    Other,
}

impl Kind {
    pub fn from_mode(mode: u32) -> Self {
        match mode & 0o170000 {
            0o100000 => Self::File,
            0o040000 => Self::Dir,
            0o120000 => Self::Symlink,
            _ => Self::Other,
        }
    }
}

/// One node the kernel knows.
#[derive(Debug, Clone)]
pub struct Node {
    pub dev: u64,
    pub ino: u64,
    pub kind: Kind,
    /// Known names: (parent node, name).
    links: Vec<(u64, Vec<u8>)>,
    /// The kernel's lookup count.
    nlookup: u64,
    /// Open file handles on this node.
    pub handles: Vec<u64>,
    /// How many of [`Node::handles`] were opened for writing.
    pub write_handles: u32,
    pub dirty: DirtyState,
    /// Links of other nodes that name this node as parent.
    children: usize,
}

impl Node {
    fn new(dev: u64, ino: u64, kind: Kind) -> Self {
        Self {
            dev,
            ino,
            kind,
            links: Vec::new(),
            nlookup: 0,
            handles: Vec::new(),
            write_handles: 0,
            dirty: DirtyState::default(),
            children: 0,
        }
    }

    pub fn links(&self) -> &[(u64, Vec<u8>)] {
        &self.links
    }
}

/// The table. Not thread-safe by itself; the FUSE layer keeps it behind one
/// mutex, never held while recording.
#[derive(Debug)]
pub struct InodeTable {
    /// Backing (device, inode) to node id, for the life of the mount.
    ids: HashMap<(u64, u64), u64>,
    nodes: HashMap<u64, Node>,
    /// (parent, name) to the node it names.
    entries: HashMap<(u64, Vec<u8>), u64>,
    next: u64,
}

impl InodeTable {
    /// A table holding only the root, whose backing inode is (`dev`, `ino`).
    pub fn new(dev: u64, ino: u64) -> Self {
        let mut root = Node::new(dev, ino, Kind::Dir);
        // The kernel never forgets the root.
        root.nlookup = 1;
        Self {
            ids: HashMap::from([((dev, ino), ROOT)]),
            nodes: HashMap::from([(ROOT, root)]),
            entries: HashMap::new(),
            next: ROOT + 1,
        }
    }

    /// The node id of backing inode (`dev`, `ino`), allocating one if it
    /// was never seen. Does not create a node (used for `readdir`).
    pub fn id_for(&mut self, dev: u64, ino: u64) -> u64 {
        if let Some(id) = self.ids.get(&(dev, ino)) {
            return *id;
        }
        let id = self.next;
        self.next += 1;
        self.ids.insert((dev, ino), id);
        id
    }

    /// The kernel learned `name` in `parent` names backing inode (`dev`,
    /// `ino`) of kind `kind` (a lookup, create, mkdir, mknod, symlink or
    /// link reply). Counts one kernel lookup and returns the node id.
    pub fn lookup(&mut self, parent: u64, name: &[u8], dev: u64, ino: u64, kind: Kind) -> u64 {
        let id = self.id_for(dev, ino);
        let node = self
            .nodes
            .entry(id)
            .or_insert_with(|| Node::new(dev, ino, kind));
        // A node with no names and no handles whose inode number came back
        // as a different file, or a different kind: the backing filesystem
        // reused the inode number before the kernel forgot the old node.
        if node.kind != kind || (node.links.is_empty() && node.handles.is_empty()) {
            node.kind = kind;
            if node.links.is_empty() && node.handles.is_empty() {
                node.dirty = DirtyState::default();
            }
        }
        node.nlookup += 1;
        self.add_link(parent, name, id);
        id
    }

    /// Records that `name` in `parent` names node `id`. A different node
    /// that held the name loses it.
    pub fn add_link(&mut self, parent: u64, name: &[u8], id: u64) {
        let key = (parent, name.to_vec());
        match self.entries.get(&key) {
            Some(existing) if *existing == id => return,
            Some(_) => {
                self.remove_link(parent, name);
            }
            None => {}
        }
        if !self.nodes.contains_key(&id) || !self.nodes.contains_key(&parent) {
            return;
        }
        self.entries.insert(key, id);
        if let Some(node) = self.nodes.get_mut(&id) {
            node.links.push((parent, name.to_vec()));
        }
        if let Some(p) = self.nodes.get_mut(&parent) {
            p.children += 1;
        }
    }

    /// `name` in `parent` is gone (unlinked, or replaced). Returns the node
    /// that held it.
    pub fn remove_link(&mut self, parent: u64, name: &[u8]) -> Option<u64> {
        let id = self.entries.remove(&(parent, name.to_vec()))?;
        if let Some(node) = self.nodes.get_mut(&id) {
            node.links.retain(|(p, n)| !(*p == parent && n == name));
        }
        if let Some(p) = self.nodes.get_mut(&parent) {
            p.children = p.children.saturating_sub(1);
        }
        self.maybe_drop(id);
        self.maybe_drop(parent);
        Some(id)
    }

    /// The node `name` in `parent` names, if known.
    pub fn child(&self, parent: u64, name: &[u8]) -> Option<u64> {
        self.entries.get(&(parent, name.to_vec())).copied()
    }

    /// Applies a rename. With `exchange` the two names swap nodes;
    /// otherwise a node at the destination loses its name.
    pub fn rename(&mut self, p1: u64, n1: &[u8], p2: u64, n2: &[u8], exchange: bool) {
        let src = self.child(p1, n1);
        let dst = self.child(p2, n2);
        if src.is_some() && src == dst {
            return;
        }
        // Detach both names first, keeping the nodes alive meanwhile.
        let pin = |t: &mut Self, id: Option<u64>| {
            if let Some(node) = id.and_then(|id| t.nodes.get_mut(&id)) {
                node.nlookup += 1;
            }
        };
        let unpin = |t: &mut Self, id: Option<u64>| {
            if let Some(id) = id {
                if let Some(node) = t.nodes.get_mut(&id) {
                    node.nlookup -= 1;
                }
                t.maybe_drop(id);
            }
        };
        pin(self, src);
        pin(self, dst);
        self.nodes.entry(p1).and_modify(|p| p.nlookup += 1);
        self.nodes.entry(p2).and_modify(|p| p.nlookup += 1);
        if src.is_some() {
            self.remove_link(p1, n1);
        }
        if dst.is_some() {
            self.remove_link(p2, n2);
        }
        if let Some(src) = src {
            self.add_link(p2, n2, src);
        }
        if exchange && let Some(dst) = dst {
            self.add_link(p1, n1, dst);
        }
        for p in [p1, p2] {
            if let Some(node) = self.nodes.get_mut(&p) {
                node.nlookup -= 1;
            }
            self.maybe_drop(p);
        }
        unpin(self, src);
        unpin(self, dst);
    }

    /// The kernel forgot `n` lookups of `id`.
    pub fn forget(&mut self, id: u64, n: u64) {
        if let Some(node) = self.nodes.get_mut(&id) {
            node.nlookup = node.nlookup.saturating_sub(n);
        }
        self.maybe_drop(id);
    }

    /// Drops `id` if nothing needs it any more, then any parent that was
    /// kept only for it.
    pub fn maybe_drop(&mut self, id: u64) {
        let mut queue = vec![id];
        while let Some(id) = queue.pop() {
            if id == ROOT {
                continue;
            }
            let Some(node) = self.nodes.get(&id) else {
                continue;
            };
            if node.nlookup > 0 || !node.handles.is_empty() || node.children > 0 {
                continue;
            }
            let Some(node) = self.nodes.remove(&id) else {
                continue;
            };
            for (parent, name) in node.links {
                self.entries.remove(&(parent, name));
                if let Some(p) = self.nodes.get_mut(&parent) {
                    p.children = p.children.saturating_sub(1);
                    queue.push(parent);
                }
            }
        }
    }

    pub fn get(&self, id: u64) -> Option<&Node> {
        self.nodes.get(&id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Node> {
        self.nodes.get_mut(&id)
    }

    /// Ids of every node, for the sweep.
    pub fn ids(&self) -> Vec<u64> {
        self.nodes.keys().copied().collect()
    }

    /// Every current path of `id`, relative to the root; empty for a node
    /// with no known name. The root's only path is empty.
    pub fn paths(&self, id: u64) -> Vec<Vec<u8>> {
        if id == ROOT {
            return vec![Vec::new()];
        }
        let Some(node) = self.nodes.get(&id) else {
            return Vec::new();
        };
        node.links
            .iter()
            .filter_map(|(parent, name)| {
                let mut path = self.dir_path(*parent)?;
                if !path.is_empty() {
                    path.push(b'/');
                }
                path.extend_from_slice(name);
                Some(path)
            })
            .collect()
    }

    /// One current path of `id`: the first that resolves.
    pub fn path(&self, id: u64) -> Option<Vec<u8>> {
        if id == ROOT {
            return Some(Vec::new());
        }
        self.paths(id).into_iter().next()
    }

    /// A directory's path, following its first name up to the root.
    fn dir_path(&self, mut id: u64) -> Option<Vec<u8>> {
        let mut names: Vec<&[u8]> = Vec::new();
        while id != ROOT {
            if names.len() >= MAX_DEPTH {
                return None;
            }
            let (parent, name) = self.nodes.get(&id)?.links.first()?;
            names.push(name);
            id = *parent;
        }
        let mut path = Vec::new();
        for name in names.iter().rev() {
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(name);
        }
        Some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(path: &[u8]) -> &str {
        std::str::from_utf8(path).unwrap()
    }

    fn paths(t: &InodeTable, id: u64) -> Vec<String> {
        let mut out: Vec<String> = t.paths(id).iter().map(|p| s(p).to_owned()).collect();
        out.sort();
        out
    }

    fn table() -> InodeTable {
        InodeTable::new(1, 100)
    }

    #[test]
    fn root_is_one_with_an_empty_path() {
        let t = table();
        assert_eq!(t.path(ROOT), Some(Vec::new()));
        assert_eq!(t.get(ROOT).unwrap().kind, Kind::Dir);
    }

    #[test]
    fn nested_paths_follow_parent_renames() {
        let mut t = table();
        let d = t.lookup(ROOT, b"d", 1, 101, Kind::Dir);
        let e = t.lookup(d, b"e", 1, 102, Kind::Dir);
        let f = t.lookup(e, b"f", 1, 103, Kind::File);
        assert_eq!(paths(&t, f), ["d/e/f"]);
        t.rename(ROOT, b"d", ROOT, b"d2", false);
        assert_eq!(paths(&t, f), ["d2/e/f"]);
        assert_eq!(t.child(ROOT, b"d"), None);
        assert_eq!(t.child(ROOT, b"d2"), Some(d));
    }

    #[test]
    fn hard_links_give_two_paths() {
        let mut t = table();
        let f = t.lookup(ROOT, b"a", 1, 101, Kind::File);
        let same = t.lookup(ROOT, b"b", 1, 101, Kind::File);
        assert_eq!(f, same, "one backing inode, one node");
        assert_eq!(paths(&t, f), ["a", "b"]);
        assert_eq!(t.remove_link(ROOT, b"a"), Some(f));
        assert_eq!(paths(&t, f), ["b"]);
    }

    #[test]
    fn forget_drops_the_node_but_keeps_its_id() {
        let mut t = table();
        let f = t.lookup(ROOT, b"a", 1, 101, Kind::File);
        t.lookup(ROOT, b"a", 1, 101, Kind::File);
        t.forget(f, 1);
        assert!(t.get(f).is_some(), "one lookup left");
        t.get_mut(f).unwrap().handles.push(7);
        t.forget(f, 1);
        assert!(t.get(f).is_some(), "an open handle keeps it");
        t.get_mut(f).unwrap().handles.clear();
        t.maybe_drop(f);
        assert!(t.get(f).is_none());
        assert_eq!(t.child(ROOT, b"a"), None);
        // The same backing inode keeps its id (its st_ino).
        assert_eq!(t.lookup(ROOT, b"a", 1, 101, Kind::File), f);
        assert_ne!(t.lookup(ROOT, b"b", 1, 102, Kind::File), f);
    }

    #[test]
    fn a_parent_lives_while_a_child_names_it() {
        let mut t = table();
        let d = t.lookup(ROOT, b"d", 1, 101, Kind::Dir);
        let f = t.lookup(d, b"f", 1, 102, Kind::File);
        t.forget(d, 1);
        assert!(t.get(d).is_some(), "its child's link pins it");
        assert_eq!(paths(&t, f), ["d/f"]);
        t.forget(f, 1);
        assert!(t.get(f).is_none());
        assert!(t.get(d).is_none(), "released with its last child");
    }

    #[test]
    fn rename_over_a_destination_unnames_it() {
        let mut t = table();
        let a = t.lookup(ROOT, b"a", 1, 101, Kind::File);
        let b = t.lookup(ROOT, b"b", 1, 102, Kind::File);
        t.get_mut(b).unwrap().handles.push(1);
        t.rename(ROOT, b"a", ROOT, b"b", false);
        assert_eq!(paths(&t, a), ["b"]);
        assert!(paths(&t, b).is_empty(), "the old b has no name now");
        assert_eq!(t.child(ROOT, b"b"), Some(a));
    }

    #[test]
    fn rename_exchange_swaps() {
        let mut t = table();
        let a = t.lookup(ROOT, b"a", 1, 101, Kind::File);
        let b = t.lookup(ROOT, b"b", 1, 102, Kind::Dir);
        t.rename(ROOT, b"a", ROOT, b"b", true);
        assert_eq!(paths(&t, a), ["b"]);
        assert_eq!(paths(&t, b), ["a"]);
    }

    #[test]
    fn a_reused_inode_number_starts_clean() {
        let mut t = table();
        let f = t.lookup(ROOT, b"a", 1, 101, Kind::File);
        t.get_mut(f).unwrap().dirty.armed = true;
        t.remove_link(ROOT, b"a");
        // Still known to the kernel (no forget yet), but unnamed: the
        // backing inode number now belongs to a new directory.
        let g = t.lookup(ROOT, b"new", 1, 101, Kind::Dir);
        assert_eq!(g, f);
        let node = t.get(g).unwrap();
        assert_eq!(node.kind, Kind::Dir);
        assert!(!node.dirty.armed);
    }

    #[test]
    fn a_corrupt_chain_does_not_hang() {
        let mut t = table();
        let a = t.lookup(ROOT, b"a", 1, 101, Kind::Dir);
        let b = t.lookup(a, b"b", 1, 102, Kind::Dir);
        // Make a cycle a -> b -> a by hand.
        t.get_mut(a).unwrap().links = vec![(b, b"a".to_vec())];
        let f = t.lookup(b, b"f", 1, 103, Kind::File);
        assert!(t.paths(f).is_empty());
    }

    #[test]
    fn kinds() {
        assert_eq!(Kind::from_mode(0o100644), Kind::File);
        assert_eq!(Kind::from_mode(0o040755), Kind::Dir);
        assert_eq!(Kind::from_mode(0o120777), Kind::Symlink);
        assert_eq!(Kind::from_mode(0o010644), Kind::Other);
    }

    #[test]
    fn readdir_ids_match_lookup_ids() {
        let mut t = table();
        let id = t.id_for(1, 105);
        assert_eq!(t.lookup(ROOT, b"x", 1, 105, Kind::File), id);
    }
}
