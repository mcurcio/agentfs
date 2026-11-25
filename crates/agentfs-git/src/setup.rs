//! The process's one libgit2 setup ([`init`]), and the remote timeouts it
//! sets. Every libgit2 option is set here and nowhere else: `clippy.toml`
//! disallows the setters everywhere, and each call below expects that lint.

use std::sync::Once;

/// How long libgit2 waits for a remote to accept a connection.
pub const SERVER_CONNECT_TIMEOUT_MS: std::ffi::c_int = 30_000;
/// How long libgit2 waits on a stalled read or write to a remote.
pub const SERVER_IO_TIMEOUT_MS: std::ffi::c_int = 300_000;

/// Process-wide libgit2 setup, done once before any repository is opened:
///
/// - objects and refs are fsynced (libgit2 does not by default; design);
/// - the system, XDG, global and ProgramData config search paths are
///   cleared, so no config outside the history repository's own file can
///   reach a volume: not `url.<base>.insteadOf` redirecting a push, not
///   `http.*`, not `core.excludesFile`;
/// - the home directory is cleared, so `~/.ssh/known_hosts` never decides
///   or steers an SSH host key check;
/// - remote connections and reads/writes time out
///   ([`SERVER_CONNECT_TIMEOUT_MS`], [`SERVER_IO_TIMEOUT_MS`]), so a hung
///   remote ends as an unreachable push.
///
/// It registers no transport: agentfs-core's SSH transport is registered by
/// its own `init_git`, after this.
pub fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        #[expect(clippy::disallowed_methods, reason = "the one libgit2 setup")]
        libgit2_sys::init();
        // SAFETY: GIT_OPT_ENABLE_FSYNC_GITDIR takes a single int argument.
        #[expect(clippy::disallowed_methods, reason = "the one libgit2 setup")]
        let rc = unsafe {
            libgit2_sys::git_libgit2_opts(
                libgit2_sys::GIT_OPT_ENABLE_FSYNC_GITDIR as std::ffi::c_int,
                1 as std::ffi::c_int,
            )
        };
        assert_eq!(rc, 0, "libgit2 rejected GIT_OPT_ENABLE_FSYNC_GITDIR");
        for level in [
            git2::ConfigLevel::System,
            git2::ConfigLevel::XDG,
            git2::ConfigLevel::Global,
            git2::ConfigLevel::ProgramData,
        ] {
            // SAFETY: called once, inside `call_once`, before this process
            // opens any repository or config through agentfs; libgit2's
            // search paths are not otherwise touched by agentfs.
            #[expect(clippy::disallowed_methods, reason = "the one libgit2 setup")]
            let result = unsafe { git2::opts::set_search_path(level, "") };
            assert!(
                result.is_ok(),
                "libgit2 rejected clearing {level:?}: {result:?}"
            );
        }
        // No home directory either: libgit2's SSH transport reads
        // `~/.ssh/known_hosts` from it, and agentfs checks host keys only
        // against each remote's own agentfsKnownHosts.
        // SAFETY: GIT_OPT_SET_HOMEDIR takes one C string, which libgit2
        // copies; called once, before any transport is created.
        #[expect(clippy::disallowed_methods, reason = "the one libgit2 setup")]
        let rc = unsafe {
            libgit2_sys::git_libgit2_opts(
                libgit2_sys::GIT_OPT_SET_HOMEDIR as std::ffi::c_int,
                c"".as_ptr(),
            )
        };
        assert_eq!(rc, 0, "libgit2 rejected GIT_OPT_SET_HOMEDIR");
        // A remote that accepts the connection and then stalls must end as
        // a failed push, not hang its thread forever.
        // SAFETY: plain integer options, set once before any transport is
        // created.
        #[expect(clippy::disallowed_methods, reason = "the one libgit2 setup")]
        let connect = unsafe {
            git2::opts::set_server_connect_timeout_in_milliseconds(SERVER_CONNECT_TIMEOUT_MS)
        };
        assert!(
            connect.is_ok(),
            "libgit2 rejected the connect timeout: {connect:?}"
        );
        // SAFETY: as above.
        #[expect(clippy::disallowed_methods, reason = "the one libgit2 setup")]
        let io = unsafe { git2::opts::set_server_timeout_in_milliseconds(SERVER_IO_TIMEOUT_MS) };
        assert!(io.is_ok(), "libgit2 rejected the I/O timeout: {io:?}");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libgit2_home_directory_is_cleared() {
        init();
        let mut buf = libgit2_sys::git_buf {
            ptr: std::ptr::null_mut(),
            reserved: 0,
            size: 0,
        };
        // SAFETY: GIT_OPT_GET_HOMEDIR fills a git_buf, disposed below.
        #[expect(clippy::disallowed_methods, reason = "a read of the setup")]
        let rc = unsafe {
            libgit2_sys::git_libgit2_opts(
                libgit2_sys::GIT_OPT_GET_HOMEDIR as std::ffi::c_int,
                &mut buf as *mut libgit2_sys::git_buf,
            )
        };
        let size = buf.size;
        // SAFETY: the buffer came from libgit2.
        unsafe { libgit2_sys::git_buf_dispose(&mut buf) };
        assert_eq!(rc, 0);
        assert_eq!(size, 0, "libgit2 still has a home directory");
    }

    #[test]
    fn libgit2_server_timeouts_are_set() {
        init();
        // SAFETY: reads of integer options.
        unsafe {
            assert_eq!(
                git2::opts::get_server_connect_timeout_in_milliseconds().unwrap(),
                SERVER_CONNECT_TIMEOUT_MS
            );
            assert_eq!(
                git2::opts::get_server_timeout_in_milliseconds().unwrap(),
                SERVER_IO_TIMEOUT_MS
            );
        }
    }

    #[test]
    fn init_clears_config_search_paths() {
        init();
        for level in [
            git2::ConfigLevel::System,
            git2::ConfigLevel::XDG,
            git2::ConfigLevel::Global,
            git2::ConfigLevel::ProgramData,
        ] {
            // SAFETY: reading the search path set once by init.
            let path = unsafe { git2::opts::get_search_path(level) }.unwrap();
            assert!(path.as_bytes().is_empty(), "{level:?}: {path:?}");
        }
    }
}
