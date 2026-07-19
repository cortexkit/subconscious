# ck-projects v5: the fleet project/workspace registry

Drafted from the ratified fold of #workspace-projects-design (rm_toolu_01ByRWpGZeSjyJC3om18Y7ds):
S1-S11 settled in round 1 (four seats), ratified by Ufuk with three additions S12-S14 and the
store-of-record call (module-owned DB, journal spine). v2 folds the round-1 blind-gate findings
(gate consult ct_...a4fd9ea6f368: alias incoherence, implicit-id hash contract, store-of-record
contradiction, seed conflict rules, generation atomicity) and the seats' [#20][#21][#25][#26]
gate-pile items. Status: DRAFT for re-gate. Owner decision pending with Ufuk.

## 1. Problem and origin case

The fleet runs three divergent project-identity concepts: MC workspaces, AFT roots, and
Alfonso per-directory identity. Every cross-product feature re-solves "what project is this
directory?" independently, and the answers drift.

The origin case is deliberately the NON-SWE user (S14): a construction manager with project
folders scattered across Documents, iCloud, and Desktop. A project is an arbitrary set of
directories registered through the CK App picker — no git, no hierarchy assumption, no repo.
This is why project-owns-N-roots is load-bearing and why unregistered roots must keep
working (implicit projects, never an error). Git-derived identity (MC memory pools, AFT
artifact caches) is an optimization layer for the SWE case, never the registry's foundation.

Consumers waiting: list_peers auto-discovery within a workspace (kills manual peer_add),
MC workspace views, AFT cross-project search scoping, the CK App board picker.

## 2. Shape

A supervised subc-daemon module, `ck-projects` (name ratified 4/4 + Ufuk), following the
CKCRED precedent: shared identity infrastructure owned platform-level, consumed over the
wire; the CK App and a `ck projects` CLI domain are its editors. The daemon is untouched:
routing stays on canonical directory paths (route.open unchanged; cortexkit-paths remains
the canonicalization floor). Project-id re-keying is consumer-side.

The model: a WORKSPACE is a set of project references. A PROJECT is a set of canonical
directory roots. The registry is the TOPOLOGY authority and nothing else:

> The registry answers WHERE a directory belongs, never WHAT a domain subsystem keys its
> state on.

If the registry and git ever disagree about "same project", caches and memory pools follow
git; grouping, scoping, discovery, and UI follow the registry (S7). MC's git:<root-commit>
and AFT's artifact_cache_key are the same key material discovered independently — recorded
as evidence the boundary is right. One ratified exception, MC-declared: for NO-GIT
multi-root projects, MC pools memory by the registry project id itself. This is not S7
drift — the principle is "the registry never replaces DOMAIN identity where domain
identity exists", and this is the case where none does; content identity resumes authority
the moment git exists.

## 3. Enforcement strengths (S13)

The registry owns MEMBERSHIP TRUTH only. Consumers attach policy keyed by workspace id at
their own declared strength, and no consumer may assume another's:

| consumer | property | strength |
|---|---|---|
| MC memory sharing | security boundary | FAIL-CLOSED (degraded answer IS a breach); share_categories stay MC-owned (S11) |
| AFT workspace search | convenience scope | fail-open (registry outage degrades to per-root behavior) |
| ALF scoping / CK App views | representation | no enforcement |

"Workspace" never means one enforcement level fleet-wide; that interpretation is explicitly
rejected.

## 4. Store: journal spine (Ufuk's call, revising S3's file clause)

