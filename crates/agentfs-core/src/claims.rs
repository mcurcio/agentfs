//! Per-path ordering for the recorder (design §3.2, §4).
//!
//! Whether a change is recorded depends on the tracked set, and the tracked
//! set changes only once a record is durable. A record therefore claims its
//! paths for the whole synchronous step: deciding, freezing, journaling and
//! updating the tracked set. A record touching a claimed path, the same path
//! or one above or below it, waits until the claim is released. Records for
//! unrelated paths proceed in parallel, so a slow freeze holds up only
//! changes to its own path.
//!
//! Without this, a removal racing an in-flight first write of the same path
//! would see the path as untracked and be skipped, and history would keep a
//! file the tree no longer has. Keying on paths rather than inodes matters:
//! `unlink` and `rename` act on the parent directory, not the inode.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::{Condvar, Mutex, MutexGuard};

use crate::paths::RepoPath;

/// The paths claimed by in-flight records. Safe to share across threads.
#[derive(Debug, Default)]
pub struct PathClaims {
    /// Claimed path to the number of claims on it (a record may name a path
    /// twice).
    held: Mutex<BTreeMap<String, usize>>,
    released: Condvar,
}

/// Paths held by one record; released on drop, including on error or panic.
#[derive(Debug)]
pub struct Claim<'a> {
    claims: &'a PathClaims,
    paths: Vec<String>,
}

impl PathClaims {
    /// Claims every path in `paths` at once, waiting until none of them
    /// conflicts with a path another record holds.
    ///
    /// All paths are taken together, never one at a time, so two records
    /// cannot each hold part of what the other needs: there is no deadlock.
    pub fn claim<'p>(&self, paths: impl IntoIterator<Item = &'p RepoPath>) -> Claim<'_> {
        let paths: Vec<String> = paths.into_iter().map(|p| p.as_str().to_owned()).collect();
        let mut held = self.lock();
        while paths.iter().any(|p| conflicts(&held, p)) {
            held = self.released.wait(held).expect("path claims lock poisoned");
        }
        for p in &paths {
            *held.entry(p.clone()).or_default() += 1;
        }
        Claim {
            claims: self,
            paths,
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, usize>> {
        // Poisoning needs a panic while the lock is held, and nothing in the
        // critical sections can panic.
        self.held.lock().expect("path claims lock poisoned")
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let mut held = self.claims.lock();
        for p in &self.paths {
            if let Some(n) = held.get_mut(p) {
                *n -= 1;
                if *n == 0 {
                    held.remove(p);
                }
            }
        }
        drop(held);
        self.claims.released.notify_all();
    }
}

/// True if `path`, one of its ancestors, or anything beneath it is held.
fn conflicts(held: &BTreeMap<String, usize>, path: &str) -> bool {
    if held.contains_key(path) {
        return true;
    }
    let mut end = 0;
    while let Some(i) = path[end..].find('/') {
        end += i;
        if held.contains_key(&path[..end]) {
            return true;
        }
        end += 1;
    }
    let prefix = format!("{path}/");
    held.range::<String, _>((Bound::Included(prefix.clone()), Bound::Unbounded))
        .next()
        .is_some_and(|(p, _)| p.starts_with(&prefix))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn conflicts_cover_the_path_its_ancestors_and_descendants() {
        let held: BTreeMap<String, usize> = [("a/b".to_owned(), 1)].into();
        assert!(conflicts(&held, "a/b"));
        assert!(conflicts(&held, "a"));
        assert!(conflicts(&held, "a/b/c"));
        assert!(!conflicts(&held, "a/bc"));
        assert!(!conflicts(&held, "a/c"));
        assert!(!conflicts(&held, "b"));
        assert!(!conflicts(&held, "ab"));
    }

    /// Spawns a thread that claims `path`, reporting once it has had it (the
    /// claim is released before the report, so the caller can check that
    /// nothing is left held).
    fn claim_in_thread(claims: &Arc<PathClaims>, path: &str) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        let claims = claims.clone();
        let path = p(path);
        std::thread::spawn(move || {
            drop(claims.claim([&path]));
            tx.send(()).unwrap();
        });
        rx
    }

    #[test]
    fn conflicting_claims_wait_and_unrelated_ones_do_not() {
        let claims = Arc::new(PathClaims::default());
        let a = p("d/a");
        let held = claims.claim([&a, &a]);

        let unrelated = claim_in_thread(&claims, "e");
        unrelated
            .recv_timeout(Duration::from_secs(5))
            .expect("unrelated path is not held up");

        let same = claim_in_thread(&claims, "d/a");
        let parent = claim_in_thread(&claims, "d");
        assert!(same.recv_timeout(Duration::from_millis(50)).is_err());
        assert!(parent.try_recv().is_err());

        drop(held);
        same.recv_timeout(Duration::from_secs(5)).unwrap();
        parent.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(claims.lock().is_empty());
    }
}
