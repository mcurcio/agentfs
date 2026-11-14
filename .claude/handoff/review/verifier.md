# Speed epic end review: verifier (0abb4f4..3d55a25), 2026-11-14
Verdict FAIL; gates green (fmt, clippy, core+failpoints 1049, fuse/agentfs/conformance 444).
1 HIGH reopen fails Error::Format (mod.rs:683 built_maps.remove(..).ok_or(Format)) after O_TMPFILE + write + first_link, killed with handle open and link unbuilt: builder Change::Link (builder.rs:496-506) doesn't add object to batch.files; rebase skips non-on_frames maps. Repro: failpoint tree.build_before_batch=error@*, drop, reopen.
2 HIGH no test of release-on-Failed (probe D: failed_event without notify SURVIVED); row a_failed_builder_releases_the_gate_and_recording_goes_on missing.
3 HIGH install order unguarded (probe B: built-log append after copy SURVIVED); crash test must read built.log vs refs on disk before reopen.
4 MED checkpoint sync_data loop deletable (probe A SURVIVED; journal/mod.rs:909).
5 MED drop_builds_everything vacuous: open settles (mod.rs:665,720); read built.log before reopen.
6 MED Disagrees never shown to fire (probe H SURVIVED; builder.rs:550).
7 MED the_builder_never_reads_past_the_mark weaker than page (needs reader with unsynced frame).
8 MED memory: peak ~60 MiB + 5.8x file; 3.6x resident after; drain ~0.6 s/MiB (1 GiB ~10 min); state area 4.4x. builder.rs:560-570, tree_reconcile.rs:179, format.rs:95. docs/trees.md:150, performance.md "All meet their bars" wrong.
9 LOW garbled doc takes.rs:311.
Harness: scratchpad bin/mem.py (MiB workdir cap_s).
