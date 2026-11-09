# Domain: Rules

**Proposed, not built.** This page proposes the Rules domain (path policy and view profiles), one part of the system around the git model ([the system](../system.md)). The git model ([git-model.md](../git-model.md)) wins wherever the two disagree. Rust sketches show surfaces only. The commit that lands the slice that builds this domain deletes this page and moves the behaviour into the library or operator docs.
"Profile" on this page always means a **view profile**; the CSI settings bundle keeps the bare word.

## 0. Guidance this domain rests on

The owner's guidance that applies here is in `OWNER-GUIDANCE.md` at the repository root. Everything else on this page is a design choice.

## 1. What Rules owns, and what it is not

Rules is a **policy decision point**: other domains enforce, Rules only answers, from one immutable rule set.

| Rules owns | Rules is not |
|---|---|
| whether a change to a path is committed: refused when saved (`record`), skipped when not tracked (`skip`) | whether a path is tracked: the tree's branch says (git-model §3) |
| database group kinds and each group's members | how a file changed while open is saved (git-model §6; the model's rule 8) |
| profiles: what each view shows (`visibility`) | mapping an answer to an error number, caches, mounts (Views) |
| the built-in never-recorded names; validating the operator's rule files at boot | path syntax and confinement (`RelPath::parse`, Trees) |

**Design choice: only the operator's rule files count; rule files inside the tree (`.gitignore`,
`.gitattributes`) are ordinary content.** One rule set per volume, fixed for the history service's life. Cost: a
program cannot steer its own recording. Not chosen: tree rules, which let a rule-file edit change another tree's
recording once it lands or syncs.

**Design choice: a view profile is a base plus operator hide patterns, narrowing only.** `Everything` (main) or
`Recordable` (forks), plus patterns that hide more; a profile never shows what its base hides (RU3).

## 2. The public contract

```rust
pub trait Rules {
    /// At the write-open (the tree's actor), since RELEASE cannot carry a refusal (git-model §3): commit,
    /// built-in skip, or refuse; a size rule is enforced at the write call. No tracked fact. Pure and total.
    fn record(&self, p: &RelPath, k: EntryKind) -> RecordVerdict;
    /// Save time, with the tracked fact read from the tree's branch: is this change left out of the commit?
    fn skip(&self, p: &RelPath, k: EntryKind, f: PathFacts) -> Option<SkipReason>;
    /// The database group p belongs to, whole (Views, Trees).
    fn group(&self, p: &RelPath) -> Option<Group>;
    /// What may this client's view see and change at n? Existence-blind.
    fn visibility(&self, n: &TreeName, k: EntryKind, v: ViewProfileId) -> Visibility;
    fn view_profile(&self, name: &ProfileName) -> Option<ViewProfileId>; // the only way to get a ViewProfileId
    fn version(&self) -> RuleSetVersion;                                  // carried in a Binding
}
pub struct PathFacts { tracked: bool, containment: Containment }
pub enum Containment { Outside, InRepository, Unknown } // as Trees observed it

pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Special,       // device, FIFO, socket: never recorded
    Removal,       // verdict only: the change removes p
}
pub enum RecordVerdict {
    Record(RecordRule),     // max size, store hint
    BuiltIn(BuiltIn),       // never recorded: SharedMemory (-shm), NfsSillyRename, Special, EmptyDir
    Refuse(RuleRefusal),    // GroupMemberSkipped: a group is recorded whole or not at all
}
pub enum SkipReason {
    NeverRecord,                       // operator attribute -agentfs (a tracked removal still commits)
    Ignored { pattern: PatternRef },   // operator ignore rule; untracked paths only
    InRepository,                      // inside a nested repository; untracked paths only
    UntrackedRemoval,
}
pub enum Visibility {
    Show,      // seen; a change goes to the client's own tree and is judged by record
    Scratch,   // only the client's own private entry: never committed, never landed
    Hide,      // nothing here, whatever exists: absent from lookups and listings, no creation
    Refuse,    // seen read-only: no change, creation or move of this kind here
}
pub enum RulesError {                  // boot only
    Unreadable { source: SourceName, io: IoKind },
    BadValue { source: SourceName, line: u32, attribute: String },
    UnknownGroupKind { line: u32, kind: String },
    UnknownBase { profile: ProfileName },
}
```

**How a profile answers** (design choice; `Recordable` reproduces today's sandbox view):

| Kind at p | `Everything` | `Recordable` |
|---|---|---|
| File, recordable if new | Show | Show |
| File the rules skip if new | Show | Scratch |
| File in a group with a member skipped | Show | Hide (the whole group) |
| Dir with nothing recordable possible beneath | Show | Hide |
| Symlink at a recordable path | Show | Refuse |
| Special, or inside a nested repository | Show | Hide |
| matched by the profile's hide patterns | Hide | Hide |

A skipped or scratch file lives in its tree's untracked area beside the working tree, never in the git working tree
itself: never committed, so no landing or sync carries it, and git's checkout never meets it (design choice; the one
home of the rule, including a tracked file arriving at its path, is git-model §3).

