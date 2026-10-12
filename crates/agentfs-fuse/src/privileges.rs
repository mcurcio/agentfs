//! What the daemon is allowed to do (design §6, §17.1).
//!
//! Decided from the effective capability set (`CapEff` in
//! `/proc/self/status`), not from the effective uid, so a non-root daemon
//! given the capabilities gets the same behaviour as root, and root without
//! them (a restricted container) does not try what will fail:
//!
//! - **Giving new objects to their caller** needs `CAP_CHOWN` (to `fchown`
//!   the object) and `CAP_FOWNER` (to set its mode once it is no longer the
//!   daemon's).
//! - **Passthrough** needs `CAP_SYS_ADMIN` in the *initial* user namespace:
//!   the kernel checks `capable(CAP_SYS_ADMIN)` when a backing file is
//!   registered. Root in a rootless container (a user namespace) has the
//!   capability only in its own namespace, so passthrough is not attempted
//!   there.
//! - **Unmounting with `umount(2)`** needs `CAP_SYS_ADMIN`; without it the
//!   daemon uses `fusermount3`.

use std::fs;

pub const CAP_CHOWN: u32 = 0;
pub const CAP_DAC_OVERRIDE: u32 = 1;
pub const CAP_FOWNER: u32 = 3;
pub const CAP_SYS_ADMIN: u32 = 21;

/// The daemon's privileges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Privileges {
    /// The effective capability set, one bit per capability.
    pub effective: u64,
    /// The process is in the initial user namespace.
    pub initial_userns: bool,
}

impl Privileges {
    /// This process's privileges. Unreadable `/proc` files count as no
    /// capabilities and a user namespace (the safe answers).
    pub fn current() -> Self {
        let effective = fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| parse_cap_eff(&s))
            .unwrap_or(0);
        let initial_userns =
            fs::read_to_string("/proc/self/uid_map").is_ok_and(|m| is_identity_uid_map(&m));
        Self {
            effective,
            initial_userns,
        }
    }

    pub fn has(&self, cap: u32) -> bool {
        cap < 64 && self.effective & (1 << cap) != 0
    }

    /// New objects can be given to their caller with the mode it asked for.
    pub fn can_chown_new(&self) -> bool {
        self.has(CAP_CHOWN) && self.has(CAP_FOWNER)
    }

    /// Registering FUSE backing files (passthrough) is allowed.
    pub fn can_passthrough(&self) -> bool {
        self.has(CAP_SYS_ADMIN) && self.initial_userns
    }

    /// `umount(2)` is allowed.
    pub fn can_unmount(&self) -> bool {
        self.has(CAP_SYS_ADMIN)
    }

    /// Why passthrough is not allowed, if it is not.
    pub fn passthrough_refusal(&self) -> Option<&'static str> {
        if !self.has(CAP_SYS_ADMIN) {
            Some("the daemon lacks CAP_SYS_ADMIN (registering backing files needs it; run as root)")
        } else if !self.initial_userns {
            Some(
                "the daemon runs in a user namespace (registering backing files needs \
                 CAP_SYS_ADMIN in the initial namespace)",
            )
        } else {
            None
        }
    }
}

/// The `CapEff:` line of `/proc/<pid>/status`.
fn parse_cap_eff(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
}

/// True for the initial user namespace's map: every uid to itself.
fn is_identity_uid_map(map: &str) -> bool {
    let lines: Vec<Vec<&str>> = map
        .lines()
        .map(|l| l.split_whitespace().collect())
        .filter(|f: &Vec<&str>| !f.is_empty())
        .collect();
    matches!(lines.as_slice(), [f] if f.as_slice() == ["0", "0", "4294967295"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_bits() {
        let status = "Name:\tagentfs\nCapInh:\t0000000000000000\nCapEff:\t000001ffffffffff\n";
        assert_eq!(parse_cap_eff(status), Some(0x1ff_ffff_ffff));
        assert_eq!(parse_cap_eff("Name: x\n"), None);
        let all = Privileges {
            effective: 0x1ff_ffff_ffff,
            initial_userns: true,
        };
        assert!(all.can_chown_new() && all.can_passthrough() && all.can_unmount());
        assert_eq!(all.passthrough_refusal(), None);
        let in_userns = Privileges {
            initial_userns: false,
            ..all
        };
        assert!(in_userns.can_chown_new());
        assert!(!in_userns.can_passthrough());
        assert!(
            in_userns
                .passthrough_refusal()
                .unwrap()
                .contains("user namespace")
        );
        let chown_only = Privileges {
            effective: 1 << CAP_CHOWN,
            initial_userns: true,
        };
        assert!(!chown_only.can_chown_new(), "needs CAP_FOWNER too");
        let caps = Privileges {
            effective: (1 << CAP_CHOWN)
                | (1 << CAP_FOWNER)
                | (1 << CAP_DAC_OVERRIDE)
                | (1 << CAP_SYS_ADMIN),
            initial_userns: true,
        };
        assert!(caps.can_chown_new() && caps.can_passthrough());
        assert!(
            Privileges::default()
                .passthrough_refusal()
                .unwrap()
                .contains("CAP_SYS_ADMIN")
        );
    }

    #[test]
    fn user_namespaces() {
        assert!(is_identity_uid_map("         0          0 4294967295\n"));
        assert!(!is_identity_uid_map("0 1000 1\n"));
        assert!(!is_identity_uid_map("0 100000 65536\n"));
        assert!(!is_identity_uid_map(""));
    }
}
