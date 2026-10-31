//! The tmp layer alone, on a temporary directory, as this process's own
//! user (the view owner the daemon is).

use std::os::fd::AsFd;

use super::*;
use crate::overlay::OwnerIds;
use crate::privileges::Privileges;

const O_RDWR: i32 = 2;
const O_TRUNC: i32 = OFlags::TRUNC.bits() as i32;

fn me() -> ViewOwner {
    let ids = OwnerIds {
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
    };
    ViewOwner::new(ids, &Privileges::default(), ids.uid).unwrap()
}

/// A tree and a state area side by side; the layer's store in the state.
struct T {
    dir: tempfile::TempDir,
    tree: OwnedFd,
}

impl T {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tree")).unwrap();
        std::fs::create_dir_all(dir.path().join("state")).unwrap();
        let tree = rustix::fs::open(
            dir.path().join("tree"),
            OFlags::PATH | OFlags::DIRECTORY,
            Mode::empty(),
        )
        .unwrap();
        Self { dir, tree }
    }

    fn store(&self) -> PathBuf {
        self.dir.path().join("state/tmp-layer")
    }

    fn open(&self) -> TmpLayer {
        TmpLayer::open(&self.store(), self.tree.as_fd(), me()).unwrap()
    }

    fn stored(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.store())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}

#[test]
fn open_refuses_another_filesystem() {
    let t = T::new();
    let proc = rustix::fs::open("/proc", OFlags::PATH | OFlags::DIRECTORY, Mode::empty()).unwrap();
    let err = TmpLayer::open(&t.store(), proc.as_fd(), me())
        .err()
        .expect("a store on another filesystem than the tree");
    assert!(
        matches!(err, TmpLayerError::OtherFilesystem { .. }),
        "{err}"
    );
    assert!(err.to_string().contains("one filesystem"), "{err}");
    // On the tree's filesystem it opens.
    t.open();
}

#[test]
fn open_and_wipe_discard_everything() {
    let t = T::new();
    // A crash left a file and a directory behind.
    std::fs::create_dir_all(t.store().join("d")).unwrap();
    std::fs::write(t.store().join("5"), "left").unwrap();
    std::fs::write(t.store().join("d/x"), "left").unwrap();
    let layer = t.open();
    assert!(t.stored().is_empty(), "{:?}", t.stored());
    let meta = std::fs::metadata(t.store()).unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o7777,
        0o700
    );
    assert_eq!(std::os::unix::fs::MetadataExt::uid(&meta), me().uid());

    let (entry, fh, _) = layer.create(1, b"a.tmp", 0o644, O_RDWR, 100, 7).unwrap();
    assert_eq!(entry.id, 100);
    layer.write(fh, 0, b"open").unwrap().unwrap();
    layer.mknod(1, b"b.tmp", 0o600, 101).unwrap();
    assert_eq!(t.stored(), ["100", "101"]);
    assert_eq!(layer.wipe().unwrap(), 2);
    assert!(t.stored().is_empty());
    assert!(!layer.holds(1, b"a.tmp"));
    assert!(layer.lookup(1, b"b.tmp").is_none());
    assert!(layer.entries(1).is_empty());
    // The open handle keeps its (unnamed) file.
    assert_eq!(layer.read(fh, 0, 10).unwrap().unwrap(), b"open");
    assert_eq!(layer.getattr(100, None).unwrap().unwrap().size, 4);
    layer.release(fh).unwrap().unwrap();
    layer.forget(100, 1).unwrap();
    assert!(layer.getattr(100, None).is_none(), "forgotten");
}

