# Domain: Rules

**Layer:** System › **Domain (this page)** › Feature. Binding parents: [../system.md](../system.md) revision 10 and
[../git-model.md](../git-model.md).
**Status:** draft 5, proposed. The target, not today's code; sketches are Rust surfaces. "Profile" on this page always
means a **view profile**; the CSI settings bundle keeps the bare word.

## 0. The owner's words this domain rests on

| # | His words | Date | What it sets here |
|---|---|---|---|
| R7 | "Security boundary. Agentfs is the gatekeeper that exposes only what we want sandboxes to see" | 2026-10-30 | profiles decide what a view shows |
| — | "the sandbox agent should only get write access to the agentfs overlay files"; "potentially should only be able to see the agentfs overlay files at all" | 2026-10-30 | the `Recordable` base (read as: "overlay files" are the names agentfs would record) |
| R8 | a consumer's personal information never enters the agentfs repo | 2026-10-30 | this page names no consumer |
| R9 | "Whole seems best, right? A partial application has unknown side effects" | 2026-10-16 | a database group is shown, hidden and recorded whole |
| R16 | "The various agentfs components should be within the same version family (major for sure, maybe minor too, patch doesnt need to match)" | 2026-11-02 | a change to how rules are judged is a minor release (RU7) |

Everything else is a design choice. Today's behaviour (origin/main b5c0dc8, design §2.4, §4, §5, §8) is evidence.

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
`Recordable` (sandboxes), plus patterns that hide more; a profile never shows what its base hides (RU3).

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
| RU4 | A database group is shown, hidden and recorded whole (R9) | `Group::members()` is computed, never supplied | construction time + test |
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
| Through a mounted sandbox view: a hidden name present and absent gives the same lookup, listing and create answers | existence leaking through a profile (RU3) |
| A landing that adds a hidden name; the sandbox never sees it | a landed name judged differently from a written one |
| A database with its `-wal` excluded: the sandbox sees neither file; a save is refused whole | half a database shown or recorded (RU4) |
| A bad attribute, an unknown group kind, an unreadable source: the service refuses to start, naming the line | a typo turning recording or a profile off (RU2) |
| Unit (pure): `Containment::Unknown` answers like `InRepository`; hide patterns never turn `Hide` into `Show` | fail-open on missing facts |

## 6. Costs

1. **Rules are fixed per history-service life:** editing a rule takes a restart.
2. **Tree rule files are ordinary content:** a program cannot steer its own recording.
3. **A move is two judgements:** a removal at the source and a placement at the target, so moving a recorded file to
   a scratch name removes it from the tree's history.
4. **Group kinds list their member suffixes:** a database whose sidecar files no kind lists is recorded without them.
5. **Sandboxes see symlinks read-only** and cannot create them, as today.

## New terms

| Term | Definition | Why needed | Could be the existing term |
|---|---|---|---|
| rule set, rule-set version | the immutable value of every operator rule, and its digest | RU2, RU7 | `Rules` (today's type) |
| group kind | a database group's member and never-recorded suffixes | RU4 | `agentfs.group` |
| base, hide patterns, scratch | a view profile's parts; a private name never committed | R7 | recordable names; the tmp layer |
| containment | inside a nested repository or not, as observed | RU6 | nested detection |

*History: drafts 1–4 are in git; draft 5 (2026-11-03) drops the merge rule and record classes with the resolver chain and journal.*
