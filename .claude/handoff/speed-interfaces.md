# Working state for the Interfaces gate: tree views speed (from speed-design.md v4)

Not for the owner. Moved off the Module page (review finding 14). Revised for findings 6, 7, 10, 12, 13.

## Numbering (finding 7)

One domain: every journal record has a `RecordNo` (its place in the journal). A step is named by its record number.
The durable mark, the built point and `makeDurable(through)` are record numbers. No separate `StepNo`.

## Records

```ts
/** The journal. Appended only by the actor. */
type Record =
  | { kind: "open"; open: OpenNo; tree: TreeId;
      path: string | "anonymous";                 // O_TMPFILE: no name, no saved state until `link`
      object: ObjectNo; truncate: boolean; by: WriterAt }
  | { kind: "call"; open: OpenNo; call: Write | Append | Truncate }   // bytes in the frame's tail
  | { kind: "page-write"; tree: TreeId; object: ObjectNo; runs: Run[] }  // R1.4, from `pageWrite`
  | { kind: "step"; by: WriterAt; at: Time; change: Change;
      pointers: PointerChange[];                  // values name steps by RecordNo
      rows: RowWrite[] }                          // M1 S3
  | { kind: "checkpoint"; pointers: PointerChange[];   // every value an oid: all at or before builtThrough
      opens: OpenRecord[]; objects: ObjectTable; builtThrough: RecordNo };

type Change =
  | { kind: "root"; tree: TreeId }                // Agentfs-Cause: Root
  | { kind: "create"; tree: TreeId; path: string; object: ObjectNo }
  | { kind: "link"; tree: TreeId; open: OpenNo; path: string }   // an anonymous open named (Step.linked today)
  | { kind: "save"; tree: TreeId; open: OpenNo; outcome: "saved" | "unchanged" | "orphan-dropped" }
  | { kind: "rename"; tree: TreeId; from: string; to: string; over?: ObjectNo }
  | { kind: "unlink"; tree: TreeId; path: string }
  | { kind: "truncate"; tree: TreeId; path: string; size: number }
  | { kind: "fork"; fork: ForkId; upstream: TreeId; at: RecordNo }   // no commit; a row
  | { kind: "drop-fork"; fork: ForkId }
  | { kind: "rows" }
  | { kind: "take"; decision: Decision };
// No transaction kinds: transactions are not built (git-model §9).

interface Decision {
  tree: TreeId;
  entry: string;                    // refs/queue/<tree>/<n>, deleted by this step
  decidedAt: RecordNo;              // the built point it read; `land` refuses if a later step touched `files`
  incoming: Oid;                    // built
  files: FileResult[];              // per file read or written, lines by reference
  collisions: Collision[];
  by: WriterAt;
}

/** The built log. Appended only by the builder; its own fsync. */
type BuiltEntry = { through: RecordNo; commits: Array<[RecordNo, Oid[]]> };
// An entry for step s is dropped only when no retained journal record names s.
```

## Journal surface (findings 6, 13)

```ts
interface Journal {
  append(record: Record): RecordNo | RecordError;       // actor only
  /** One leader fsyncs; callers whose records it covers join it. The mark becomes
      max(mark, highest record appended before that fsync began), only after it returns. */
  makeDurable(through: RecordNo): void | DurableError;  // actor (FLUSH, fsync, O_SYNC) or builder
  read(from: Cursor): Iterable<Frame>;                  // stops at the durable mark
  dropSegments(built: RecordNo): void;                  // whole segments; copy-forward of a live open's calls
}
```

Batch install: fast-import exits with pack and idx fsynced (`core.fsync=pack,pack-metadata`, to verify in B2), pack
renamed before idx; the builder fsyncs `objects/pack/`, appends and fsyncs its built-log entry, then copies refs and
emits the built event. Recovery deletes temporary packs and stale `.keep` files and rebuilds steps past the built log.

## Errors

```ts
type RecordError = "no-space" | "too-large" | { io: Errno } | "stopped";
type DurableError = { io: Errno } | "stopped";       // a failed fsync stops the actor, as today
type BuildError = "no-space" | { io: Errno } | { disagrees: RecordNo } | { missing: Oid };
type WaitError = { failed: BuildError } | "stopped";
type Landed = { step: RecordNo };
```

## Mutation probes (each must fail a test)

- B1: a per-file blob write put back (objects-per-save count); object fsync skipped before the record (crash test); a stale file left by the checkout.
- B2: the built event before the built-log fsync (failpoint); idx renamed before the pack (failpoint); a take reading past the built point (does not compile: no snapshot constructor); the stall removed (lag test); a covered hole called torn and a torn tail called corrupt, per interleaving of the two fsync callers (journal test); the builder reading past the durable mark (power-loss test); `land` accepting a decision whose file changed (model replay).
- B3: a whole-file copy per write (bytes-copied counter); a full compare for "unchanged" (bytes-compared counter); a live open's segments copied forward on the 1 GiB bar (bytes-written counter).
- B4: a whole-tree blob per step (objects-per-save count); conformance reads the format from its page, sharing no code with the builder.

Kept from v3: build with the wall clock (dates come from records). Replaced (finding 12): "refs byte-identical to an uninterrupted run" becomes "refs name only built-log commits, whose trees and messages match an uninterrupted run in the same image".
