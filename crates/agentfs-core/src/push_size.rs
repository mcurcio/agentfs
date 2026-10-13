//! Measuring a push before it is sent (design §11.3,
//! `remote.<name>.agentfsMaxPushSize`).
//!
//! A remote with a maximum push size has the git pack of every attempt
//! measured before agentfs connects: libgit2's pack builder packs the
//! commits the remote lacks (the walk from the head, hiding the base: the
//! branch the remote advertises, see `RemotePusher`) and the bytes it writes are counted, stopping as soon as the
//! count passes the limit, so a huge backlog is never written out in full.
//!
//! When the whole backlog does not fit, [`largest_step`] finds the longest
//! prefix of the first-parent chain from the last pushed commit that does,
//! so a backlog that grew during an outage goes out in steps instead of
//! never. A single commit over the limit cannot be stepped: the push fails
//! as `too_large`.
//!
//! Only the git pack is measured: LFS objects go to their own servers
//! (design §7). The pack libgit2 then sends can differ slightly from the
//! measured one, since libgit2 negotiates against every ref the remote
//! advertises, not the branch alone.

use git2::{ErrorCode, Oid, Repository};

/// What [`measure`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measured {
    /// The pack fits, at this many bytes.
    Fits(u64),
    /// The pack is larger than the limit; measuring stopped once this many
    /// bytes (more than the limit) had been counted.
    Over(u64),
}

/// How much of a backlog one push may send (see [`largest_step`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Everything fits.
    All,
    /// Only up to `commit`, a first-parent ancestor of the head; that many
    /// commits of the first-parent chain are left for later steps.
    UpTo { commit: Oid, commits_left: usize },
    /// Not even the first commit of the chain fits (`first`, whose pack
    /// reached at least `size_at_least` bytes), or the backlog cannot be
    /// stepped because the last pushed commit is not an ancestor of the
    /// head.
    None { first: Oid, size_at_least: u64 },
}

/// The size of the pack holding every object reachable from `tips` and
/// not from `base`, up to `limit`: counting stops once it is passed.
/// A `base` the repository does not have is ignored (everything counts).
pub fn measure(
    repo: &Repository,
    tips: &[Oid],
    base: Option<Oid>,
    limit: u64,
) -> Result<Measured, git2::Error> {
    let mut walk = repo.revwalk()?;
    for tip in tips {
        walk.push(*tip)?;
    }
    if let Some(base) = base
        && repo.odb()?.exists(base)
    {
        walk.hide(base)?;
    }
    let mut builder = repo.packbuilder()?;
    builder.set_threads(1);
    builder.insert_walk(&mut walk)?;
    let mut total = 0u64;
    let result = builder.foreach(|chunk| {
        total = total.saturating_add(chunk.len() as u64);
        total <= limit
    });
    match result {
        Ok(()) if total <= limit => Ok(Measured::Fits(total)),
        Ok(()) => Ok(Measured::Over(total)),
        // The callback stopped the walk.
        Err(e) if e.code() == ErrorCode::User || total > limit => Ok(Measured::Over(total)),
        Err(e) => Err(e),
    }
}