| Caller | Asks | When |
|---|---|---|
| Views | `visibility`, `group` | each lookup, listing, create, rename end, and each name a `TreeMoved` drops |
| the tree's actor | `record`, `skip` | building a save's commit |
| Trees | `group` | reporting containment; a pinned snapshot's checkout |
| Clients | `view_profile`, `version` | narrowing a grant, building a `Binding` |

## 3. Invariants

| # | Invariant | Enforced by | Strength |
|---|---|---|---|
| RU1 | Rules is pure and total: no I/O, no clock, no error after boot | depends on `model` and `contracts` only; methods return no `Result` | compile time |
| RU2 | A rule-set error stops the service; it never degrades to "show" or "record everything" | `RuleSet::build` is the only constructor; no fallback | boot time |
| RU3 | Visibility never depends on existence or on who wrote a name; a profile shows nothing its base hides | `visibility` takes no existence or writer argument; a profile holds only a base and hide patterns | compile + construction time |
| RU4 | A database group is shown, hidden and recorded whole | `Group::members()` is computed, never supplied | construction time + test |
| RU5 | Ignore rules decide only for untracked paths; a tracked path stays recorded until removed | `skip` order: never-record, then `tracked` short-circuits ignore and containment | test |
| RU6 | A path whose containment is unknown is judged as inside a repository (fail closed) | one exhaustive `match` | compile time |
| RU7 | Every view host judges with the history service's rule set | a `Binding` carries `RuleSetVersion` (a digest of the sources and the family); another version reloads before serving | boot time per binding |

## 4. Ports and configuration

`RuleSources::read() -> Result<RuleTexts, RulesError>`, once at boot: the operator's exclude file, attributes file and
profile file (adapters: the state area for the sidecar and CLI; the files a StorageClass installs for CSI). Matching
uses `gix-ignore` and `gix-attributes` on those buffers only; system and user git config are never read. Rules
persists nothing.

| Setting | Values | Default (design choice) |
|---|---|---|
| ignore patterns | git ignore syntax, case-sensitive | none |
| `-agentfs` | never record content at a path | unset |
| `agentfs.group` | a group kind name (`sqlite`: `""`, `-wal`, `-journal`; never `-shm`) | none (a group of one) |
| `agentfs.maxSize` | a size | unset |
| `agentfs.store`, `filter=lfs` | store hints | git's |
| nested repositories | `exclude` or `working-tree` | `exclude` |
| profiles | name, base, hide patterns | `everything`, `recordable` |

Built in, not settable: `-shm` files, NFS silly-rename names, special files and empty directories are never
recorded. Names that are not `RelPath`s (a `.git` component, git's refused spellings) reach Rules only through
`visibility`; `Everything` shows them, `Recordable` hides them.

## 5. Tests

| Test (through the real artifact unless marked) | Rules out |
|---|---|
| A corpus of paths and rule files gives the same answers as `git check-ignore` and `git check-attr` | drift from git's semantics |
| Through a mounted fork view: a hidden name present and absent gives the same lookup, listing and create answers | existence leaking through a profile (RU3) |
| A landing that adds a hidden name; the fork never sees it | a landed name judged differently from a written one |
| A database with its `-wal` excluded: the fork sees neither file; a save is refused whole | half a database shown or recorded (RU4) |
| A bad attribute, an unknown group kind, an unreadable source: the service refuses to start, naming the line | a typo turning recording or a profile off (RU2) |
| Unit (pure): `Containment::Unknown` answers like `InRepository`; hide patterns never turn `Hide` into `Show` | fail-open on missing facts |

## 6. Costs

1. **Rules are fixed per history-service life:** editing a rule takes a restart.
2. **Tree rule files are ordinary content:** a program cannot steer its own recording.
3. **A move is two judgements:** a removal at the source and a placement at the target, so moving a recorded file to
   a scratch name removes it from the tree's history.
4. **Group kinds list their member suffixes:** a database whose sidecar files no kind lists is recorded without them.
5. **Forks see symlinks read-only** and cannot create them, as today.

## New terms

| Term | Definition | Why needed | Could be the existing term |
|---|---|---|---|
| rule set, rule-set version | the immutable value of every operator rule, and its digest | RU2, RU7 | `Rules` (today's type) |
| group kind | a database group's member and never-recorded suffixes | RU4 | `agentfs.group` |
| base, hide patterns, scratch | a view profile's parts; a private name never committed | the rule that a view shows only what a client may see | recordable names; the tmp layer |
| containment | inside a nested repository or not, as observed | RU6 | nested detection |

