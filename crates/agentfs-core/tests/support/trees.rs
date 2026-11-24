//! The tree actor tests' volumes, one line each: a new volume, a reopen, and a
//! writer with an empty pod context.

#![allow(dead_code, reason = "each test file uses what it needs")]

use std::path::Path;

use agentfs_contracts::clients::FileOwner;
use agentfs_contracts::name::{VolumeName, WriterLabel};
use agentfs_contracts::pod::{PodContext, WriterAt};
use agentfs_contracts::volume::{VolumeId, VolumeIdentity};
use agentfs_core::tree_actor::{
    Attrs, ChannelGate, Checkpointed, CompactGate, Gap, Perm, RecordNo, TreeActor, TreeId, Upkeep,
};

/// What a test's create gives a file: mode 0644, owned by 1000:1000.
pub const ATTRS: Attrs = Attrs {
    perm: Perm::from_mode(0o644),
    owner: FileOwner {
        uid: 1000,
        gid: 1000,
    },
};

/// The variable that picks the slow tree tests' volume sizes: `small` or
/// `full` (docs/development.md, "Running tests").
pub const VOLUMES_VAR: &str = "AGENTFS_TEST_VOLUMES";

/// The variable every GitHub Actions runner sets to `true`.
pub const ACTIONS_VAR: &str = "GITHUB_ACTIONS";

/// The volume sizes a slow tree test builds: `Small` locally, `Full` on
/// GitHub Actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Volumes {
    Small,
    Full,
}

impl Volumes {
    /// The sizes this run builds, from [`VOLUMES_VAR`] and [`ACTIONS_VAR`];
    /// panics on a value [`Volumes::parse`] refuses.
    pub fn from_env() -> Volumes {
        let on_actions = std::env::var_os(ACTIONS_VAR).is_some_and(|v| v == "true");
        Volumes::parse(std::env::var(VOLUMES_VAR), on_actions).unwrap_or_else(|e| panic!("{e}"))
    }

    /// The sizes `var`, [`VOLUMES_VAR`]'s value, names. Unset is `Small`
    /// locally and `Full` on GitHub Actions, so no workflow, job or script has
    /// to remember to set it. Any value but `small` or `full`, empty included,
    /// is refused, and so is `small` on Actions: CI never quietly runs the
    /// small sizes.
    pub fn parse(
        var: Result<String, std::env::VarError>,
        on_actions: bool,
    ) -> Result<Volumes, String> {
        match var {
            Err(std::env::VarError::NotPresent) if on_actions => Ok(Volumes::Full),
            Err(std::env::VarError::NotPresent) => Ok(Volumes::Small),
            Ok(v) if v == "full" => Ok(Volumes::Full),
            Ok(v) if v == "small" && on_actions => Err(format!(
                "{VOLUMES_VAR}=small on GitHub Actions: CI runs the full sizes"
            )),
            Ok(v) if v == "small" => Ok(Volumes::Small),
            other => Err(format!("{VOLUMES_VAR} is `small` or `full`, not {other:?}")),
        }
    }

    /// `small` or `full` files, by these sizes.
    pub fn files(self, small: usize, full: usize) -> usize {
        match self {
            Volumes::Small => small,
            Volumes::Full => full,
        }
    }
}

/// The identity [`new_actor`] gives: `test`, with a fixed id.
pub fn identity() -> VolumeIdentity {
    VolumeIdentity {
        name: VolumeName::parse("test").expect("a safe name"),
        id: VolumeId(uuid::Uuid::from_u128(1)),
    }
}

/// A new volume [`identity`], served with an empty pod context.
pub fn new_actor(dir: &Path) -> TreeActor {
    TreeActor::create(dir, &identity(), None, &PodContext::default()).expect("creates")
}

/// A new volume [`identity`] served with `upkeep`.
pub fn new_actor_with(dir: &Path, upkeep: Upkeep) -> TreeActor {
    TreeActor::create_with(dir, &identity(), None, &PodContext::default(), upkeep).expect("creates")
}

/// Opens the volume again, with `upkeep`.
pub fn reopen_with(dir: &Path, upkeep: Upkeep) -> TreeActor {
    TreeActor::open_with(dir, &PodContext::default(), upkeep).expect("reopens")
}

/// A compaction at every built event, never parked: `Upkeep::gated` over a
/// gate whose two ends are dropped.
pub fn compacting() -> Upkeep {
    let (gate, _, _) = ChannelGate::pair();
    Upkeep::gated(gate)
}

/// The checkpoint History counts once one at or past `through` is published
/// and its compaction has ended: what it covers is unlinked.
pub fn compacted_through(v: &TreeActor, through: RecordNo) -> Checkpointed {
    let history = v.history();
    let mut last = history.checkpointed();
    while last.built < through {
        last = history
            .wait_checkpointed(last.generation)
            .expect("a checkpoint is published");
    }
    history
        .wait_compacted(last.generation)
        .expect("its compaction ends");
    last
}

/// A compaction gate that fails at `gap` while armed, passes everywhere
/// else, and never parks.
pub struct FailAt {
    gap: Gap,
    armed: std::sync::atomic::AtomicBool,
    /// Disarms itself at its first failure.
    once: bool,
}