/// How far a push from `base` towards `head` may go within `limit`: all of
/// it if it fits, else the longest prefix of the first-parent chain from
/// `base` (exclusive) to `head` that fits, found by bisection (pack size
/// grows with the prefix).
pub fn largest_step(
    repo: &Repository,
    head: Oid,
    base: Option<Oid>,
    limit: u64,
) -> Result<Step, git2::Error> {
    let first_size = match measure(repo, &[head], base, limit)? {
        Measured::Fits(_) => return Ok(Step::All),
        Measured::Over(n) => n,
    };
    let base = base.filter(|b| repo.odb().is_ok_and(|odb| odb.exists(*b)));
    // The first-parent chain from head back to the base, oldest first.
    let mut chain = Vec::new();
    let mut at = head;
    loop {
        if Some(at) == base {
            break;
        }
        chain.push(at);
        let commit = repo.find_commit(at)?;
        match commit.parent_id(0) {
            Ok(parent) => at = parent,
            Err(_) if base.is_none() => break,
            // The base is not on the first-parent chain.
            Err(_) => {
                return Ok(Step::None {
                    first: head,
                    size_at_least: first_size,
                });
            }
        }
    }
    chain.reverse();
    // chain[n - 1] is head, which does not fit. Find the largest k with
    // chain[k - 1] fitting.
    let (mut fits, mut over) = (0usize, chain.len());
    let mut over_size = first_size;
    while over - fits > 1 {
        let mid = fits + (over - fits) / 2;
        match measure(repo, &[chain[mid - 1]], base, limit)? {
            Measured::Fits(_) => fits = mid,
            Measured::Over(n) => {
                over = mid;
                over_size = n;
            }
        }
    }
    if fits == 0 {
        return Ok(Step::None {
            first: chain[0],
            size_at_least: over_size,
        });
    }
    Ok(Step::UpTo {
        commit: chain[fits - 1],
        commits_left: chain.len() - fits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Repo {
        _dir: tempfile::TempDir,
        repo: Repository,
    }

    impl Repo {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = Repository::init_bare(dir.path()).unwrap();
            Self { _dir: dir, repo }
        }

        /// A commit on top of `parent` adding `name` with `content`.
        fn commit(&self, parent: Option<Oid>, name: &str, content: &[u8]) -> Oid {
            let repo = &self.repo;
            let mut builder = match parent {
                Some(p) => repo
                    .treebuilder(Some(&repo.find_commit(p).unwrap().tree().unwrap()))
                    .unwrap(),
                None => repo.treebuilder(None).unwrap(),
            };
            let blob = repo.blob(content).unwrap();
            builder.insert(name, blob, 0o100644).unwrap();
            let tree = repo.find_tree(builder.write().unwrap()).unwrap();
            let sig = git2::Signature::new("t", "t@example", &git2::Time::new(0, 0)).unwrap();
            let parents: Vec<_> = parent
                .map(|p| repo.find_commit(p).unwrap())
                .into_iter()
                .collect();
            let refs: Vec<_> = parents.iter().collect();
            repo.commit(None, &sig, &sig, name, &tree, &refs).unwrap()
        }
    }

    /// Incompressible bytes: a xorshift stream.
    fn random(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    const MIB: u64 = 1 << 20;

    #[test]
    fn small_commits_fit() {
        let r = Repo::new();
        let a = r.commit(None, "a", b"one");
        let b = r.commit(Some(a), "b", b"two");
        let Measured::Fits(n) = measure(&r.repo, &[b], None, MIB).unwrap() else {
            panic!("over");
        };
        assert!(n > 0 && n < 4096, "{n}");
        assert_eq!(largest_step(&r.repo, b, None, MIB).unwrap(), Step::All);
    }

    #[test]
    fn a_large_blob_is_over_and_counting_stops_early() {
        let r = Repo::new();
        let a = r.commit(None, "big", &random(2 * MIB as usize, 7));
        match measure(&r.repo, &[a], None, MIB).unwrap() {
            Measured::Over(n) => {
                assert!(n > MIB, "{n}");
                // libgit2 hands the pack over in chunks; the count stops
                // at the first one past the limit, well short of the whole.
                assert!(n < 2 * MIB, "{n}");
            }
            m => panic!("{m:?}"),
        }
        assert!(matches!(
            measure(&r.repo, &[a], None, 4 * MIB).unwrap(),
            Measured::Fits(n) if n > 2 * MIB
        ));
        assert_eq!(
            largest_step(&r.repo, a, None, MIB).unwrap(),
            Step::None {
                first: a,
                size_at_least: match measure(&r.repo, &[a], None, MIB).unwrap() {
                    Measured::Over(n) => n,
                    m => panic!("{m:?}"),
                }
            }
        );
    }

    #[test]
    fn the_base_is_hidden() {
        let r = Repo::new();
        let a = r.commit(None, "big", &random(2 * MIB as usize, 1));
        let b = r.commit(Some(a), "small", b"x");
        assert!(matches!(
            measure(&r.repo, &[b], None, MIB).unwrap(),
            Measured::Over(_)
        ));
        assert!(matches!(
            measure(&r.repo, &[b], Some(a), MIB).unwrap(),
            Measured::Fits(n) if n < 4096
        ));
        // A base the repository lacks is ignored.
        let unknown = Oid::from_str("1234567890123456789012345678901234567890").unwrap();
        assert!(matches!(
            measure(&r.repo, &[b], Some(unknown), MIB).unwrap(),
            Measured::Over(_)
        ));
    }

    #[test]
    fn stepping_picks_the_largest_fitting_prefix() {
        let r = Repo::new();
        let base = r.commit(None, "base", b"base");
        // Five commits of 400 KiB each: two fit in 1 MiB, three do not.
        let mut chain = Vec::new();
        let mut at = base;
        for i in 0..5 {
            at = r.commit(Some(at), &format!("f{i}"), &random(400 << 10, i + 10));
            chain.push(at);
        }
        let step = largest_step(&r.repo, at, Some(base), MIB).unwrap();
        assert_eq!(
            step,
            Step::UpTo {
                commit: chain[1],
                commits_left: 3
            }
        );
        // From there, the next two.
        assert_eq!(
            largest_step(&r.repo, at, Some(chain[1]), MIB).unwrap(),
            Step::UpTo {
                commit: chain[3],
                commits_left: 1
            }
        );
        assert_eq!(
            largest_step(&r.repo, at, Some(chain[3]), MIB).unwrap(),
            Step::All
        );
        // Never pushed: the chain runs to the root.
        assert_eq!(
            largest_step(&r.repo, at, None, MIB).unwrap(),
            Step::UpTo {
                commit: chain[1],
                commits_left: 3
            }
        );
    }

    #[test]
    fn a_base_off_the_chain_is_not_stepped() {
        let r = Repo::new();
        let root = r.commit(None, "root", b"r");
        let side = r.commit(Some(root), "side", b"s");
        let mut at = root;
        for i in 0..3 {
            at = r.commit(Some(at), &format!("f{i}"), &random(600 << 10, i + 20));
        }
        assert!(matches!(
            largest_step(&r.repo, at, Some(side), MIB).unwrap(),
            Step::None { first, .. } if first == at
        ));
    }
}