#[test]
fn names_nodes_and_handles() {
    let t = T::new();
    let layer = t.open();
    let (entry, fh, handle) = layer.create(1, b"x.tmp", 0o4755, O_RDWR, 100, 7).unwrap();
    assert!(handle.write);
    assert_eq!(entry.attr.perm, 0o755, "never set-id");
    assert_eq!(entry.attr.ino.0, 100);
    assert_eq!(
        layer
            .create(1, b"x.tmp", 0o644, O_RDWR, 101, 8)
            .unwrap_err(),
        Errno::EEXIST
    );
    assert!(layer.handle(8).is_none());
    assert_eq!(layer.write(fh, 0, b"hello").unwrap().unwrap(), 5);
    layer.flush(fh).unwrap().unwrap();
    assert_eq!(layer.lookup(1, b"x.tmp").unwrap().unwrap().id, 100);
    assert_eq!(layer.entries(1).len(), 1);
    assert!(layer.entries(2).is_empty());

    // Renamed within the layer, over another of its files.
    layer.mknod(2, b"y.tmp", 0o644, 102).unwrap();
    assert_eq!(
        layer
            .rename(1, b"x.tmp", 2, b"y.tmp", RenameFlags::NOREPLACE.bits())
            .unwrap()
            .unwrap_err(),
        Errno::EEXIST
    );
    assert_eq!(
        layer
            .rename(1, b"x.tmp", 2, b"y.tmp", RenameFlags::EXCHANGE.bits())
            .unwrap()
            .unwrap_err(),
        Errno::EINVAL
    );
    layer.rename(1, b"x.tmp", 2, b"y.tmp", 0).unwrap().unwrap();
    assert_eq!(t.stored(), ["100"], "the replaced file is gone at once");
    assert!(!layer.holds(1, b"x.tmp"));
    assert!(layer.rename(1, b"x.tmp", 1, b"z", 0).is_none());
    // A node it never had, and one it dropped, are not its.
    layer.forget(102, 1).unwrap();
    assert!(layer.getattr(102, None).is_none());
    assert!(layer.getattr(999, None).is_none());
    assert!(layer.read(999, 0, 1).is_none());

    // Opened again, truncated; setattr; unlinked while open.
    let (fh2, _) = layer.open_node(100, O_RDWR | O_TRUNC, 9).unwrap().unwrap();
    assert_eq!(layer.getattr(100, Some(fh2)).unwrap().unwrap().size, 0);
    let set = SetAttr {
        size: Some(3),
        mode: Some(0o600),
        ..SetAttr::default()
    };
    let attr = layer.setattr(100, &set).unwrap().unwrap();
    assert_eq!((attr.size, attr.perm), (3, 0o600));
    layer.unlink(2, b"y.tmp").unwrap().unwrap();
    assert!(t.stored().is_empty());
    assert!(layer.unlink(2, b"y.tmp").is_none());
    assert_eq!(layer.getattr(100, None).unwrap().unwrap().size, 3);
    layer.release(fh).unwrap().unwrap();
    layer.release(fh2).unwrap().unwrap();
    assert_eq!(
        layer.getattr(100, None).unwrap().unwrap_err(),
        Errno::ENOENT,
        "known to the kernel, but no file left"
    );
    layer.forget(100, 2).unwrap();
    assert!(layer.getattr(100, None).is_none());
}

#[test]
fn copy_file_range_stays_in_the_layer() {
    let t = T::new();
    let layer = t.open();
    let (_, a, _) = layer.create(1, b"a", 0o644, O_RDWR, 100, 7).unwrap();
    let (_, b, _) = layer.create(1, b"b", 0o644, O_RDWR, 101, 8).unwrap();
    layer.write(a, 0, b"copied").unwrap().unwrap();
    assert_eq!(layer.copy_file_range(a, 0, b, 0, 6).unwrap().unwrap(), 6);
    assert_eq!(layer.read(b, 0, 10).unwrap().unwrap(), b"copied");
    assert_eq!(
        layer.copy_file_range(a, 0, 50, 0, 6).unwrap().unwrap_err(),
        Errno::EXDEV
    );
    assert_eq!(
        layer.copy_file_range(50, 0, b, 0, 6).unwrap().unwrap_err(),
        Errno::EXDEV
    );
    assert!(layer.copy_file_range(50, 0, 51, 0, 6).is_none());
}

#[test]
fn move_out_hands_everything_over_or_nothing() {
    let t = T::new();
    let layer = t.open();
    let (_, fh, _) = layer.create(1, b"c.tmp", 0o644, O_RDWR, 100, 7).unwrap();
    layer.lookup(1, b"c.tmp").unwrap().unwrap();
    layer.mapping_may_write(100).unwrap();
    // Refused: nothing changes.
    let refused = layer
        .move_out(1, b"c.tmp", |m| {
            assert_eq!(m.name, b"100");
            assert_eq!(m.moved.node, 100);
            assert_eq!(m.moved.lookups, 2);
            assert!(m.moved.mapping_may_write);
            assert_eq!(m.moved.handles.len(), 1);
            assert_eq!(m.moved.handles[0].0, fh);
            Err(Errno::EACCES)
        })
        .unwrap();
    assert_eq!(refused.unwrap_err(), Errno::EACCES);
    assert!(layer.holds(1, b"c.tmp"));
    assert!(layer.handle(fh).is_some());
    // Moved: out of the store, and the layer forgets it.
    let tree = t.dir.path().join("tree");
    let moved = layer
        .move_out(1, b"c.tmp", |m| {
            let dir =
                rustix::fs::open(&tree, OFlags::PATH | OFlags::DIRECTORY, Mode::empty()).unwrap();
            rustix::fs::renameat(m.store, m.name, &dir, "c").unwrap();
            Ok(Entry {
                id: m.moved.node,
                attr: layer_attr(),
            })
        })
        .unwrap()
        .unwrap();
    assert_eq!(moved.id, 100);
    assert!(t.stored().is_empty());
    assert!(tree.join("c").is_file());
    assert!(!layer.holds(1, b"c.tmp"));
    assert!(layer.getattr(100, None).is_none());
    assert!(layer.handle(fh).is_none());
    assert!(layer.move_out(1, b"c.tmp", |_| unreachable!()).is_none());
}

fn layer_attr() -> FileAttr {
    let st = rustix::fs::stat("/").unwrap();
    attr(100, &st)
}