impl FailAt {
    pub fn new(gap: Gap, once: bool) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            gap,
            armed: std::sync::atomic::AtomicBool::new(true),
            once,
        })
    }

    pub fn disarm(&self) {
        self.armed
            .store(false, std::sync::atomic::Ordering::Release);
    }

    pub fn arm(&self) {
        self.armed.store(true, std::sync::atomic::Ordering::Release);
    }
}

impl CompactGate for FailAt {
    fn at(&self, gap: Gap) -> std::io::Result<()> {
        use std::sync::atomic::Ordering;
        if gap != self.gap || !self.armed.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.once {
            self.armed.store(false, Ordering::Release);
        }
        Err(std::io::Error::other("a test's fault"))
    }
}

/// A fork of the root with no row: the engine's own tests, below the store
/// adapter that writes each fork's row (`agentfs_core::store`).
#[allow(
    clippy::disallowed_methods,
    reason = "a tree actor test forks below the store"
)]
pub fn rowless_fork(v: &mut TreeActor) -> TreeId {
    v.fork_with(TreeId::Root, |_, _| Ok(Vec::new()))
        .expect("forks")
}

/// Opens the volume again.
pub fn reopen(dir: &Path) -> TreeActor {
    TreeActor::open(dir, &PodContext::default()).expect("reopens")
}

/// Returns once no process holds the state area's lease. A killed process's
/// tree Git children (the builder's `git fast-import`) share its lease and die
/// of their parent-death signal after it (`tree_git`'s `attach_lease`), so the
/// lease can outlive the process a test waited for. Taking the lease, blocking,
/// is the event that every holder is gone; it is released at once.
pub fn wait_for_lease(dir: &Path) {
    agentfs_core::tree_actor::wait_for_lease(dir).expect("takes the lease");
}

/// [`reopen`] after the process that served the volume was killed.
pub fn reopen_after_kill(dir: &Path) -> TreeActor {
    wait_for_lease(dir);
    reopen(dir)
}

/// `label` with an empty pod context.
pub fn writer(label: &str) -> WriterAt {
    WriterAt {
        writer: WriterLabel::parse(label).expect("a writer"),
        at: PodContext::default(),
    }
}

/// Where a file named `name` sits under `files/`, `lines/` and a checkout:
/// `<aa>/<bb>/<name>`, `aa` and `bb` the first four hex digits of the name's
/// blob id, as `git hash-object --stdin` gives it (docs/trees.md).
pub fn fanned(name: &str) -> String {
    let hex = git2::Oid::hash_object(git2::ObjectType::Blob, name.as_bytes())
        .expect("hashes")
        .to_string();
    format!("{}/{}/{name}", &hex[..2], &hex[2..4])
}

/// Tree `tree`'s checkout of `name` in the state area `dir`.
pub fn checkout(dir: &Path, tree: &str, name: &str) -> std::path::PathBuf {
    dir.join("trees").join(tree).join(fanned(name))
}

/// The journal's segment files in the state area `dir`, in order.
pub fn segments(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(dir.join("journal"))
        .expect("reads the journal")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| path.extension().is_some_and(|e| e == "seg"))
        .collect();
    found.sort();
    found
}

/// The segment the journal appends to: its last.
pub fn current_segment(dir: &Path) -> std::path::PathBuf {
    segments(dir).pop().expect("a segment")
}

/// The journal's carry files in the state area `dir`: live frames a
/// compaction copied out of the segments it let go.
pub fn carries(dir: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir.join("journal"))
        .expect("reads the journal")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("carry-"))
        })
        .collect()
}

/// Each segment's first record, in order: its file name.
pub fn segment_firsts(dir: &Path) -> Vec<u64> {
    segments(dir)
        .iter()
        .map(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse().ok())
                .expect("a segment's name")
        })
        .collect()
}

/// Every segment's and carry file's bytes.
pub fn journal_len(dir: &Path) -> u64 {
    segments(dir)
        .iter()
        .chain(carries(dir).iter())
        .map(|path| std::fs::metadata(path).expect("a journal file").len())
        .sum()
}

/// The disk blocks the journal's segments and carry files hold, in bytes: the linked ones,
/// and the unlinked ones this process still has open (a reader holding a
/// dropped segment keeps its space). Read from each file's allocated blocks.
pub fn journal_blocks(dir: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let journal = std::fs::canonicalize(dir.join("journal")).expect("the journal");
    let linked: u64 = segments(dir)
        .iter()
        .chain(carries(dir).iter())
        .map(|path| std::fs::metadata(path).expect("a journal file").blocks() * 512)
        .sum();
    let mut held = 0;
    for fd in std::fs::read_dir("/proc/self/fd").expect("lists descriptors") {
        let fd = fd.expect("a descriptor").path();
        let Ok(target) = std::fs::read_link(&fd) else {
            continue;
        };
        let target = target.to_string_lossy();
        let Some(path) = target.strip_suffix(" (deleted)") else {
            continue;
        };
        let name = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if Path::new(path).parent() == Some(journal.as_path())
            && (name.ends_with(".seg") || name.starts_with("carry-"))
        {
            // The descriptor itself: its file has no name left to stat.
            if let Ok(meta) = std::fs::metadata(&fd) {
                held += meta.blocks() * 512;
            }
        }
    }
    linked + held
}

/// Where a torn tail is saved.
pub fn torn(dir: &Path) -> std::path::PathBuf {
    dir.join("journal").join("torn")
}
