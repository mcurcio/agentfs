//! The process's allocator settings, one home: glibc's malloc keeps up to
//! eight arenas per CPU, and memory a thread freed in its arena stays
//! resident for that arena alone. A binary calls [`limit_arenas`] first thing
//! in `main`, before any thread starts (docs/architecture.md, "Technology
//! choices"). A library never calls it: the allocator is the binary's choice.

/// The most malloc arenas a process keeps (`M_ARENA_MAX`).
pub const ARENAS: i32 = 2;

/// Sets glibc's `M_ARENA_MAX` to [`ARENAS`]. Call before any thread starts:
/// arenas made before it stay. A no-op where the C library is not glibc. A
/// refusal by `mallopt` is ignored: the process runs on with glibc's default.
pub fn limit_arenas() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: `mallopt` only sets a malloc parameter, and the caller runs it
    // before any thread starts, so nothing allocates beside it.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, ARENAS);
    }
}
