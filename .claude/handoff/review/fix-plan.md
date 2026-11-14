# Speed epic: fix plan after the end review (2026-11-14)
Findings: coherence.md, durability.md, verifier.md (this folder). Three FAILs; several findings share two root causes,
so the fix runs as two sequential agents (shared files), not one: the class fixes first, then the rest.

## Bar for history of large files (decided, from the owner's "Match recording engine")
The owner's "1 GiB to history in 7-12 s" in speed-brief.md quoted the engine's *close* budgets (bench-budget.json
bigclose: 8 s XFS / 12 s ext4), which B3 meets. The engine's own 1 GiB commit takes 23-29 s (performance.md). So:
1 GiB save's drain_s <= 60 s on CI runners (engine parity with margin), builder peak memory bounded independent of
file size (target: under 256 MiB extra for a 1 GiB save), both measured in the image job's figures with bars in
ci/trees-budget.json.

## Fix 1: class fixes
R1 File bytes have one home. Line maps carry identities and line lengths/offsets, never bytes (tree_reconcile.rs
Line.bytes; format.rs per-byte JSON; TakeRecord.files). Bytes live in the journal frames / take tail / files/ blob.
Alignment streams (agentfs-align over readers or the files themselves, old image from the built blob/checkout);
an append-only or new file aligns without reading it whole. Checkout writes stream from the blob. Take tails larger
than a frame use a multi-frame form or are bounded by streaming. History content (what the model sees) unchanged:
model digests and conformance unchanged.
R2 Opening a volume never waits for history. Open serves from the journal's fold (saved maps from frames/checkpoint),
the builder catches up behind as at runtime; a build failure raises the alarm (HistoryBuildFailed) and never refuses
open (principle 5). Tests that need history call settle explicitly.
R3 Restart equals an uninterrupted run: (a) no record appended after a checkpoint names a step at or before it
(fork head resolved after compaction, or checkpoint carries the built oid) - durability P1-1; (b) Change::Link
publishes the linked object's map (verifier 1). A test for each that fails first.
## Fix 2: the rest
Tests that close surviving probes A, B, D, G, H and the weak mark test (verifier 2-7); compaction progresses under
constant writes (durability P2-2); one fd per file per tree (P2-3); torn tail cross-checks built.log (P2-1);
dir_unsynced true at start, compaction fsync errors sticky (P3-1, P3-2); frame relocation owned by the journal and a
space test by statvfs (coherence 3); TreeActor constructors crate-private, one take path (coherence 5); admit_then
without a thread per call (10); build_cost on B4's layout (9); metrics on built events, no 100 ms poll in figures
(11); fanout one reader per language in ci (12); object identity check via inode (13); docs/CHANGELOG/comments
truthful (14, verifier 8-9); close_4k p99 regression explained (15); P3 items; apply or close every open erratum.