The module owns its store outright: ordinary cortexkit-store sqlite under
~/.local/share/cortexkit/ck-projects/ (descriptor-declared, single-writer lease, namespaced
migrations, first-class on engram's WAL-incremental backup lane). There is NO config file
for topology data. If the module ever grows behavior knobs, those go to
~/.config/cortexkit/ck-projects.jsonc per the config-home convention; topology is domain
data and lives in the store like every module's domain data.

The spine is an append-only CHANGE JOURNAL; serving tables are a projection of it:

- registry_journal(seq INTEGER PRIMARY KEY AUTOINCREMENT, op TEXT, payload_json TEXT,
  actor TEXT, request_key TEXT UNIQUE NULL, created_at INTEGER)
- Every mutation (register, assign_workspace, upgrade_implicit, remove, seed_import) is ONE
  journal append plus its projection update in ONE sqlite transaction.
- GENERATION = the journal head seq. Monotonic by construction, survives any rebuild
  (projections can be re-derived from the journal), and one mutation is exactly one
  generation transition — MC's exactly-one-HARD contract ("generation changed implies
  topology settled") holds structurally, not by discipline.
- The journal is the audit trail Ufuk asked for: what changed, when, by whom (actor =
  cli | app | seed_import | module) is the primary data structure.
- request_key gives mutation idempotency: a replayed mutation with the same request_key
  returns the recorded outcome instead of appending again.

Projection tables:

- workspace(workspace_id TEXT PK, name TEXT, created_at, updated_at)
- workspace_member(workspace_id TEXT REFERENCES workspace, ref_kind TEXT CHECK(ref_kind IN
  ('local','remote')), device_fingerprint TEXT NOT NULL, project_id TEXT NOT NULL,
  PRIMARY KEY (workspace_id, ref_kind, device_fingerprint, project_id),
  UNIQUE (ref_kind, device_fingerprint, project_id),
  CHECK ((ref_kind = 'local' AND device_fingerprint = '') OR
         (ref_kind = 'remote' AND device_fingerprint <> '')))
  — the UNIQUE constraint makes one-reference-one-workspace schema-enforced for BOTH
  kinds: I2 for locals no longer rests on dual-write discipline alone, and a remote
  reference likewise sits in at most one workspace of THIS registry instance (the remote
  device's own topology is its own business).
  — the S12 reference schema. v1 accepts only ref_kind='local' writes; the CHECK pins the
  fingerprint discipline both ways (a local is exactly the empty fingerprint; a remote
  must carry one), so a malformed remote can never collide with the local PK shape. The
  same (workspace, project) MAY legitimately appear as both a local row and remote rows
  (the same repo on several machines); enumerate reports them as distinct members with
  their refKind, and consumers join twins on content identity (S12), not on the registry.
- project(project_id TEXT PK, name TEXT, implicit INTEGER NOT NULL DEFAULT 0, created_at,
  updated_at)
- project_workspace(project_id TEXT PK REFERENCES project, workspace_id TEXT REFERENCES
  workspace) — I2 (one workspace per project) enforced by the PK; membership rows in
  workspace_member for locals mirror this projection and the pair is written in the same
  transaction.
- project_root(canonical_root TEXT PK, project_id TEXT REFERENCES project, added_at)
  — I1 (one project per root) enforced by the PK.
- derived_root_parent(canonical_parent TEXT PK, project_id TEXT REFERENCES project)
- project_alias(old_id TEXT PK, project_id TEXT REFERENCES project, created_at)

Deletion semantics (journal op `remove`), PER-RELATION disposition (v5: the blanket
"retarget everything on merge" rule was unimplementable — retargeting membership rows
collides with project_workspace's PK and workspace_member's UNIQUE):

| relation | remove (no successor) | remove with successor (merge) |
|---|---|---|
| project (the row itself) | DELETED | DELETED |
| project_root | deleted | RETARGETED to successor (successor absorbs the roots; I1 holds — roots were exclusively the removed project's) |
| derived_root_parent | deleted | RETARGETED (same argument; two-sided ancestry cannot newly conflict because both projects' claims were already mutually valid and now share one owner) |
| project_alias (rows targeting removed) | deleted | RETARGETED to successor + new alias(removed -> successor) written |
| project_workspace | deleted | DELETED — membership NEVER transfers on merge. The successor keeps its own membership (or none); transferring would collide with the successor's PK row or silently re-home the successor. If the operator wants the successor in the removed project's workspace, that is an explicit follow-up assign_workspace. |
| workspace_member (locals naming removed) | deleted | DELETED (mirrors project_workspace in the same transaction, I2 dual-representation preserved) |

MERGE PRECONDITION: none beyond successor-is-live-and-distinct. Cross-workspace merge is
legal precisely because membership does not transfer — the merged roots land in the
successor's existing workspace context, and the report of the remove op names the dropped
membership (workspace id) so the operator sees what detached.

Removing a workspace detaches its projects (project_workspace AND the mirroring
workspace_member locals cleared), never deletes them. All in one journal entry, one
generation.

MEMBERSHIP READ INVARIANT: for local members, workspace_member and project_workspace are
dual representations of one fact and must satisfy: a local workspace_member row exists
IFF the matching project_workspace row exists. Every op that touches either writes both
in its transaction; the rebuild (below) reconstructs both from the same journal entries;
a debug assertion op (`verify` in the CLI) checks the invariant on demand.

REBUILD SEMANTICS: projections are repaired ONLY by ordered journal replay (seq ascending,
applying each op's projection rules to empty tables; generation ends at MAX(seq)). Rebuild
is the sole repair path after any suspected projection corruption — no manual projection
surgery. Both membership representations rebuild from the same entries, so they cannot
disagree after recovery.

ALIAS COLLAPSE OBLIGATIONS (per identity-changing op, enforced by a store invariant "an
alias target must be a live project id and never itself an alias key"):
- upgrade_implicit: writes alias(implicit -> project); no prior aliases can point at an
  implicit id (implicits are never alias targets), so no rewrite needed.
- remove with successor (merge): retargets ALL alias rows whose target is the removed
  project to the successor, and writes alias(removed -> successor), in the same
  transaction.
- remove without successor: deletes all alias rows targeting the removed project.
- Any future rename/split op must state its rewrite rule against the same invariant
  before it ships.

REQUEST_KEY OUTCOME SHAPE: the journal entry stores a FROZEN response blob (the exact
reply bytes of the first execution); a replay returns that blob verbatim. Frozen replies
can therefore carry project ids that were later merged away — consumers that persist any
id from a mutation reply follow the same rule as offline hints: re-validate via
resolve_project_id before durable keying. (One rule for both staleness classes.)

NO-OP MUTATIONS (v5, I8): a mutation whose application would leave every projection table
byte-identical is a SEMANTIC NO-OP: it returns `{noop: true, generation}` (current head)
WITHOUT a journal append — generation moves iff topology moves, in both directions, which
is the property MC's exactly-one-HARD fingerprint requires. Enumerated per op:
- register: all named roots already owned by the target project, no new roots, no
  name/workspace change → no-op. Any conflicting owner is still the I1 ERROR, not a no-op.
- assign_workspace: project already in the named workspace → no-op.
- upgrade_implicit: alias already present and target identical → no-op.
- remove: absent target → typed ERROR `not_found` (no append). Removing an already-removed
  id is not a no-op success; the caller's model is stale and must hear it.
- seed_import: an import whose every pair is skipped/conflicted/duplicate and whose
  resulting topology is unchanged → no-op; the report is still returned (recomputed — it
  is a pure function of the payload and the topology).
REQUEST_KEY interaction: request_key rows are recorded ONLY with journal appends. A no-op
reply is not frozen; a re-sent no-op re-evaluates against current state. This is safe
because every ck-projects mutation is DECLARATIVE (ensure-state, not apply-delta): if the
world changed so the same request is now effectful, executing it is exactly the caller's
declared intent. Errors likewise never append and never freeze.

## 5. Invariants

I1. One root belongs to at most ONE project (PK-enforced; register rejects with the
    conflicting owner named).
I2. One project belongs to at most ONE workspace (PK-enforced; MC's budget/provenance
    ambiguity argument).
I3. Root:project is N:1 BY DESIGN (two checkouts of one repo = two roots, one project). No
    surface assumes 1:1; the seed importer must not derive projects from paths.
I4. Unowned roots never fail-close: they resolve to an implicit project (section 7).
I5. Containment never crosses filesystem boundaries (when checkable; section 6) and
    containment answers carry via:"containment".
I6. The registry never re-implements path identity, and canonical discipline is split by
    op class: MUTATIONS require cortexkit-paths canonical input (typed `not_canonical`
    error — stored state is always canonical), while QUERIES (resolve) accept raw paths
    and canonicalize MODULE-SIDE, echoing the resolved canonicalRoot in the reply.
    Without the split, every non-Rust consumer (Swift app, TS plugins) would need a
    cortexkit-paths parity port just to ask a question. Module-side canonicalization of
    a query path is a pure function of the filesystem, so determinism is preserved.
I7. One mutation = one journal entry = one generation transition (section 4). No observable
    intermediate topology states.
I8. Generation moves IFF topology moves (v5): semantic no-ops and errors never append or
    bump; every append changes at least one projection row. Together with I7 this is MC's
    exactly-one-HARD contract stated as a store invariant.

## 6. Resolution

resolve { canonicalRoot } -> { projectId, workspaceId?, projectName?, via, gone, generation }

Order:
1. Exact project_root match -> via:"root".
2. Nearest registered derived_root_parent whose canonical path is a PREFIX-OR-EQUAL (path
   component boundary; equality explicit — v5 closes the strict-prefix reading under which
   the parent path itself would fall through to implicit) of the query -> via:"containment".
   Nested parents: nearest (longest) wins. The parent path ITSELF resolves to its project
   via containment (it is project infrastructure, e.g. the per-repo worktree pool dir).
   ST_DEV CANDIDATE RULE (v5): candidates are evaluated nearest-first. When the query path
   exists AND the candidate parent can be statted AND st_dev differs, that CANDIDATE is
   rejected and evaluation continues with the NEXT-nearest qualifying parent (never a
   direct fall-through to implicit while qualifying candidates remain). When the candidate
   parent cannot be statted (registered parent since vanished), the boundary check for
   that candidate is unavoidably skipped and canonical-prefix matching decides — same
   posture as the query-path-gone case, and consistent with it: filesystem checks apply
   when the filesystem can answer, prefix logic is the fallback truth.
   ANCESTRY VALIDATION IS TWO-SIDED (both directions of the same rule, checked in the
   mutating transaction): registering a derived parent rejects if it is an ancestor of any
   OTHER project's registered root, AND registering a root rejects if it falls under any
   OTHER project's derived parent (`root_under_foreign_derived_parent`, naming the parent's
   owner). Without the second check, the root itself would win by exact match while its
   CHILD paths leaked to the foreign project via containment.
3. Otherwise -> implicit project (section 7), via:"implicit".

ALIAS IS NOT IN THIS ORDER (gate finding 1): aliases key on project IDs, not roots. A root
that was upgraded resolves via its project_root row (step 1). Aliases serve consumers that
PERSISTED an old project id; they use the dedicated op:

resolve_project_id { projectId } -> { projectId, via: "current"|"alias"|"gone", gone:
bool, generation } — TOTAL over all inputs (v5):
- live project id → { projectId (echoed), via:"current", gone:false }
- alias key → { projectId (the live target), via:"alias", gone:false } (chains are
  collapsed at write time: an alias always points at a live project, upgrades/merges
  rewrite existing alias targets in the same transaction, so lookup is one hop)
- anything else → { projectId (echoed back), via:"gone", gone:true } — never an error.
  Unknown-forever and removed-without-successor are DELIBERATELY indistinguishable here
  (a non-merge remove deletes the aliases; retaining tombstones would leak removed
  topology forever for no consumer need). A gone:true answer tells the consumer exactly
  what to do: drop the durable key and re-key from resolve() on the path.
A well-formed but reserved id (pj-implicit1-<16hex>) resolves like any other input: alias
hit if the root was absorbed into a project, gone:true otherwise — consumers holding
offline-derived hints get a truthful answer either way.

Filesystem boundary and existence: when the queried path exists, containment additionally
requires same st_dev as the matched parent (I5). When it does not exist (reclaimed worktree,
stale consumer path), the boundary check is unavoidably skipped, canonical-path prefix
matching decides, and the answer carries gone:true — a structural answer instead of an
error (AFT's dead-worktree lesson). gone is about the QUERIED path, not the project.

enumerate { workspaceId? } -> { workspaces, projects: [{ projectId, name, roots, implicit,
refKind, deviceFingerprint?, lastRouteActivityMs? }], generation } — remote members (S12)
appear flagged refKind:"remote" and are never locally resolvable.

Every reply (resolve, resolve_project_id, enumerate, mutations) carries generation.
Implicit answers do NOT materialize rows and do NOT bump generation (a Query is never a
mutation; gate finding on query-side generation bumps closed).

## 7. Implicit projects and the hash contract

An unregistered root resolves to `pj-implicit1-<16hex>`:

- UNICODE / BYTE-FORM GUARANTEE (adopted from AFT's source answer, channel
  [#37]): cortexkit-paths performs no unicode normalization by explicit
  policy; safety is filesystem-first — realpath returns each component's
  on-disk byte form, and normalization-insensitive filesystems (APFS)
  converge NFC/NFD spellings of an existing directory to the same bytes.
  The non-existent-path corner is closed by EXISTENCE-AT-REGISTRATION:
  register and derived-parent declaration reject non-existent roots with a
  typed error, so every hash-contract input stored by the registry is an
  on-disk byte form. resolve() may still mint an implicit id for a
  non-existent queried path (gone:true answers); such ids inherit the
  offline-derivation caveat — hints, never persistable.
- ALGORITHM (version tag "1", AFT's proposal, MC co-signed): blake3 over the UTF-8 bytes of
  the cortexkit-paths canonical absolute path, first 16 lowercase hex characters. A future
  algorithm change mints `pj-implicit2-...` — distinguishable, never silently colliding.
- Deterministic across restarts and machines FOR THE SAME CANONICAL PATH STRING; the doc
  makes no cross-machine claim beyond that (different mount layouts produce different
  canonical paths and thus different implicit ids — federation joins use content identity
  where available, S12).
- NAMESPACE RESERVATION (v5): every `pj-implicit<n>-` prefix is RESERVED for
  module-derived ids. register/upgrade_implicit/seed_import reject a caller-supplied
  projectId under a reserved prefix with typed `reserved_project_id_namespace`. Seed-minted
  ids (§10) use `pj-<16hex>` outside the reserved space. Without the reservation, an
  explicit project could squat an id the hash contract may later derive for a real root.
- TRUNCATION-COLLISION HANDLING (v5): 16 hex = 64 bits; a collision between two distinct
  roots is cryptographically negligible but DEFINED: if the universal-alias write finds
  project_alias(old_id) already present pointing at a DIFFERENT project (and the id is
  implicit-derived from a different root), the alias write is SKIPPED, the collision is
  recorded in the journal entry's payload, and the mutation reply carries a typed
  `implicit_alias_collision` warning. Consequence is bounded by design: resolve() on
  either root still answers correctly (root/containment precede implicit); only the
  offline-hint path for the second root degrades — and offline hints are already
  non-persistable and re-validated by contract.
- IMPLICIT ALIASES ARE UNIVERSAL: every root entering a project — via register,
  upgrade_implicit, or seed_import — writes project_alias(pj-implicit1-<hash(root)> ->
  projectId) in the same journal entry. Online implicit answers are legitimate registry
  answers (via:"implicit", carried generation) that consumers may persist; without the
  universal alias, a project registered directly over roots A and B would orphan a
  consumer that persisted the registry's own prior implicit answer. upgrade_implicit is
  sugar over register plus the explicit prior-id intent; the alias write is identical.

OFFLINE HONESTY (gate finding 2 + AFT [#20]): a consumer that cannot reach the module may
derive the implicit id client-side from the same contract, but the guarantee is narrow:
offline and registry answers agree ONLY for roots that are genuinely unregistered. A root
already registered (e.g. checkout B of a project registered via checkout A) yields an
offline implicit id the registry never answered and never will (no alias exists — the root
was never implicit). Therefore: offline-derived ids are NON-PERSISTABLE HINTS. Consumers
must re-resolve before durably keying anything on a project id obtained offline. (MC
complies by construction — absent signal freezes its cached fingerprint; AFT never persists
registry answers; Alfonso's peer scoping must follow the same rule when it re-keys.)

## 8. Operations

Query: resolve, resolve_project_id, enumerate, journal_tail { afterSeq, limit } (the audit
read), health/status (snapshot-answered per module conventions).
Mutate (each = one journal entry, idempotent via request_key): register, assign_workspace,
upgrade_implicit, remove, seed_import.

register { projectId?, name, workspaceId?, roots: [...], derivedRootParents?: [...],
requestKey } — creates or extends; rejects I1/I2/derived-parent-ancestry conflicts naming
the conflicting owner. All paths must be canonical (I6).

Operator access is journaled CLI, not hand-editing: `ck projects` (git-style external
dispatch to these ops, same pattern as ck quota / ck auth). The CK App drives the same ops.

## 9. Federation (S12) — schema now, transport never

Workspace membership is a set of project REFERENCES: local (project_id) or remote
(device_fingerprint, project_id). v1 builds local-only; the schema (section 4) already
carries the reference shape. resolve() stays machine-local forever: a remote project never
resolves, it only enumerates, flagged. The registry has NO transport: each consumer uses
its existing cross-machine lane when it meets a remote member (AFT search via callosum's
fed-exposed aft, MC memory via engram sync + team-memory, ALF via fed peer identity).
Local/remote twins of one repo join on content identity (git:<root-commit>) where git
exists — the same key team-memory already assumes.

## 10. Seed import (S11) — deterministic or rejected

seed_import { source: "mc", requestKey, payload: { pairs: [(canonical_root, mc_identity)],
workspaces: [...], members: [...] } }:

- Grouping: roots sharing an mc_identity form one project (I3). Path-derived grouping is
  forbidden.
- GENERATED OUTPUT DETERMINISM (v5 — the same corpus forces one topology AND one report):
  - Processing order: identity groups in lexicographic mc_identity order; roots within a
    group in lexicographic canonical-path order; report entries in processing order.
  - Minted project ids: `pj-<16hex>` = first 16 lowercase hex of
    blake3("seed1:" || mc_identity) — deterministic, outside the reserved implicit
    namespace, versioned by the seed1 domain tag. If the minted id already exists in the
    store (re-import into a non-empty store), the EXISTING project is extended iff it was
    seed-minted from the same mc_identity (recorded as `rejoined`); otherwise the group is
    recorded `conflicted` and excluded (no silent adoption of a caller-created id).
  - Project names: the export's per-identity name when present, else the last path
    component of the lexicographically-first root in the group.
  - Workspace mapping: export workspaces are created verbatim (id, name); each surviving
    group maps to the single workspace the export's members claim for its mc_identity
    (post rule-2: multi-claim → detached + recorded).
  - MULTI-OWNER identity_split (v5): when rule 4 skips a group's roots to MORE THAN ONE
    existing owner, the identity_split entry names the mc_identity, EVERY existing owner
    id with its skipped roots, and the new project id (if any roots remained to group) —
    one entry per mc_identity, however many owners.
- CONFLICT RULES, all deterministic, applied in this order, every decision recorded in
  the import report:
  0. IDENTITY-CLASS PRECEDENCE (MC's cooldown semantics, co-signed [#33]): for a root
     observed under both a git:-class and dir:-class identity, the git:-class identity
     wins — the dir: row is a documented transient-fallback artifact of MC's git-timeout
     cooldown, and MC's own store prefers the last-known-good git identity the same way.
     A root observed ONLY under dir:-class is a genuine no-git directory (S14 case) and
     groups by that identity normally. MC's export carries per-row identity_class
     (git|dir) and, where known, a resolved-through-cooldown flag, so this rule applies
     from data rather than re-derived semantics.
  1. One root under multiple mc_identities AFTER rule 0 (i.e. genuinely conflicting, e.g.
     two different git identities): the root is EXCLUDED from grouping (falls to
     implicit), recorded as conflicted. No guessing.
  2. A grouped project claimed by multiple workspaces: assigned to none (detached),
     recorded. I2 is never auto-resolved by preference.
  3. Duplicate pairs: idempotent, collapsed.
  4. A root already registered (import into a non-empty store): the existing owner wins
     (I1); the pair is recorded as skipped. When this splits an mc_identity group — some
     of its roots skipped to an existing project, the rest grouped into a new one — the
     report additionally records an explicit `identity_split` entry naming the
     mc_identity, both project ids, and the roots on each side. A silent split would
     understate an I3 violation; the split still happens (I1 outranks I3 on import), but
     it happens on the record.
- ATOMICITY: the whole import is ONE journal entry and one generation transition — this is
  how seed_import satisfies I7's contract ("one generation transition per observable
  topology state"): the pre-import and post-import topologies are the only two observable
  states, with one seq between them. Partial failure aborts the transaction entirely. Idempotency via requestKey: a replay
  returns the recorded report.
- MC ships the export op when this shape freezes; the freeze-candidate field list is:
  pairs: [(canonical_root, mc_identity, identity_class: git|dir,
  resolved_through_cooldown?: bool)], workspaces, members.

## 11. Liveness hints

enumerate's lastRouteActivityMs comes from the daemon's route-activity view, consumed by the
registry on a slow cadence and mapped roots->projects registry-side (the daemon learns no
registry concepts). AFT may supplement from live_actor_roots/health metrics (offered [#20]).
Hints are best-effort display data, never correctness inputs. Exact feed shape is the one
open wire item, settled with SUBC before build.

## 12. Failure semantics

- Module down: consumers degrade to current behavior (raw-directory keying). Offline
  implicit derivation per section 7's honesty rule — hints only, never persisted.
- Store corruption: projections re-derive from the journal; the journal is the engram
  restore unit (restoring it restores full history, not a snapshot).
- Non-existent queried paths: structural answers with gone:true (section 6), never errors.
- resolve/resolve_project_id/enumerate never fail-close on unregistered input (I4).

## 13. Consumer re-keying (unchanged from v1, none of it in this module)

- Alfonso: peer scoping re-keys to resolve().projectId (re-resolve before persisting, §7);
  worktree pool parents registered as derived_root_parents; list_peers workspace
  auto-discovery via enumerate.
- AFT: workspace search via enumerate; cache identity stays git-derived (S7).
- MC: views via enumerate; membership authority moves to the registry; memory pools keyed
  by git identity, except no-git multi-root projects keyed by registry project id ([#26]).
- CK App: board picker via enumerate + liveness hints; edits via the mutation ops.

## 14. Open items

- Liveness feed shape with SUBC (§11).
- journal_tail pagination/retention (journal grows forever; is pruning ever allowed, or is
  it append-only for life? position: append-only, it's small).
- Owner: build/operate assignment is Ufuk's call, taken to him with the gated doc.
- cortexkit-paths canonicalization residuals (v2 gate): whether the canonical form pins
  unicode normalization and case-folding on case-insensitive filesystems is a PROPERTY OF
  CORTEXKIT-PATHS, not of this design; the implicit-id contract inherits whatever the
  canonical string is. Action: verify at cortexkit-paths source during implementation and
  record the answer in the hash-contract section; if canonicalization is ambiguous there,
  that is a cortexkit-paths defect to fix fleet-wide, not a ck-projects workaround.

## 15. Gate and adversarial-pass dispositions

v4 -> v5 (FULL-PANEL AUDIT ct_...5bd5ae19bb00, 3 families, BLOCK with 6 findings — all
folded above): (1) per-relation merge/remove disposition table replaces the blanket
retarget rule (membership never transfers; project row deleted); (2) resolve_project_id
made total with via:"gone"/gone:true for unknown and removed ids; (3) I8 no-op semantics —
no-ops and errors never append, request_key freezes only appended entries, declarative-op
re-evaluation argument recorded; (4) seed output determinism — processing order, minted-id
derivation (blake3 seed1 domain), name rule, workspace mapping, multi-owner identity_split;
(5) containment: prefix-or-equal explicit + st_dev next-nearest-candidate rule +
unstattable-parent posture; (6) pj-implicit namespace reservation + truncation-collision
skip-record-warn handling. Non-blocking residuals accepted as build-time pins:
assign_workspace field spec, detached-project enumerate visibility, explicit-id minting
when register.projectId omitted.

v3 -> v4 (SUBC adversarial pass [#31], reconciled [#34]): F1 universal implicit aliases
(every root entering a project writes its alias \u2014 register/upgrade_implicit/seed_import);
F2 UNIQUE(ref_kind, device_fingerprint, project_id) on workspace_member; F3 seed rule 0
with MC's both-identities-observed precision verbatim + export field markers; F4 withdrawn
by SUBC (two-sided rejection stands \u2014 allow-with-warning would leak child paths inside one
tree); F5 I6 split (mutations canonical-required, queries module-canonicalized).

### v2 gate disposition (single-seat AUDIT, grok-4.5; sol seat died zero-output)

Blocking findings folded into this v3: c7 (remove now clears BOTH membership
representations in-transaction; membership read invariant + rebuild-by-replay added), c10
(two-sided ancestry validation: late root registration under a foreign derived parent
rejects), c12 (seed report gains the explicit identity_split class). Pins folded: c4
(rebuild = ordered journal replay, sole repair path), c5 (request_key returns the frozen
first-execution blob; consumers re-validate persisted ids via resolve_project_id), c6
(per-op alias rewrite obligations + the alias-target-liveness invariant), c8 (read
invariant + dual-write rule), c9 (CHECK on ref_kind/device_fingerprint; local+remote twin
coexistence stated). c3's residual is §14's cortexkit-paths item.
