# Domain: Rules (the unbuilt part)

**Partly built.** The operator's rule files, what a tree records, the untracked area and moves across it are built:
[trees](../../trees.md#rules-and-the-untracked-area) describes them, and
[the library](../../library/core.md#rules-and-the-untracked-area) the pure crate `agentfs-rules`. This page proposes
the rest: view profiles, database groups, size and store hints, rule-set versions across view hosts, tree rule files,
and bare-repository detection. A tree volume refuses at open any attribute these would read, naming its line. The
git model ([git-model.md](../git-model.md)) wins wherever the two disagree. Rust sketches show surfaces only. The
commit that lands the last of these deletes this page and moves the behaviour into the library or operator docs.
"Profile" on this page always means a **view profile**; the CSI settings bundle keeps the bare word.

## 0. Guidance this domain rests on

The owner's guidance that applies here is in `OWNER-GUIDANCE.md` at the repository root. Everything else on this page
is a design choice.

## 1. View profiles

**Design choice: a view profile is a base plus operator hide patterns, narrowing only.** `Everything` (the root) or
`Recordable` (forks), plus patterns that hide more; a profile never shows what its base hides (RU3). Today every
client sees everything, so a grant names no profile.

```rust
pub trait Rules {
    /// What may this client's view see and change at n? Existence-blind.
    fn visibility(&self, n: &TreeName, k: EntryKind, v: ViewProfileId) -> Visibility;
    fn view_profile(&self, name: &ProfileName) -> Option<ViewProfileId>; // the only way to get a ViewProfileId
    fn version(&self) -> RuleSetVersion;                                  // carried in a Binding
    /// The database group p belongs to, whole (Views, Trees).
    fn group(&self, p: &RelPath) -> Option<Group>;
}
pub enum Visibility {
    Show,      // seen; a change goes to the client's own tree
    Scratch,   // only the client's own private entry: never committed, never landed
    Hide,      // nothing here, whatever exists: absent from lookups and listings, no creation
    Refuse,    // seen read-only: no change, creation or move of this kind here
}
```

**How a profile answers** (design choice; `Recordable` reproduces the recording engine's sandbox view):

| Kind at p | `Everything` | `Recordable` |
|---|---|---|
| File, recordable if new | Show | Show |
| File the rules skip if new | Show | Scratch |
| File in a group with a member skipped | Show | Hide (the whole group) |
| Dir with nothing recordable possible beneath | Show | Hide |
| Symlink at a recordable path | Show | Refuse |
| Special, or inside a nested repository | Show | Hide |
| matched by the profile's hide patterns | Hide | Hide |

| Caller | Asks | When |
|---|---|---|
| Views | `visibility`, `group` | each lookup, listing, create, rename end, and each name a `TreeMoved` drops |
| Trees | `group` | reporting containment; a pinned snapshot's checkout |
| Clients | `view_profile`, `version` | narrowing a grant, building a `Binding` |

## 2. Database groups, size and store hints

| Setting | Values | Default (design choice) |
|---|---|---|
| `agentfs.group` | a group kind name (`sqlite`: `""`, `-wal`, `-journal`; never `-shm`) | none (a group of one) |
| `agentfs.maxSize` | a size, enforced at the write call | unset |
| `agentfs.store`, `filter=lfs` | store hints | git's |
| profiles | name, base, hide patterns | `everything`, `recordable` |

A group is recorded, shown and hidden whole: a save that would record a member without the rest is refused
(`GroupMemberSkipped`).

## 3. Rule-set versions, tree rule files, bare repositories

- **Several view hosts.** Every view host judges with the history service's rule set: a `Binding` carries a
  `RuleSetVersion` (a digest of the sources and the group kinds); a host with another version reloads before serving.
- **Tree rule files** (`respectTreeRules`): today `.gitignore` and `.gitattributes` in the tree are ordinary content.
  Honouring them would let a program steer its own recording, and a rule-file edit change another tree's recording
  once it lands or syncs.
- **Bare-repository detection.** Today a nested repository is a directory holding a `.git` entry, git's own rule. The
  recording engine also detected bare repositories by their files.

## 4. Invariants

| # | Invariant | Enforced by | Strength |
|---|---|---|---|
| RU3 | Visibility never depends on existence or on who wrote a name; a profile shows nothing its base hides | `visibility` takes no existence or writer argument; a profile holds only a base and hide patterns | compile + construction time |
| RU4 | A database group is shown, hidden and recorded whole | `Group::members()` is computed, never supplied | construction time + test |
| RU6 | A path whose containment is unknown is judged as inside a repository (fail closed) | one exhaustive `match` | compile time |
| RU7 | Every view host judges with the history service's rule set | a `Binding` carries `RuleSetVersion`; another version reloads before serving | boot time per binding |

## 5. Tests

| Test (through the real artifact unless marked) | Rules out |
|---|---|
| Through a mounted fork view: a hidden name present and absent gives the same lookup, listing and create answers | existence leaking through a profile (RU3) |
| A landing that adds a hidden name; the fork never sees it | a landed name judged differently from a written one |
| A database with its `-wal` excluded: the fork sees neither file; a save is refused whole | half a database shown or recorded (RU4) |
| An unknown group kind or profile base: the service refuses to start, naming the line | a typo turning a profile off |
| Unit (pure): `Containment::Unknown` answers like `InRepository`; hide patterns never turn `Hide` into `Show` | fail-open on missing facts |

## 6. Costs

1. **Group kinds list their member suffixes:** a database whose sidecar files no kind lists is recorded without them.
2. **Forks see symlinks read-only** under `Recordable` and cannot create them.

## New terms

| Term | Definition | Why needed | Could be the existing term |
|---|---|---|---|
| rule-set version | the digest of every operator rule | RU7 | `RuleSet` |
| group kind | a database group's member and never-recorded suffixes | RU4 | `agentfs.group` |
| base, hide patterns, scratch | a view profile's parts; a private name never committed | the rule that a view shows only what a client may see | recordable names; the untracked area |
| containment | inside a nested repository or not, as observed | RU6 | nested detection |
