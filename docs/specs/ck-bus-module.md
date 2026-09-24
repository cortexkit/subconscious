# ck-bus: the supervised NATS message-plane module

Status: SETTLED for slices 0 and 1 (merged); REVISION 2 for everything after them. This
is the build specification for `crates/ck-bus`, and it is the authority a slice
implements against. Where this text and the chair-ruling index at the end disagree, the
SECTION is what a slice implements and the disagreement is a defect to report.

## Revision 2 (2026-09-24)

What changed from r1, and why. r2 folds r1's two in-place corrections (2026-09-20,
2026-09-23) and the sources below into the sections they govern.

1. Credentials follow design D: prefrontal `48c83a68e` (the foundation amendment,
   agreed by SUBC, CKCRED and ALF), `0eb12229f` (the root grant syntax) and
   `b9e827c69` (the attested principal comes from the spawn nonce, not the `reserved`
   flag).
   - Root keys come from an operator ceremony. Per-process keys live in ck-bus's
     memory. JWTs are signed through the existing `credential.sign`. Revocation is NATS
     state.
   - The four vault ops r1 waited on will not be built, and their gates are DELETED:
     `ckcred-mint-unlanded`, `ckcred-delete-unlanded`, `ckcred-audit-read-unlanded`
     and `ckcred-admission-limit-unlanded`.
   - Also deleted: ruling R9's root-record rule, the root record names, the
     mint-intent and orphan store shapes, and revocation step (4).
2. Serving side: R10 is amended as R12 (a chair decision on this revision). A harness
   signer with throwaway keys proves ck-bus's behaviour against a real `nats-server`.
   Authorization rows run only against a real claustrum (`claustrum-binary-absent`).
3. Federation role, added from `docs/designs/nats-federation.md` r3, CALLO's report of
   callosum `f5d34f8`, and CKCRED's `credential.open` contract. All federation slices
   come after the single-machine plane, which stays fully usable with no hub.
   Unmeasured stock-`nats-server` behaviour carries `nats-federation-rig`.
4. The machine id comes from the daemon (`docs/designs/machine-id-and-required-capabilities.md`;
   subconscious `cac1e9bd`, `ModuleHelloAckBody.machine_id`). `{acct}` is
   `box_<machine id>`.
5. Slice record. Slice 0 merged at `0b6f32da` (merge `3050f7b1`). Slice 1 merged as
   `2b7ccc94`, with the follow-up `4bc1d17e`. Slice 0's
   `tests/fixtures/FIRE-TIME-RECORD.md` discharged
   `foundation-disposition-table-unquoted` and `health-down-escalation-unpinned`, and
   replaced `cortexkit-paths` with `cortexkit-store-types`. Its vendored foundation
   predates the amendment, so the disposition gate reopens until slice 2 re-vendors it.
6. Slices 2 onward are renumbered and re-fenced. Reports use the r2 numbers.

Foundation: prefrontal `docs/specs/nats-message-plane-foundation.md` at `b9e827c69`,
its amendment normative over its earlier Credentials lines. Bus primitives are consumed
from cortexkit/commons at `4f09c7c7c7f86394d21abde6ed3f97b582ee4d28`
(`cortexkit-bus-trait`, `-inmemory`, `-naming`, `-nats`) and `cortexkit-store-types`
0.2.2 from the same repository.

Slices are dispatched directly against this text, one at a time, and reviewed and
merged individually. A citation to a dependency's behaviour cannot be reviewed from
inside this document, so every slice re-reads what it cites at fire time.

## Intent

Build `ck-bus`, the supervised module that owns the NATS message plane the foundation
defines. The foundation shipped the trait layer, the naming and limits crate, the
`async-nats` backend and the acceptance rig. Every arm that needed the module recorded
`bus-module-unlanded` and ran against the rig fixture F-PROV instead. This work replaces
F-PROV with the module. On one machine it provides credential issuance and revocation
under design D, the census register, stream and consumer bootstrap, the sentinel probe,
the dead-letter consumer and the spawn-stream consumer. Across machines it adds the
federation account, the leaf configuration read from Callosum, and sealing and opening
of cross-machine deliveries.

Completion is stated here and nowhere else. The work completes when every row of the
acceptance ladder either gates against the module or carries a named skip whose
dependency is external and unlanded at fire time. F-PROV deletion is decided per gate,
not by a blanket rule. `bus-module-unlanded` leaves a row's recorded skips, and the
F-PROV rows that row replaces are deleted, when two things hold: no row still runs
against F-PROV, and every gate whose "Blocks" cell covers those rows is discharged.
Each gate names the repository and owner that can discharge it, because no
subconscious slice may discharge a gate whose fix lands elsewhere. Where the named party
has not agreed to the work, the cell says "unagreed". A row gating against the harness
signer counts as gating against the module: the observable it claims is ck-bus's
behaviour. A vault-authorization row counts only when it ran against the real binary.

| Gate | Discharged by (the observable a slice checks at fire time) | Owner | Blocks |
| --- | --- | --- | --- |
| `spawn-stream-unlanded` (C) | `supervisor.spawn_snapshot` and `supervisor.spawn_subscribe` in `server.describe`'s `subc_ops`, observed list recorded | SUBC, wi_081820c5; both landed at 6844bbb5b7f9 | only a row whose fire-time read lacks a name |
| `membership-contract-unpinned` (D) | a slice quoting the foundation's membership op, authorized caller and the event the 10 s bound runs from, from the vendored copy | prefrontal foundation author (ALF) | the membership row |
| `foundation-disposition-table-unquoted` (D) | slice 2 re-vendoring the foundation at `b9e827c69` or later and re-mapping every F-PROV row onto a row below or a named exclusion | subconscious seat, slice 2 | the completion claim only |
| `prefrontal-seat-unnamed` (S) | an owner naming the prefrontal seat, campaign, and the path by which the built `ck-bus` reaches prefrontal's rig | operator | A6/A7 re-run, golden regeneration, F-PROV deletion (a prefrontal commit) |
| `health-class-carrier-unpinned` (C) | the byte-exact class written at `metrics.class` in `health.check` returned byte-exact by `supervisor.health_probe` in the same run | subconscious seat; a SUBC daemon change outside this work if a leg cannot carry it | the health and sentinel rows |
| `naming-constructor-absent` (C) | `cortexkit-bus-naming` at the pinned sha constructing every name this work introduces; commons branch filed for SUBC to merge, re-pin | commons; ALF authors, SUBC merges | the rows that emit the missing name |
| `claustrum-binary-absent` (C) | a claustrum binary found through `CK_CLAUSTRUM_BIN` (and a `ck` through `CK_CK_BIN`) at fire time, versions recorded | SUBC | the vault-authorization and signer-shape-against-real rows; a skip here is LOUD and never a pass |
| `user-jwt-ttl-unpinned` (S) | the foundation naming the per-process user JWT lifetime | ALF (foundation); unagreed, no value proposed by any party | the expiry arm of the signer-outage row only |
| `nats-federation-rig` (S, per behaviour) | the federation rig reporting the named stock-`nats-server` behaviour measured, with the server version | SUBC (rig task in flight) | each federation row naming a behaviour it depends on |
| `fed-foundation-amendment-unlanded` (S) | a foundation amendment pinning the federation account subject grammar, the cross-machine participant publish grant, the federation stream limits and the re-sync bound | ALF; agreed in principle in nats-federation r3, not written | every federation row that emits a federation name or stream |
| `kemkey-open-unlanded` (S) | `credential.open` and `ck auth mint-kem-key` in claustrum's served vocabulary with store migration 11, per CKCRED's contract (claustrum campaign `ct_00000000-0000-4006-98d8-bc6a13f46a50`, fired, not landed) | CKCRED | the real-binary half of the open rows, and deployment of inbound federation |
| `own-key-record-op-unnamed` (S) | CKCRED naming the claustrum op returning this machine's `seal_x25519` and `msgsig_ed25519` public halves and the record generation | CKCRED; unnamed | no ck-bus row (ck-bus seals with peers' keys and opens by id); callosum's own-record announcement |
| `leaf-credential-ceremony-unlanded` (S) | the pairing ceremony creating the hub account and each machine's leaf credential | CALLO with CKCRED; the ceremony shape is not written | the leaf link row |
| `leaf-signing-shape-unchosen` (label) | SUBC and CKCRED choosing a shape; r3 recommends shape 3 pending `SignatureCB` in the pinned server | SUBC, CKCRED | nothing; the link arm runs labelled and claims no seedless leaf auth until chosen |

The markers are S (standing, discharged only when an external dependency lands), D
(dischargeable by the slice itself by citing or quoting) and C (conditional: recorded
only when the named condition is observed at fire time). This is the only such list.
Four deployment gates, defined under Credentials, block no acceptance row, only the
operator placing `ckbus` on a real machine: `root-ceremony-unrun` (operator with
CKCRED), `server-config-writer-unnamed` (SUBC with the operator), `machine-id-absent`
(SUBC: a daemon at `cac1e9bd` or later deployed) and `ckbus-client-ops-unagreed` (SUBC
for subc-client-rs, ALF for prefrontal-core). Two names are row-level conditions, not gates:
`a1-signal-unix-only`, the single platform skip, and `stub-reply-shape-unrecorded`.

Discharged and carrying no row: `route-target-ids-unnamed` (R10, R12),
`nats-server-argv-injection` (subconscious `97b20cce`, subc-daemon 0.20.5: a
`protocol: "none"` spawn carries no `--subc` and no launch nonce, `SUBC_MODULE_ID`
staying; A1 re-measures it), `health-down-escalation-unpinned` (slice 0; Health answer), and
`paths-crate-function-absent`, which named the wrong crate.

Until the gates covering a given F-PROV row are discharged, module slices may land and
report green per row, but may not report that row's fixture retired. A row that gates
while its dependency is absent is a rig failure, not a pass.

Location and identity (operator ruling, room `nats-plane: message bus foundation` #10;
SUBC 2026-09-19): the module lives at `crates/ck-bus/`, binary `ck-bus`, module id
`ckbus`, supervised by subc. Every slice fires from the subconscious seat and fails its
own arm if it writes outside the subconscious worktree. Three obligations land in
prefrontal: regenerating `tests/grants/permission_golden.txt`, deleting F-PROV, and
re-running A6/A7 against a built `ck-bus`. They are carried by `prefrontal-seat-unnamed`
and no subconscious slice claims them.

Three SUBC rulings bind every slice, verbatim. (1) `protocol: "none"` is THE
supervision mode for `nats-server`, never a second HELLO-less path. (2) The daemon holds
no credential and has no Claustrum route, ever. (3) `ck-bus` is a supervised module
whose liveness is the sentinel probe's to report, not the supervisor's probe.

## Non-goals

- Porting any workload (rooms, wakes, peer deliveries, effect intents, Pi pushes) onto
  the bus. Those are later prefrontal slices; this work lands the owner they need.
- Denying a child process the parent's launch-nonce principal. The foundation records
  it as an observation; it stays one here.
- Any daemon change. No slice edits the daemon; the supervisor holds no credential and
  writes nothing to the census. Where a gate names a daemon change as its discharge,
  that change is SUBC's work outside these slices.
- Authoring commons changes as slices. A slice that needs one files it as a branch for
  SUBC to merge and re-pins.
- Building the hub, the hosted hub, the pairing ceremony, the key-record exchange, or
  the leaf credential. Those are CALLO's, CKCRED's and the operator's. ck-bus consumes
  `callosum.hub_read` and `callosum.peer_keys_read` and the vault ops, and runs no
  ceremony.
- Cross-machine rooms beyond delivery (the nats-federation r3 decision 1, OPEN):
  ck-bus seals and opens ROOM deliveries like PEER and WAKE, and nothing here makes a
  room readable while its owning store is offline.
- Collecting streams left behind by a machine-id change (Credentials, below): ck-bus
  names them and deletes nothing.
- Windows. The unix scope is Linux and macOS; non-unix is `a1-signal-unix-only`.
- A cold-start admission limiter. The foundation's herd bound named a vault-side
  limiter that the amendment dropped. See Open questions.

## Constraints

Evidence currency and citation form. Every citation is a repository-relative path plus
an item name (`crates/subc-control/src/lib.rs::SupervisorObservedProcess`), read at the
commit named beside it, or at the evidence snapshot 6844bbb5b7f9 where none is named.
`ev-N` ids and line ranges are not citation vocabulary. HEAD moves, so every slice
re-reads at fire time the file and item it depends on and records the path, item and
observed shape. A citation that no longer holds is a gate to record, not a fact to
carry forward. Where this section names an item but not its file, the slice records the
path. Facts reported by another seat and not read by this spec are marked as reports.

Repository and dependencies.
- Crate `crates/ck-bus`, package and binary `ck-bus`, module id `ckbus`. The daemon
  injects `SUBC_MODULE_ID` on every spawn; the crate never sets it and reads it as
  given. The root `Cargo.toml` `members` entry and its one-line description in the
  trailing comment block landed in slice 0.
- Acceptance layout, fixed so no later slice edits a manifest. Cargo builds only the
  direct `.rs` children of `crates/ck-bus/tests/` as integration targets, so every row
  file is a direct child at the path the Sequencing table names, auto-discovered, with no
  `[[test]]` entry. The shared harness is `crates/ck-bus/tests/harness/mod.rs` and its
  submodules, which each row file pulls in with `mod harness;`. A slice that believes it
  needs a manifest target stops and reports.
- Test inventory check, stated once. Every slice's done test runs
  `cargo test -p ck-bus -- --list` and reads the combined cargo output. For its own
  target and every row target landed so far, it requires cargo's
  `Running tests/<file>.rs` line and at least one listed test name under it. A target
  with zero listed tests fails. A build with no `Running tests/` line for a landed
  target is never reported green.
- Unix scope is Linux and macOS. `a1-signal-unix-only` records one condition, a
  non-unix host (Windows), where the A1 rows assert nothing. No row is restricted to
  Linux. The argv read is the portable `ps -ww -o args= -p <pid>`.
- `ck-bus` dev-depends on `subc-daemon` with `features = ["test-support"]` as a
  workspace path dep resolving to subc-daemon >= 0.20.5. The floor is checked against
  the workspace source, not a wire op: `ServerDescribe` carries `protocol_ver`,
  `build_git_sha` and `build_lock_digest` but no crate version. Every such test calls
  `BootstrapConfig::with_capture_logs_dir` and `with_terminal_journal_path` with
  fixture-tree paths. The capture directory carries stdout and stderr only, so no arm
  reads argv from it. Wire types come from `subc-protocol`, `subc-transport` and
  `subc-control` directly, never through `subc-daemon` re-exports (SUBC, 2026-09-19).
  The machine id needs subc-protocol >= 0.25.0 on the module side, which carries
  `ModuleHelloAckBody.machine_id`.
- Bus primitives come from cortexkit/commons in the three-field form
  `{ version, git, rev }`, never a branch (SUBC, 2026-09-20), pinned at
  `4f09c7c7c7f86394d21abde6ed3f97b582ee4d28`. `cortexkit-store-types` 0.2.2 is added the
  same way. Commons changes are authored by ALF and merged by SUBC. A slice that needs
  one files it as a branch and re-pins to the merged sha, and the gate it carried flips on
  the new pin. `091bd1fc0` is the prefrontal commit at which the rig last regenerated
  the permission golden; it is provenance only, never a manifest pin.
- NATS JWT and nkey encoding: a slice may add a crate for nkey encoding and JWT
  construction (the `nkeys` crate or equivalent), pinned exactly and named in its
  report. No slice adds a dependency that holds or persists a seed outside process
  memory.
- Golden diff rule, stated once and identically in the A9 row: the regenerated golden
  differs from its prefrontal-era predecessor in exactly the header line naming the
  generator plus zero or more added subjects, each named in the slice report. Every
  subject, stream and grant the foundation enumerates is byte-identical, and nothing is
  removed or modified.
- Every subject, stream, bucket, grant, consumer and credential-id name comes from
  `cortexkit-bus-naming`; the module emits no name of its own. A refused name is a
  refused act. At the pinned sha the crate constructs `c_ckbus_dead`
  (`AccountNames::consumer_name`) and not the census key grammar (slice 0 record). The
  names this revision adds and the crate must construct before a row emits them are:
  the census key grammar, `box_<machine id>`, the root credential ids (Credentials),
  the federation account's inbox and outbox families, and the federation stream
  names. `naming-constructor-absent` is recordable by any row, naming the constructor
  it stopped on. The row never emits the literal or floats the pin.
- Foundation text reaches a slice by one path: a verbatim vendored copy at
  `crates/ck-bus/tests/fixtures/foundation/nats-message-plane-foundation.md` with a
  `SOURCE` file naming the prefrontal commit and sha256. Slice 0 vendored
  `76a910737`, which predates the amendment. Slice 2 re-vendors at `b9e827c69` or later
  and rewrites `DISPOSITION-MAPPING.md` for the amended rows. A2's child-signing half is
  now answered by bus-held seeds, and the audit-read references are gone. A quoting
  slice shows the quoted material byte-for-byte in the copy. With no copy and no
  operator-supplied extract, it records the D gate and reconstructs nothing.

Durable store: eight shapes and nothing else, under the store root below.
- `spawn_cursor.json`: last processed spawn cursor.
- `epoch_high_water.json`: the highest (generation, epoch) issued per module.
- `sentinel_verdict.json`: the last verdict plus the incarnation id that wrote it.
- `account.json`: the machine id and `{acct}` this store last served, and the box
  account's identity public key (`account_public`, the account id), which ck-bus
  generated once in memory at first boot. The seed is never kept. Absent, it is not
  a reason to create: boot lists the resolver's accounts and adopts one named
  `{acct}` before it creates anything.
- `own_users.json`: the public keys of this incarnation's box-account, system-account
  and, once federation lands, federation-account users, plus the earlier box users
  still pending revocation. Written before each first connect so the next
  incarnation can revoke its box users.
- `revocation_progress/{module_id}.g{generation}.e{epoch}.json`, one file per
  in-flight revocation. It carries that identity, `highest_completed_step` (inclusive
  domain 0..=3), the user public key, the `user_jwt_id`, and the kick target.
- Two federation shapes that exist only once the federation slices land:
  `fed_send/{recipient_machine_id}.json` (the next sequence, plus at most one reserved
  (sequence, delivery id) pair) and `fed_recv/{sender_machine_id}.json` (the durable
  high-water mark and the gap list).

No shape holds key material: a seed is never written anywhere (Credentials).

Durability and damage.
- Every write is an atomic durable replacement: a sibling `*.tmp` in the same
  directory, fsynced, renamed over the target, the directory fsynced. A progress
  record is never rewritten in place. A stale `*.tmp` is ignored at start and removed.
  A file that fails to parse, or is short, is damaged. For the per-identity shapes,
  identity comes from the filename, never the body.
- `spawn_cursor.json` damaged: read as absent, so the module snapshots and
  reconciles. `sentinel_verdict.json` damaged: read as absent, which is already this
  process's start state.
- `own_users.json` damaged: left untouched, path named in the report and start-up
  log. The previous incarnation's users are then not revoked by name; they die by
  expiry once `user-jwt-ttl-unpinned` is discharged, and until then the report names
  the residual. Nothing guesses a key.
- `account.json` damaged: fails closed. The module refuses to create streams or issue
  credentials, answers health down with class `Unavailable` naming the file, and never
  rewrites it. A wrong `{acct}` would build a second plane silently.
- `epoch_high_water.json` damaged: fails closed per generation. The entry for a
  (module, generation) is fsynced before the JWT for that epoch is signed, so it bounds
  every epoch issued. The census cannot reconstruct it: a revoked epoch's entry is
  gone. So for an unreadable entry the module issues no further epoch for that
  generation. It leaves the file untouched and names it, refuses re-issue and
  membership mints for the generation with a recorded refusal, answers health down
  with `Unavailable`, and keeps serving the connections already open. A generation
  spawned afterwards gets a fresh entry.
- `revocation_progress` damaged: read as `highest_completed_step: 0` with its key,
  jwt id and kick target lost. Recovery takes the census entry for the filename's
  module id, and there are three outcomes.
  - The entry is at exactly the filename's (generation, epoch): re-derive the inputs
    from its `credential_public` and `user_jwt_id`, then replay from step (1). Every
    step is idempotent.
  - The read succeeds and the entry is absent or at another pair: step (2) committed,
    so step (1) did too. A pushed revocation disconnects the revoked user by itself (the
    foundation amendment's measured basis: 1.44 ms and 4.85 ms). So recovery clears the
    file, issues nothing, and names the case.
  - The read fails: recovery defers and retries once per sentinel period.
- The two federation shapes are durable before the act they cover (a reservation
  before publish, a high-water mark before ack). A damaged `fed_recv` file refuses
  delivery from that sender until repaired: it quarantines, never guesses a high-water
  mark, and names the file. A damaged `fed_send` file refuses sealing to that
  recipient.

Store root: `cortexkit_store_types::module_data_dir("ckbus")` (`cortexkit-store-types`
0.2.2 at the commons pin, recorded by slice 0). The returned directory is used as-is and
must be absolute. The module refuses to start unless `XDG_DATA_HOME` or `HOME` is set
and the root is absolute. The literal `~/.local/share` is never used. Every acceptance
run sets `XDG_DATA_HOME` into its fixture tree and asserts nothing was written under the
operator's real data home. The store is raw module state, not the managed sqlite store,
so `ck-bus` does not open the `StorageDescriptor` on HELLO_ACK. The census lives in the
bucket, root keys live in Claustrum, per-process keys live in memory, and nothing else
is durable.

Module declaration and acceptance fixture (slice 0, merged).
`crates/ck-bus/tests/fixtures/subc.jsonc` carries these blocks: the `ckbus` block; the
`nats-server` block; the stand-in child declared twice, once `protocol: "none"` and
once with the key omitted, both naming `tests/support/standin_child.sh`; and the
`claustrum` and `callosum` stub blocks. A later slice adds a module block (the harness
signer, a real claustrum, a second daemon's blocks) only through the harness's
per-test config renderer: it adds a renderer registration in its own harness
submodule, never by editing the fixture file. A slice that cannot stops and reports.
No slice edits an operator-facing deployed `subc.jsonc`.
- `protocol: "subc"`. `protocol: "none"` would make `route.open` terminal with
  `module_no_protocol` and suppress health, leaving ruling (3) unimplementable.
- `reserved: true` from the first placement. The flag makes the daemon check the
  launch nonce on HELLO, so no unsupervised process can register `ckbus` and receive
  every child's sign and credential requests. It protects the provider's identity and
  has nothing to do with the callers' attestation (Credentials). `ck-bus` reads
  `SUBC_LAUNCH_NONCE` and echoes it verbatim in `ModuleHelloBody::launch_nonce`, never
  synthesising, defaulting or caching it. Registration is asserted by the declaration
  row's four observables, never by a `supervisor.list` entry. The daemon refuses
  `reserved: true` with `protocol: "none"` at parse.
- Catalog advertisement is read from the `CatalogEntry` field slice 0 named. If it
  disappears at fire time, the arm fails loudly and invents no accessor.
- Supervised pid source (R6), stated once. Every arm that needs a supervised pid reads
  `supervisor.provenance` narrowed to the module id under test, at
  `daemon_observed.pid` on the entry whose `module_id` matches, and cites that op. It is
  not read from `supervisor.list` (no pid field), the daemon-internal snapshot, or
  `ck module status`. An unnarrowed reply is never indexed positionally. There are
  three outcomes: (a) an error frame fails the arm, quoting the code and the id; (b) an
  entry whose `pid` is `None` or absent is re-read once per 200 ms for up to 2 s, then
  fails quoting the last reply, with no fallback and no `ps` on an empty pid; (c)
  `Some(pid)` proceeds. Settling after a spawn is the same bounded re-read.
- Terminal records, read only by A1, come from the fixture terminal journal,
  cross-read against `supervisor.terminals` in the same run, against the fields slice 0
  recorded. A missing field path fails loudly.
- `drain_timeout_ms` is declared explicitly; every arm reads the effective value from
  `supervisor.list`.

Health answer. `ckbus` advertises `health.check` and answers it with the sentinel's
verdict for the running process: state plus class. The module-side leg is
`ModuleControlResponse::HealthCheck { status, detail, metrics }`. The client leg is
`ClientControlResponse::SupervisorHealthProbe { module_id, status, detail, metrics }`.
Both carry `metrics: Option<serde_json::Value>`.
- Every arm reads the class through `supervisor.health_probe` only. The cached
  `supervisor.health` snapshot passes metrics through `truncate_health_metrics`
  (`supervise.rs::handle_health_report`), and the probe path relays them whole (slice 0
  record). If the probe path starts truncating, that is a
  `health-class-carrier-unpinned` observation naming the leg.
- The module writes the byte-exact `Unavailable` or `Denied` at `metrics.class`, with
  `detail` carrying `bus.health.up` or `bus.health.down` and no class when up. Status is
  `Failing` when down (slice 0 record). No second encoding is invented.
- The persisted verdict never answers for a process that did not write it. On start
  the verdict is down/`Unavailable` until this process's first probe answers.
- The probe transports the verdict and never originates one. An answered unhealthy
  probe does not escalate: `supervise.rs::handle_health_report` dispatches it through
  the declared action, the `ckbus` declaration fixes both unhealthy actions to
  `report`, and `apply_l3_health_action`'s `Report` branch does not restart (slice 0
  record). Ruling (3) holds by that citation.
- The module never exits on a Claustrum, signing, broker or federation failure.
  Exiting burns the crash budget and lands `Failed`. It stays registered, retries on
  the sentinel period, and answers health down with the class and a `detail` naming the
  cause.
- Health down with class `Unavailable` also covers four cases, each named in `detail`:
  the machine id absent (`machine-id-absent`), the system-account user unissuable
  (`sysaccount-absent`, per the foundation), a damaged `account.json`, and a root key
  the vault answers `not_found` for (`root-key-unreachable`).

Outbound calls, principal and serving side. The module calls Claustrum
(`credential.sign`, `credential.public_key`) and Callosum (`callosum.hub_read`, and
from the federation slices `callosum.peer_keys_read`). Each call is a subc route opened
with `route.open` through `subc-transport` and `subc-protocol` directly. Target ids are
the real `claustrum` and `callosum` in acceptance and in production. No outbound route
reads a vault record's secret half: `credential.get` and `credential.get_scoped` are
never called, and a signing key refuses them by design.
- Principal. Every route the module opens carries `ConsumerIdentity { module_id,
  launch_nonce }` in `ClientControlRequest::RouteOpen.consumer_identity`, from
  `SUBC_MODULE_ID` and `SUBC_LAUNCH_NONCE`. Only then does
  `control.rs::route_open_principal` stamp `Principal::Reserved { module_id }`, after
  `SupervisorHandle::spawned_consumer_authorized`. An absent identity stamps
  `Principal::Direct`. An altered one is refused `bad_consumer_identity`. The callee
  observes the stamp at `subc_client_rs::RouteBindRequest.principal` (slice 0 record).
  Claustrum answers a `Direct` caller against a `reserved:ckbus` grant with
  `not_found`, byte-identical to "no such key". So a missing identity reads as a missing
  key, and the module's report names both causes when it sees `not_found`. The HELLO
  nonce echo never establishes the principal on an outbound route.
- Serving side (R10, amended by R12). No acceptance run uses a real callosum. Three
  serving sides exist, and every row states which one served it, in its report as
  `served-by: <side>`:
  - `harness-stub`: the slice-0 stubs under `tests/harness/stubs.rs`, registered as
    `claustrum` and `callosum` so `route.open` resolves as in production. A stub is a
    fixture of shape. It serves the recorded reply shapes, records the principal it
    observed, never verifies a signature, and never produces one.
    `stub-reply-shape-unrecorded` is recordable by any row, naming the op it stopped
    on (R11). The shape table is data with one registration seam (op name to recorded
    reply body), so filling it adds rows rather than control flow.
  - `harness-signer`: a harness module under `tests/harness/signer/**`, registered as
    `claustrum` in the runs that use it. It holds fixture Ed25519 root keys generated
    per run, and answers `credential.sign` and `credential.public_key` with real
    signatures and real public halves, in exactly claustrum's wire shape (Credentials).
    It exists only under `tests/`, and the production binary has no path to it; the
    signer-shape row checks both properties. It proves ck-bus's own behaviour: the JWT
    ck-bus builds is accepted by a real `nats-server` whose operator and account JWTs the
    harness wrote from the same fixture keys. It proves no vault authority. It answers
    every principal the same way, and a row that asserts authorization against it fails
    the harness.
  - `claustrum-binary`: a real claustrum found through `CK_CLAUSTRUM_BIN`, declared by
    the harness renderer into a fixture vault under the run's `XDG_DATA_HOME`. The
    ceremony is run there with the placed `ck auth` commands (Credentials), with `ck`
    found through `CK_CK_BIN`. The vault-authorization rows run only here. If either
    binary is absent, the row records `claustrum-binary-absent` in its report and on
    stderr and reports SKIP. It never passes and never falls back to another side.
- The harness fails a run in which a row omits its `served-by`, in which a row
  asserting authorization was served by anything but `claustrum-binary`, or in which
  an arm authenticates to the broker with material the harness signer did not sign
  under a key the harness wrote into the server config.
- Refusals are classified, not collapsed. `target_unavailable` and `module_warming` are
  retryable and retried on the sentinel period. `module_no_protocol` is terminal and
  reports health down/`Unavailable` naming the target, without exiting.

Credentials (design D; foundation amendment `48c83a68e`, `0eb12229f`, `b9e827c69`).
- Root keys are the only vault records. The operator creates them once by ceremony,
  in the syntax the placed `ck auth` accepts: `ck auth mint-signing-key --id
  signing:<provider>[:<generation>]`. It generates the Ed25519 pair inside the vault
  and prints only the public key and key id. Each key then gets exact grants to the
  principal `reserved:ckbus`, one `ck auth grant --principal reserved:ckbus
  --selector-kind exact --selector <credential id> --operation <op>` per operation. The
  selector is exact, never a category. CKCRED's statement, read from claustrum
  source: ck-bus needs TWO grants per key, `sign` for `credential.sign` and `read` for
  `credential.public_key`, because the sign grant alone does not authorize the public
  key. Claustrum's own test
  `credentials-module/src/main.rs::sign_grant_does_not_authorize_scoped_public_key`
  pins that at `57a501b`.
- Rotation (CKCRED, 2026-09-24, superseding its earlier "a new generation id, not a
  replace"). A root rotates only by `ck auth mint-signing-key --replace` on the same
  id. `record_version` is NOT monotonic across a delete-and-remint or a vault
  restore, so a consumer detects a rotated key by comparing `key_id` first, and uses
  the version only when the `key_id` values are equal. ck-bus keeps the `key_id` of
  every root it signed with in memory. When a `credential.sign` or
  `credential.public_key` reply carries a different `key_id`, it treats the root as
  rotated. It re-reads the public key, re-issues the JWTs that root signed, and
  reports the rotation. It never trusts a version number alone.
- Which roots, and how far each is agreed:

  | Root | Credential id | Signs | Agreed |
  | --- | --- | --- | --- |
  | box account signing key | `signing:ck-bus-account:1` | participant and ck-bus box-account user JWTs | EXISTS: minted with the operator's approval (CKCRED, 2026-09-24), `record_version` 1, public key hex `c73fe2b0df0d9921f4531bf1277404839a6d630be49ed77dbd29848f3e1bfcfa`, `key_id` `0253fac9609168a5`, exact `sign` and `read` grants to `reserved:ckbus` |
  | system account signing key | `signing:ck-bus-sysaccount:1` | the system-account user JWT | proposed to CKCRED with exact `sign` and `read` to `reserved:ckbus` (install trust chain design, 6.3); acceptance uses a fixture key |
  | operator root | `signing:ck-bus-operator-root:1` | the operator JWT (self-signed) and the system account JWT, once, by ceremony at install | EXISTS with zero grants; ck-bus has no use for it and never calls the vault for it (`RootCredential::OperatorRoot` refuses a credential id) |
  | operator signer | `signing:ck-bus-operator-signer:1` | the box account JWT, at first boot and in every revocation claims update | EXISTS with `sign` to `reserved:ckbus` (R14); `read` requested from CKCRED (design 6.2), which ck-bus needs to name the signer as the JWT's issuer |
  | federation account signing key | by analogy, `signing:ck-bus-fedaccount:1` | ck-bus's federation-account user JWT | required by nats-federation r3; unagreed |
  | message-signing key | `signing:msgsig:<host>:1` (CKCRED's earlier form; its 2026-09-24 note says `signing:msgsig`) | per-message sender signatures | works today by the same mechanism; awaiting the operator: `sign` to `reserved:ckbus` and `read` to `reserved:callosum`, which publishes the public half in its key record; `<host>` is unspecified, and this spec reads it as the machine id |

  The ids are credential ids, not the foundation's `nats.*.{acct}` record-name grammar,
  which described vault records ck-bus would have minted (Open questions). ck-bus
  resolves them through `cortexkit-bus-naming::root_credential_id` (commons
  `e14a671bd`, the pin ck-bus builds against). Slice 4 replaced r2's single operator row with the two above, per
  `docs/designs/nats-install-trust-chain.md` (section 7, 6.1), which governs keys,
  which JWTs exist, who signs them and first boot wherever it differs from this text.
- `root-ceremony-unrun` (deployment gate; owner the operator, with CKCRED; no build
  work). It is tracked per root. DISCHARGED for the box account key on the operator's
  machine, with the facts in the table above. Standing for the system-account and
  operator keys, whose ids and grants are unagreed, for the federation account key, and
  for the `msgsig` grants. It blocks no acceptance row: the harness signer holds its own
  throwaway fixture roots, generated per run, and never the real
  `signing:ck-bus-account:1` or any other production key. The `claustrum-binary` rows
  run the ceremony in their own fixture vault. A harness that finds a production key
  id or `key_id` in a fixture fails the run. When a root answers `not_found` in production, ck-bus reports
  `root-key-unreachable` and names the credential id.
- `credential.sign` wire shape, per CKCRED and read at claustrum `57a501b`
  (`crates/credentials-core/src/signing.rs::sign_ed25519`, `::key_id_for_public`;
  `crates/credentials-module/src/read_surface.rs`). The request carries
  `credential_id` and `payload_b64`, standard base64. The signature is pure Ed25519
  over the DECODED bytes, with no pre-hash. Payload is at most 1 MiB. The reply is
  `{signature_b64, key_id}`: `signature_b64` is standard padded base64 of 64 bytes, and
  `key_id` is lowercase hex of `sha256(public key)[..8]`, not the nkey.
  `credential.public_key` returns the 32 raw public bytes as lowercase hex in
  `public_key_hex`. ck-bus builds every NATS encoding itself: the nkey string (role
  prefix, base32, crc16) from the hex, and the JWT signature by re-encoding
  `signature_b64` as base64url without padding over the `<header>.<payload>` ASCII it
  sent. The signer-shape row pins this against a golden request/reply pair.
- Per-process keys never leave ck-bus's memory: participants' users and ck-bus's own
  box-account, system-account and federation-account users. Each is an Ed25519 nkey
  generated in ck-bus's memory on issue and never written to env, argv, disk, the
  store, a log, a report, or the vault. ck-bus builds the user JWT with the grant the
  foundation enumerates for that role (the naming crate's generator) and has the right
  root sign it through `credential.sign`. The child never holds its seed. When the
  server asks for a connect nonce signature, the child asks ck-bus over its subc route
  and ck-bus signs in memory. This is CKCRED's bearer-versus-oracle reason: a copied
  child-held seed works anywhere until expiry, while a bus-held seed works only for a
  caller that reaches ck-bus as that module.
- Delivery and attestation. ck-bus serves two ops to supervised children over the subc
  wire. Their working names are `ckbus.credential` and `ckbus.nonce_sign`. The final
  spellings are this spec's to fix with the client owners, and they are unagreed with the client owners
  (subc-client-rs: SUBC; prefrontal-core: ALF), recorded as the deployment gate
  `ckbus-client-ops-unagreed`, which blocks no row because the harness drives both ops
  directly.
  - `ckbus.credential` returns the caller's user JWT, its `{acct}`, its inbox prefix
    `_INBOX.{credential_public}`, and the server URL.
  - `ckbus.nonce_sign` signs the given nonce with the caller's current seed.
  - Both authorize by the stamped principal alone. `Principal::Reserved { module_id }`
    binds the answer to that `module_id` and to the live generation the spawn stream
    shows for it. A body field claiming another id is ignored. A `Direct` caller gets
    the named refusal `ckbus_principal_direct` and nothing else. A caller whose module id
    has no live generation gets `ckbus_generation_not_live`.
  - Every subc-wire spawn receives a nonce whether or not it is declared `reserved`, so
    a participant qualifies by presenting its consumer identity. No participant needs
    `reserved: true`.
  - Participants at launch are ck-bus and prefrontal-core only; every other module joins
    when its port lands.
- Revocation is NATS state, never a vault delete. Three ordered steps, the
  foundation's four minus the dropped vault delete.
  1. Read `user_jwt_id` and the user public key from the census value. Add the user
     public key to the box account JWT's `revocations`, have the operator key sign the
     updated account JWT through `credential.sign`, and push it over the system-account
     user's claims-update subject. The subject string comes from A9's golden. The
     directory resolver persists it.
  2. Delete the census key.
  3. Kick the client over `$SYS`.

  The account JWT being updated is read from the server by the system-account user's
  claims lookup (subject from A9's golden) immediately before each update, never from a
  local cache. ck-bus is the single writer. A lookup that fails defers the revocation.
  The progress record is written and fsynced with `highest_completed_step: 0` and its
  inputs before step (1), replaced with N after step N commits, and cleared after step
  (3). On restart the module resumes at `highest_completed_step + 1`. The progress
  record, not the census, is the recovery source. Exactly-once counts effects: exactly
  one revocation entry for the user key in the account claims read back, and exactly one
  `$SYS` disconnect event for the target in the run's capture. Replays are required to
  be no-ops: re-adding a present revocation leaves the claims equal, and a repeated kick
  of a gone client emits no new event. The arms assert this, and nothing assumes it.
- JWT expiry is the foundation's second revocation half. Its lifetime is unset
  (`user-jwt-ttl-unpinned`, owner ALF, unagreed). Until it is pinned, ck-bus issues
  user JWTs with no `exp`, revocation alone enforces, and the report names the residual:
  a user whose revocation was lost to damage stays valid. When pinned, ck-bus re-issues
  before expiry at the next epoch and revokes the superseded user.
- ck-bus restart and in-memory keys. Measured by ALF on nats-server 2.15.0 with the full
  resolver, per the foundation amendment's measured basis: while ck-bus is down, open
  connections keep working and new connects and reconnects fail, because nobody can sign
  a nonce. A ck-bus restart loses every seed, and the restarted process does not revoke
  the users it cannot sign for, because revoking disconnects them. A child that asks it
  to sign for a key it does not hold gets `ckbus_credential_superseded`. The child then
  calls `ckbus.credential`, and ck-bus issues at the next epoch: high-water first, JWT
  signed, census overwritten, then the superseded user revoked by the three steps.
  "Reconnect succeeds once the signer returns" therefore means "after one credential
  refetch" (Open questions). On start, ck-bus reads `own_users.json`, revokes the
  previous incarnation's own users once its new ones connect, then writes its own new
  keys. nats-server is a daemon-supervised sibling, never ck-bus's child, so a ck-bus
  crash cannot restart it.
- Machine id and `{acct}`. ck-bus reads `ModuleHelloAckBody.machine_id`, validates it
  with `subc_protocol::MachineId::parse`, and derives `{acct}` = `box_<machine id>`
  through the naming crate. That is 36 bytes, inside the foundation's account lexicon.
  The id is a name, never an authority: nothing is admitted or trusted because it
  matches.
  - An absent field means the daemon predates the machine id, never "no machine" and
    never a reason to mint. ck-bus creates nothing, issues nothing, and answers
    `machine-id-absent`. This is a deployment gate: the acceptance daemon at
    `cac1e9bd`+ serves the id, so no row carries it.
  - An id that differs from `account.json`'s (a `ck machine adopt` between runs), or a
    box account for another machine id found on the server when `account.json` is
    absent, is REFUSED (ALF, amending the r2 rule and the trust-chain design's section
    5): ck-bus creates no second box account while one exists under the old id. It
    answers health down/`Unavailable` with cause `machine-id-changed`, naming both
    ids, creates and deletes nothing, and leaves `account.json` as it is. The operator
    decides the migration.
  - This spec does not make root keys per `{acct}`, since the credential ids carry no
    token. Whether a new `{acct}` needs new account keys is an Open question.
- Server configuration. The local server needs its operator JWT, system account, the
  directory resolver and TLS material before it starts. It is a sibling started
  concurrently, so ck-bus cannot write that config in time. The writer is
  unnamed in production (`server-config-writer-unnamed`, deployment gate, owner SUBC
  with the operator; the foundation named "SUBC's installer calling CKCRED", which the
  amendment's ceremony does not cover). In acceptance the harness writes it from the
  fixture roots. The resolver runs with deletion disabled, per the foundation.
- `server-config-writer-unnamed` and `root-ceremony-unrun` block placement, not rows.

Ownership of acts.
- Issuance: ck-bus issues every per-process credential. Issuing and writing the census
  key are one act, in this order:
  1. Fence against the spawn snapshot's generation.
  2. Advance and fsync `epoch_high_water`.
  3. Generate the nkey in memory and sign the JWT.
  4. Create the participant's `c_{agent_id}` durables with the foundation's
     configuration, if absent.
  5. Write the census key.
  6. Answer `ckbus.credential`.

  A crash anywhere before step 5 leaves an unusable signed JWT (its seed died with the
  process) and no census entry: nothing to roll back. The reconciliation re-issues. A
  crash after step 5 leaves a census entry whose key nobody holds, which is the restart
  rule above.
- Census writes use ck-bus's own box-account user. ck-bus's own census key is
  self-written, the single named exception, and its authority rests on the box account
  grant. A participant never holds write on its own entry.
- Bootstrap: on start, after the machine id and roots resolve, ck-bus signs its
  system-account user with the system account key and connects to `$SYS`, finds or
  (only if none exists) creates the box account, re-signs its JWT for the same id with
  the operator signer, carrying earlier revocations and adding the previous
  incarnation's box users, pushes it, and reads it back through the claims lookup
  before treating it as applied (a claims update is saved without a trust or `iat`
  check). It then issues its box-account user, connects, and creates the census bucket
  and all five streams with their literal bindings if absent. It does NOT revoke the
  previous incarnation's system-account users (trust-chain design 6.7): their seeds
  died with that process, so they cannot answer a nonce, and revoking them would
  re-sign the root-signed system account. Only ck-bus's system user holds the
  claims-update permission. ck-bus reads its broker inputs from its supervised
  environment: `CKBUS_NATS_URL` (a loopback `nats://` URL; the listener has no TLS),
  `CKBUS_OPERATOR_JWT` (the operator JWT's path) and `CKBUS_SYSTEM_ACCOUNT` (which must
  equal the operator JWT's `system_account`); the operator signer must be listed in the
  operator JWT's `signing_keys`, or ck-bus refuses with `operator-jwt-mismatch`. The
  local listener binds a loopback address explicitly, never `0.0.0.0`, and needs
  `max_control_line` above the 4 KiB default: a CONNECT carrying ck-bus's box grant is
  longer (a setup obligation, measured in slice 4). It never publishes a workload message and holds no workload
  publish grant in the box account. If the system-account user cannot be issued, the
  module serves nothing and answers `sysaccount-absent`, per the foundation: revocation
  is never left silently unenforced.
- Sentinel probe: hosted in the module process, requester and responder in one process
  through the server, period 10 s, timeout 2 s. Three failures report `bus.health.down`.
  A permissions violation on its own sentinel or inbox subjects within the period wins
  over the requester's timeout. These are the shipped defaults, with no operator-facing
  key. The test seam is `CKBUS_SENTINEL_PERIOD_MS` and `CKBUS_SENTINEL_TIMEOUT_MS`,
  set by the harness renderer in the `ckbus` block's `env` list (slice 0). At start the
  module emits exactly one structured log line carrying `sentinel_period_ms`,
  `sentinel_timeout_ms` and its incarnation id. Every arm computing `3 * period +
  timeout` parses that line and fails when it is absent. The harness sets 1000 ms and
  200 ms for the health and A8 rows, so a down bound is 3.2 s and those two rows are
  budgeted at 120 s together. Exceeding the budget fails the row with the measured wall
  time. Silence before the first answer is the initial down/`Unavailable` verdict. The
  verdict is published on the bus and is also the `health.check` answer; it never gates
  spawning.
- Dead-letter consumer: `c_ckbus_dead` on `CK_{ACCT}_EFFECT_DEAD`, deduplicating on
  message id, because the claimant publishes the record before `term()`.
- Membership: re-issue at the next epoch of the same generation, overwrite the census
  key, adjust the room consumer's `filter_subjects`, and revoke the superseded epoch,
  within the foundation's 10 s bound. The client reconnects by refetching.

Spawn event stream (SUBC, wi_081820c5; landed at 6844bbb5b7f9). The module never
synthesises a generation from restart counters.
- `supervisor.spawn_snapshot` returns `{cursor: {daemon_incarnation, seq}, ring_bound,
  live[]}` under one lock. `supervisor.spawn_subscribe {since: Option<SpawnCursor>}`
  delivers one `StreamData` per event: `{cursor, kind: spawned | exited, module_id,
  spawn_generation, pid, exit_code?, exit_signal?}`. The consumer builds against the
  `subc-control` types `SpawnCursor`, `LiveSpawn`, `SpawnSnapshot`, `SpawnEventKind` and
  `SpawnEvent`.
- Refusals, asserted byte-exact: `spawn_cursor_incarnation_mismatch` with
  `current_daemon_incarnation`, and `spawn_cursor_too_old` with
  `oldest_retained_cursor`. A stream the daemon drops for a stalled subscriber ends with
  a terminal `spawn_subscriber_lagged` carrying `first_undelivered_cursor`, after every
  queued event. The consumer resubscribes from the last cursor it received.
- `exited` is fact-only, and revocation runs on the fact alone. `ring_bound` is read
  from the reply, never hard-coded. Every terminal path emits `exited`, and
  `spawn_generation` increments once per successful spawn.
- The last processed cursor is recorded. On restart the consumer resumes, or across an
  incarnation or a damaged cursor it snapshots and reconciles. Live generations with no
  census entry are issued lazily, on the child's first `ckbus.credential`, because
  ck-bus has no channel to push a credential. Entries with no live generation are
  revoked. The same reconciliation runs every 60 s. Generation fencing compares
  generations only.

Standing rules carried from the foundation.
- Absence is neutral. An unsuccessful read is never a confirmed absence, and recovery
  obeys the same rule.
- Spawn generation is SUBC's. Credential epoch is ck-bus's and starts at 0 per
  generation.
- Permission files are allow-only, wildcards are whole-token, and every stream-bearing
  subject is emitted with the five literal stream names expanded per account.
- Inbox prefix is `_INBOX.{credential_public}` on every client the module runs or
  issues for, set before connecting.
- No constant from an unverified citation is hard-coded in a control. The drain
  ceiling, sentinel values, claims-update, claims-lookup and kick subjects, and the
  JetStream request set are read at run time.

Federation role (nats-federation r3; CALLO report of callosum `f5d34f8`; CKCRED
report). Everything here lands after the single-machine slices. With no hub row in
the roster, the federation code is inert: no federation account user, no leaf
configuration, no sealing. The single-machine plane is unchanged. A hub read that
fails or answers "no hub" is health-neutral, because the local bus is unaffected.
Federation state is reported separately under `detail` keys prefixed `fed.`.
- Federation account. Each machine has a local federation account, distinct from the
  box account and bound to the leaf remote. It holds only sealed cross-machine
  families: an outbox and an inbox, whose grammar is owed by
  `fed-foundation-amendment-unlanded`. Effect subjects never exist in it. ck-bus holds
  a federation-account user. The account's JWT is signed by the box-local operator
  key; the user's JWT by the federation account signing key.
- The authority boundary, from project rule and nats-federation r3 (ALF agreed). ck-bus's
  federation authority is limited to sealing outbound frames and opening inbound frames
  in the federation account. It holds no publish permission in the box account, so
  effect intents never cross machines, and it never republishes an opened body under
  its box-account user. How an opened delivery reaches the recipient's local consumer
  without a box-account publish by ck-bus is unsettled (Open questions: local delivery
  path). The candidate is a server-side account import from a federation-account
  local-delivery family into the box account's ROOM, WAKE and PEER subjects only, with
  the leaf remote denying that family. It rests on `nats-federation-rig` and on
  `fed-foundation-amendment-unlanded`, and no slice asserts it before both clear.
- Outbound capture must be durable, because loss is not acceptable across a split. The
  foundation's disjoint bindings make capturing `ck.{other}.…` in the box account
  impossible without overlapping the local streams. So the outbound family and the
  participant's cross-machine publish grant are part of
  `fed-foundation-amendment-unlanded`. ck-bus consumes from a durable, acks the local
  original only after the sealed frame's publish ack, and never uses a core-NATS
  subscription as the outbound path.
- Leaf configuration. ck-bus reads `callosum.hub_read` every 60 s and on every
  reconnect. Exactly one hub is accepted; two are refused. The SPKI SHA-256 pin from the
  roster column is the authority and candidate addresses are advisory: a dial whose
  presented SPKI differs is refused before any credential is sent. A tombstoned or
  changed hub identity disconnects the leaf within 75 s of the roster write. ck-bus
  produces a leaf configuration document into its run directory: the ordered candidates,
  the pin, the federation account's public key and the leaf credential reference. The
  mechanism by which the leaf-bearing server consumes it, and whether config reload
  can add, change and drop a leaf remote without a restart, rests on
  `nats-federation-rig` and on the leaf-signing shape.
- Hub and accounts. A self-hosted hub (the default) or the hosted hub (opt-in) holds
  one account per user. The leaf binds the federation account into it. Routing from a
  per-machine federation account into a per-user hub account is `nats-federation-rig`.
  `{acct}` in a subject names the recipient machine. Local subjects staying off the
  leaf under a hub-side `ck.{own}.>` subscription is `nats-federation-rig`. The census
  bucket and `$SYS` never cross.
- Leaf signing. Stock `nats-server` gives a leaf remote only a creds file or an nkey,
  which would put a seed on disk. The leaf credential is a key of the hub's account,
  created at pairing (`leaf-credential-ceremony-unlanded`), and ck-bus never creates
  it. The shapes are (1) a Rust bridge, (2) an ephemeral memory-backed creds fd, and
  (3) a Go binary embedding `nats-server` with `SignatureCB` calling `credential.sign`
  over subc. r3 recommends (3), pending `SignatureCB` in the pinned server
  (`nats-federation-rig`). Under (3) the Go binary needs its own consumer identity and
  exact grant, or it routes its callback through ck-bus's `ckbus.nonce_sign`; SUBC and
  CKCRED choose (`leaf-signing-shape-unchosen`). The link arm runs labelled with the
  shape it built. The connect error must name the signer, so "vault unavailable" and
  "hub refused" read differently.
- Keys. Each machine holds two operational keys in its vault: a seal key (X25519, for
  HPKE) and a message-signing key (Ed25519, `signing:msgsig:<machine id>:1`, used
  through `credential.sign`, which works today).
  - The seal key is a new vault kind, a KEM key, minted with
    `ck auth mint-kem-key --id kem:<name>`. `<name>` is unagreed; by analogy this spec
    uses `kem:ck-bus-seal:1`. It is refused by `get` and `sign`. Its public half comes
    from `credential.public_key` with algorithm `"x25519"`. Its private half is usable
    only through `credential.open`, by `credential_id` under a new `open` grant given to
    `reserved:ckbus` alone. A handle is refused.
  - This is CKCRED's contract, fired as claustrum campaign
    `ct_00000000-0000-4006-98d8-bc6a13f46a50` and not landed. It is gated by
    `kemkey-open-unlanded`, and deploying it needs claustrum store migration 11.
  - Which principal gets `read` on the KEM key, so callosum can publish the public half,
    is unagreed.
  - This machine's record generation, and a single op returning both public halves with
    it, remain unnamed (`own-key-record-op-unnamed`). ck-bus needs neither to seal or
    open. The generation lives in the vault's credential ids, never in the trust
    document.
  - Peers' key records reach ck-bus only through callosum's local-only management op
    `callosum.peer_keys_read`, which callosum's forwarder refuses to any remote peer.
    Callosum stores records only from verified peers in the paired Noise session, gated
    by the pairing ceremony counter, and clears them on retire, re-pair and tombstone
    (CALLO report, callosum `f5d34f8`, not read by this spec; the slice that stubs it
    records its reply shape). ck-bus never reads a key record from the hub, a registry,
    or a message.
- Sealing. For a delivery addressed to another machine, the body travels inline and
  sealed. The digest moves inside the seal and no header carries anything sensitive.
  - Sign inside, then seal. ck-bus has `msgsig` sign a purpose-tagged context covering
    the envelope version, sender machine id, recipient machine id, the full destination
    (family, `agent_id`, and `session_id` or `room_id`), message id, the sender's
    sequence number and the body.
  - It then seals that signed plaintext by HPKE base mode, the only suite the vault
    opens: KEM 0x0020 DHKEM(X25519, HKDF-SHA256), KDF 0x0001 HKDF-SHA256, AEAD 0x0003
    ChaCha20-Poly1305. `info` is `sender_machine_id|recipient_machine_id`, and `aad` is
    empty in envelope version 1. The vault interprets neither `info` nor `aad`, so the
    machine ids and the destination are bound by the inner signature, and the opener
    checks them there. Nothing relies on `info` for binding.
  - Sealing needs only the recipient's public key, so it does not need
    `kemkey-open-unlanded`. ck-bus refuses to seal a body whose opening context would
    exceed the vault's 1 MiB limit. The item is reported, never truncated.
  - A delivery for N machines is sealed N times. A hosted hub is never a recipient.
  - A peer with no current key record is not sealed to. The outbound item waits, never
    falls back to plaintext, and is reported. A peer absent from `callosum.peer_keys_read`
    (removed) is fenced: ck-bus stops sealing to it and stops accepting from it. Removal
    is not instantaneous; the hub-side revocation of its leaf credential is the
    ceremony's, not ck-bus's.
- Sequence. r3 says "per-sender sequence". A single counter per sender would show every
  recipient gaps for frames sent elsewhere, so ck-bus numbers per (sender, recipient)
  pair (Open questions). Before publishing, ck-bus fsyncs a reservation of (sequence,
  delivery id) in `fed_send/{recipient}.json`. It publishes with `Nats-Msg-Id` =
  delivery id, and advances only after the publish ack. A redelivered local original
  whose delivery id matches the reservation reuses its sequence, so a crash neither
  skips nor reuses a number.
- Opening. For each inbound frame on its own inbox, ck-bus makes one call:
  `credential.open {credential_id, enc_b64, ciphertext_b64, info_b64, aad_b64}`,
  answered `{plaintext_b64, key_id}`, by `credential_id` and never by handle. No
  `enrollment_token` is sent; it is optional in the contract. Failures are classified:
  - `open_failed` is the vault's one uniform permanent failure, covering bad `enc`, the
    wrong key, tampering, and a wrong `aad` or `info`. It quarantines the frame.
  - `context_overflow` (over 1 MiB) quarantines the frame.
  - `kind_not_openable` means the credential id names a non-KEM key. That is ck-bus's
    misconfiguration, not the frame's fault: frames wait, and health `detail` carries
    `fed.open_misconfigured` naming the id.
  - `not_found`, and transport refusals, leave frames waiting.

  A `key_id` different from the one last seen means the seal key rotated, and is
  reported. ck-bus then verifies the inner signature against the sender's current
  `msgsig` record and checks the signed machine ids against the sender and itself. It rejects any
  mismatch between the opened destination and the arrival subject, and dedupes on
  (sender, sequence) against `fed_recv/{sender}.json`, which it fsyncs before acking.
  It reports gaps as `fed.gap`. Unsigned, unverifiable, mismatched, retired-generation
  or unopenable frames are quarantined under their own terminal disposition. They are
  never acked as delivered, and never `Absent` or a digest mismatch. While Claustrum is
  down, inbound frames wait unopened: late, not lost. Recipient-side idempotent insert on
  delivery id stays the owning store's job.
- Split tolerance. Federation streams need days of max-age, discard new, a stated
  re-sync bound, and recipient dedupe. The values are owed by
  `fed-foundation-amendment-unlanded`, and ck-bus creates no federation stream before
  they are pinned. Two behaviours rest on `nats-federation-rig`: sourcing a stream
  across a split with catch-up, and a subject-filtered purge of one recipient's inbox.

Sequencing and file fences. Slices are strictly sequential, and each depends only on
earlier ones. Their fences are disjoint by subdirectory. One path is the declared
integration ref: `crates/ck-bus/src/main.rs`, authored by slice 0, in which a later slice
adds only its `mod <area>;` line and one wiring call, both named in its plan entry. The
harness has the same rule at `crates/ck-bus/tests/harness/mod.rs`: a later slice adds
only its `mod <submodule>;` line there, and a shape-table row through the stubs'
registration seam. No slice writes inside another slice's directory. A slice that needs
an edit outside its fence stops and reports.
- Forward dependencies are carried by seams. `crates/ck-bus/src/runtime/**` (slice 0)
  defines the internal interfaces each later area implements, each defaulting to a
  refusal naming the area. No row may pass against a refusing default. An arm needing a
  second area lands in the later of the two slices, and that slice owns its row. A slice
  adding an area the r1 seams did not name (credentials, issuance, federation) adds its
  seam inside its own area and wires it through its one `main.rs` call.
- Slice 0 (crate, merged `0b6f32da`) owns the root `Cargo.toml`,
  `crates/ck-bus/Cargo.toml`, `src/runtime/**`, `tests/harness/**` as it stands,
  `tests/module_declaration.rs` and `tests/fixtures/**`. It landed every r1 slice-0
  obligation (FIRE-TIME-RECORD.md): the runtime skeleton that completes HELLO, echoes
  the nonce, serves the control lane and advertises `health.check`; the stubs; the
  renderer with the sentinel env seam; the store root; the field records; the naming
  report; the vendored foundation copy; the disposition mapping. Slice 1 (supervised
  server, merged `2b7ccc94`, follow-up `4bc1d17e`) owns `tests/support/**` and the two A1
  files.
- Every row has exactly one owning slice and one test file, named here and nowhere
  else. Paths are relative to `crates/ck-bus/`. Each slice after 1 owns the `src/**` area
  and harness submodule its entry names, plus its test files.

| Ladder row | Owning slice (fence) | Test file |
| --- | --- | --- |
| Module declaration and registration | 0, crate (merged) | `tests/module_declaration.rs` |
| A1 supervised server lifecycle | 1, supervised server (merged) | `tests/supervised_server.rs` |
| A1 argv non-injection | 1, supervised server (merged) | `tests/supervised_server_argv.rs` |
| Grant generation | 2, grants (`src/grants/**`, `tests/fixtures/foundation/**`) | `tests/grant_generation.rs` |
| Signer wire shape | 3, credentials (`src/credentials/**`, `tests/harness/signer/**`) | `tests/signer_shape.rs` |
| Vault authorization | 3, credentials | `tests/vault_authorization.rs` |
| User JWT and seed absence | 3, credentials | `tests/user_jwt.rs` |
| Machine id and account | 4, bootstrap (`src/bootstrap/**`) | `tests/account_identity.rs` |
| Install bootstrap and own users | 4, bootstrap | `tests/install_bootstrap.rs` |
| Credential delivery and attestation | 5, issuance (`src/issuance/**`) | `tests/credential_delivery.rs` |
| A3 census write and issuance recovery | 5, issuance | `tests/census.rs` |
| A9 grant conformance | 5, issuance | `tests/grant_conformance.rs` |
| A3 revocation | 6, revocation (`src/revocation/**`) | `tests/revocation.rs` |
| Spawn-stream consumer | 7, spawn consumer (`src/spawn_consumer/**`) | `tests/spawn_stream.rs` |
| Spawn reconciliation and census recovery | 7, spawn consumer | `tests/spawn_reconcile.rs` |
| Module health answer | 8, sentinel (`src/sentinel/**`) | `tests/module_health.rs` |
| A8 sentinel probe | 8, sentinel | `tests/sentinel.rs` |
| Dead-letter | 9, dead-letter (`src/dead_letter/**`) | `tests/dead_letter.rs` |
| Membership lifecycle | 10, membership (`src/membership/**`) | `tests/membership.rs` |
| Signer outage and restart | 11, outage (no `src` area; test only) | `tests/signer_outage.rs` |
| Federation account and isolation | 12, federation link (`src/fed_link/**`, `tests/harness/fed/**`) | `tests/fed_account.rs` |
| Leaf configuration | 12, federation link | `tests/leaf.rs` |
| Leaf link, labelled | 12, federation link | `tests/leaf_link.rs` |
| Seal outbound | 13, sealing (`src/fed_seal/**`) | `tests/fed_seal.rs` |
| Open inbound | 13, sealing | `tests/fed_open.rs` |
| Federation sequence and crash | 13, sealing | `tests/fed_sequence.rs` |
| Split store-and-forward | 14, split and removal (`src/fed_store/**`) | `tests/fed_split.rs` |
| Peer removal fence | 14, split and removal | `tests/fed_removal.rs` |
| A6/A7 prefrontal re-run | no subconscious slice | none in this repository |

- That order is the dependency order, and a slice that reorders it says why. Grants
  come before any issuance because every JWT carries a generated grant. Credentials come
  before bootstrap because ck-bus's own users are issued through them. Bootstrap comes
  before issuance because participant durables need the streams. Issuance comes before
  revocation because revocation reads what issuance wrote, and revocation before the
  spawn consumer because reconciliation revokes. The sentinel follows the areas whose
  deferrals it reads back. Outage closes the single-machine plane. Every federation slice
  (12 to 14) comes after slice 11 and depends only on earlier slices. No single-machine
  slice waits on a federation gate.
- Slice 2 re-vendors the foundation and the prefrontal golden at `b9e827c69` or later
  into `tests/fixtures/foundation/`, updating `SOURCE` and `DISPOSITION-MAPPING.md`.
  That discharges `foundation-disposition-table-unquoted` again. The r1 mapping's A2
  exclusion is rewritten: seed absence for ck-bus's own process is now claimed by
  "User JWT and seed absence". The r1 references to the audit-read gate are removed.

## Acceptance ladder

Every arm pairs with a control that fails for the intended reason, on an empty vault
and an empty broker store, per the foundation. Module-side arms live in
`crates/ck-bus/tests/`, one direct-child file per row. The harness that enforces these
rules lives at `crates/ck-bus/tests/harness/`. No subconscious slice edits prefrontal's
rig. Every run sets `XDG_DATA_HOME` into its fixture tree and asserts nothing was
written under the operator's real data home. The foundation's arms are not
re-specified. The completion test is stated once, in Intent.

Ladder completeness. This ladder is the set of rows this spec claims. Whether it covers
every F-PROV row is settled by slice 2's disposition mapping; until then the Intent gate
stands. A4 and A6 stay named exclusions from slice 0's mapping, and A5 is now the
federation rows.

Serving side, per row. Every row states its `served-by` in the table, and its report
repeats it: `harness-stub`, `harness-signer`, `claustrum-binary`, `none` (no Claustrum
or Callosum route), or a combination for rows that use two. A `harness-signer` pass
proves what ck-bus built and how the broker judged it. It never proves that the vault
would authorize ck-bus, or refuse another caller. Only a `claustrum-binary` pass
proves that.

Gating rules. Each row names in its Gates cell every skip name its arm can record. The
rig fails a run in which a row:
- records a skip name that is neither in its own cell nor universal;
- reports a pass while reaching an op absent from the serving side's advertised
  vocabulary;
- omits its `served-by`;
- records an unnamed skip.

Gating is per row, and the row is the unit of skip. Where part of an assertion set
gates and part does not, the ladder splits the row. A row gates now when it carries no
standing S or D gate and no C condition fires. A slice that finds a gate discharged
flips its row and says so.

Universal conditions, recordable by any row without listing, naming what was observed:
`naming-constructor-absent` (C), `health-class-carrier-unpinned` (C, rows that read the
class), `stub-reply-shape-unrecorded` (C, `harness-stub` rows), and `nats-server-absent`
or `nats-server-too-old` (C, per the foundation's constants, with the observed version).
`foundation-disposition-table-unquoted` suppresses the completion claim only. No row
records it.

| Row | Consumes | served-by | Gates named | State |
| --- | --- | --- | --- | --- |
| Module declaration and registration | daemon config parse; nonce echo; `supervisor.list`, `catalog.list`, `supervisor.provenance` | none | none row-specific | green (slice 0) |
| A1 supervised server lifecycle | acceptance daemon >= 0.20.5; real `nats-server` under `protocol: "none"`; stand-in child | none | `a1-signal-unix-only` (C) | green on Linux and macOS (slice 1) |
| A1 argv non-injection | the same spawn; provenance pid; `ps -ww -o args= -p <pid>`; effective `connection_file_path` | none | `a1-signal-unix-only` (C) | green on Linux and macOS (slice 1) |
| Grant generation | naming crate generator; vendored foundation and golden | none | none row-specific | gates now |
| Signer wire shape | `credential.sign`, `credential.public_key` shapes cited at claustrum `57a501b`; golden request/reply pair | harness-signer; claustrum-binary for the real-binary half | `claustrum-binary-absent` (C, real-binary half only) | harness half gates now |
| Vault authorization | real claustrum + `ck auth` ceremony in a fixture vault; `route.open` with and without `ConsumerIdentity` | claustrum-binary | `claustrum-binary-absent` (C) | gates when the binaries are present; loud skip otherwise |
| User JWT and seed absence | `credential.sign` for JWT signatures; real `nats-server` with harness-written operator and account JWTs | harness-signer | none row-specific | gates now |
| Machine id and account | `ModuleHelloAckBody.machine_id`; `account.json`; bucket and stream creation | harness-signer | none row-specific | gates now |
| Install bootstrap and own users | own box and system users; census bucket, five streams; `$SYS` kick; `own_users.json` | harness-signer | none row-specific | gates now |
| Credential delivery and attestation | `ckbus.credential`, `ckbus.nonce_sign` over subc; stamped principal; spawn snapshot for the live generation | harness-signer | `spawn-stream-unlanded` (C) | gates now |
| A3 census write and issuance recovery | issuance order; census key grammar; `epoch_high_water.json` | harness-signer | `naming-constructor-absent` for the census key until commons constructs it | skipped until the census key constructor lands |
| A9 grant conformance | participant, bus-module and system users on a live server; prefrontal seat for the golden | harness-signer | `prefrontal-seat-unnamed` (S, the golden-commit half only) | live-server half gates now |
| A3 revocation | the three steps; claims lookup and update; `$SYS` capture; operator-key signing | harness-signer | none row-specific | gates now |
| Spawn-stream consumer | `supervisor.spawn_snapshot`, `supervisor.spawn_subscribe`, cursor, refusal codes | none | `spawn-stream-unlanded` (C) | gates now, subject to that read |
| Spawn reconciliation and census recovery | the same ops; issuance and revocation | harness-signer | `spawn-stream-unlanded` (C) | gates now |
| Module health answer | `health.check` carrier; `supervisor.health_probe`; the deferral controls' health half | harness-signer; harness-stub for the refusing-signer control | none row-specific | gates now |
| A8 sentinel probe | own box user; probe; `supervisor.health_probe` | harness-signer; harness-stub for the refusing-signer control | none row-specific | gates now |
| Dead-letter | own user and a claimant user; `c_ckbus_dead` | harness-signer | none row-specific | gates now |
| Membership lifecycle | the foundation's membership contract from the vendored copy; re-issue; revocation | harness-signer | `membership-contract-unpinned` (D, and S if the foundation names no op) | skipped until quoted |
| Signer outage and restart | supervised ck-bus and nats-server; issuance; revocation | harness-signer | `user-jwt-ttl-unpinned` (S, the expiry arm only) | gates now except the expiry arm |
| Federation account and isolation | federation account JWT and user; local-subject isolation; local delivery path | harness-signer | `fed-foundation-amendment-unlanded` (S); `nats-federation-rig` (S: local subjects off the leaf; account routing) | skipped |
| Leaf configuration | `callosum.hub_read` | harness-stub | none row-specific | gates now against the stub's recorded shape |
| Leaf link, labelled | hub server in the harness; leaf credential; the chosen or labelled shape | harness-signer | `leaf-credential-ceremony-unlanded` (S); `nats-federation-rig` (S: account routing; `SignatureCB` for shape 3) | skipped; runs labelled once the rig reports |
| Seal outbound | `callosum.peer_keys_read`; `msgsig` via `credential.sign`; HPKE seal; outbound durable | harness-stub + harness-signer | `fed-foundation-amendment-unlanded` (S) | skipped |
| Open inbound | `credential.open` per CKCRED's contract; `fed_recv` | harness-signer for opening behaviour; claustrum-binary for the `open` grant and shape half; harness-stub for peer keys | `fed-foundation-amendment-unlanded` (S); `kemkey-open-unlanded` (S) and `claustrum-binary-absent` (C), real-binary half only | skipped |
| Federation sequence and crash | `fed_send`, `fed_recv`; publish ack; redelivery | harness-signer; claustrum-binary for the open half | as Seal outbound, plus Open inbound's gates for the receive half | skipped |
| Split store-and-forward | federation streams; sourcing across a split; recipient purge | harness-signer | `fed-foundation-amendment-unlanded` (S); `nats-federation-rig` (S: sourcing across a split; subject-filtered purge) | skipped |
| Peer removal fence | `callosum.peer_keys_read` losing a peer | harness-stub | `fed-foundation-amendment-unlanded` (S) | skipped |
| A6/A7 prefrontal re-run | built `ck-bus` reachable from prefrontal's rig | n/a | `prefrontal-seat-unnamed` (S) | skipped |

Row contents.

- Module declaration and registration (kept). `supervisor.list` reports `ckbus` with
  `protocol: "subc"` and the declared `drain_timeout_ms`. That proves configuration,
  never registration. Four registration observables are asserted individually (R6):
  1. `supervisor.provenance` narrowed to `ckbus` yields a pid P by the three-outcome
     rule.
  2. `catalog.list` reports exactly one `ckbus` registration, with `health.check` among
     the advertised control ops read from the recorded `CatalogEntry` field.
  3. A hand-started `ck-bus` with `SUBC_LAUNCH_NONCE` absent or altered has its HELLO
     refused and never appears in `catalog.list`.
  4. A re-read of provenance afterwards returns the same P, and `catalog.list` still
     shows only the supervised registration.

  `live: true` is asserted too. Reservation is asserted through that spawn/HELLO pair
  only. The daemon refuses `reserved: true` with `protocol: "none"` at parse. The row
  also runs the inventory check.
- A1 supervised server lifecycle, unix (kept). The declared `nats-server` is launched
  directly under `protocol: "none"`, with no wrapper and no stand-in for the primary
  arm.
  - Precondition: the recorded subc-daemon version is >= 0.20.5.
  - Terminal records come from the fixture journal, cross-read against
    `supervisor.terminals`. "Inside the budget" means the record's completion timestamp
    minus the instant captured just before teardown, against `drain_timeout_ms` from
    `supervisor.list`.
  - On teardown and restart the server records one terminal record strictly inside the
    budget, with a clean stop (`exit_code == Some(0)` or `exit_signal == Some(15)`),
    never `Some(9)` and never at or past the budget.
  - Controls (R8) are three argv-ignoring, never-registering stand-ins under
    `tests/support/`. None registers, so the supervisor asks each by SIGTERM at drain
    start whatever its declared protocol (#125). (a) `standin-none`
    (`standin_child.sh`, `protocol: "none"`): a clean stop strictly inside the budget.
    (b) `standin-default-protocol` (the same script with the key omitted, so a Subc
    module handed `--subc` it ignores, shown running and `live: false` first): the same
    clean stop, proving an unregistered subc module is signalled rather than killed at
    the deadline. (c) `standin-ignores-term` (`standin_ignores_term.sh`, `protocol:
    "none"`, a short budget, torn down only after its ready file shows the trap is
    installed): the negative control, `exit_signal == Some(9)` at or past its budget,
    proving the harness can see a kill at the deadline. Each stand-in's elapsed time is
    printed beside its budget.
  - No real `nats-server` is declared with the key removed; that would measure the
    unknown-flag error. Off unix the row records `a1-signal-unix-only` and asserts
    nothing.
- A1 argv non-injection, unix (kept). The pid comes from provenance alone, by the
  three-outcome rule. `ps -ww -o args= -p <pid>` is recorded verbatim, and no
  whitespace-separated token equals or begins with `--subc`.
  - Fixture constraint: the `nats-server` block and both stand-in blocks declare a
    non-empty `args` list with no whitespace in any element, and the arm names each
    final element.
  - Self-checks: empty output fails, a non-zero `ps` exit fails, and the last token
    must equal the named final argument. The companion assertion is token-sequence
    equality with the declared argv, argv[0] compared after path normalisation and the
    rest byte-exact.
  - Control: the daemon's effective `connection_file_path`, read and recorded in the
    same run. A run where it is absent fails, since then the absence of `--subc` proves
    nothing. Off unix the row records `a1-signal-unix-only`.
- Grant generation. The generator in `src/grants/**`, fed the foundation's pinned
  golden literals (`box_goldenfixture`, `ckbus`, `agent_gold_a`, `agent_gold_b`,
  `room_gold_bound`, `room_gold_unbound`), emits the per-process, bus-module and
  system-account permission sets. Compared against the vendored golden under the golden
  diff rule, they differ in the header line and named added subjects only. Controls: a
  generator emitting a partial-token `CK_{ACCT}_*` or a `deny` entry fails, and an
  out-of-lexicon account (`box_a-b`) is refused at derivation with the token named.
- Signer wire shape. The harness signer's `credential.sign` and `credential.public_key`
  are checked against a committed golden request/reply pair. The pair holds: a fixed
  RFC 8032 test key; a payload whose `payload_b64` is standard base64; a reply whose
  `signature_b64` is standard padded base64 of the pure-Ed25519 signature over the
  decoded bytes; `key_id` = hex of `sha256(pub)[..8]`; and `public_key_hex` as 32 bytes
  of lowercase hex. The pair cites claustrum `57a501b`, with
  `credentials-core/src/signing.rs::sign_ed25519` and `::key_id_for_public`.
  - With `claustrum-binary` present, the same request is sent to the real binary, with
    the same key imported by ceremony if the ceremony allows a fixed key. Otherwise a
    minted key is used, and the check verifies the signature under the returned public
    key and recomputes `key_id`. Any divergence in field names, encodings or padding
    fails the row by name.
  - Controls: a signer that pre-hashes, that signs the base64 text instead of the
    decoded bytes, or that returns base64url fails. The production binary is built and
    its symbol table and dependency tree are checked for the signer module. Present
    means fail.
- Vault authorization (`claustrum-binary` only). The fixture vault receives the
  ceremony: `ck auth mint-signing-key --id signing:ck-bus-account:1`, then `sign` and
  `read` grants to `reserved:ckbus`, exact selector.
  - A real supervised `ckbus` whose `route.open` carries `ConsumerIdentity` gets
    `credential.sign` and `credential.public_key` answered.
  - Twin: the same open without the identity, arriving `Direct`, gets `not_found` for
    both.
  - With only the `sign` grant, `credential.public_key` answers `not_found` and
    `credential.sign` succeeds.
  - No test constructs `Principal::Reserved` directly.
  - A missing binary records `claustrum-binary-absent` loudly and reports SKIP.
- User JWT and seed absence. ck-bus builds a participant user JWT, signed through the
  harness signer. A real `nats-server`, whose operator and account JWTs the harness
  wrote from the same fixture keys, accepts it, with the connect nonce signed by
  ck-bus's `ckbus.nonce_sign` path. Controls:
  - one flipped signature byte is refused by the server as an authorization error;
  - a JWT signed by a key the server does not trust is refused;
  - a nonce signed with a different seed is refused.

  Seed absence, on enumerated surfaces only, for the ck-bus process and for a
  participant: its environment (`/proc/<pid>/environ` on Linux, `ps eww -o command=`
  on macOS), argv, the store root, the capture logs and the slice report contain no
  string matching `S[UAOCN][A-Z2-7]{54}`. Control: a harness-planted seed in the store
  is found by the same scan.
- Machine id and account. `{acct}` is `box_` plus the HELLO_ACK machine id, and the
  bucket and streams carry it. A supervised restart keeps the account id, so a durable
  consumer resumes at its cursor. With `account.json` deleted, boot adopts the account
  found by name, with no second account. Booting with another machine id while the old
  box account exists is refused, health down naming both ids, with no second account
  (slice 4, amending r2's new-account rule). A claims update whose read-back differs
  from the push is not applied and is reported (unit level). A damaged `account.json` fails closed
  with health down naming it. Absence of the field is not drivable against the
  acceptance daemon and is asserted at unit level against the seam: nothing is created,
  and the answer is `machine-id-absent`.
- Install bootstrap and own users. From an empty broker store and a fixture vault
  holding only the roots, ck-bus issues its own box and system users, creates the census
  bucket and the five streams with their literal bindings, writes its own census key,
  publishes on its sentinel subject, and performs a successful `$SYS` kick of a harness
  client. No fleet operator key is present anywhere.
  - A restart writes a new `own_users.json` and revokes the previous incarnation's box
    users (not its system users): the claims read back contain their keys, and a
    client presenting a revoked user's JWT is refused as revoked. The old ck-bus user's
    own JWT cannot be presented at all (its seed died, and the server verifies the
    nonce before it checks revocation), so the arm also records a harness-held key as
    a previous box user and presents that one.
  - With the signer refusing (`harness-stub` control), ck-bus creates nothing, deletes
    nothing, and retries once per sentinel period, read from its own log line. Its
    health half is asserted by the health row.
  - The system-account root refusing yields `sysaccount-absent` and no plane.
- Credential delivery and attestation. A harness participant, spawned as a supervised
  module and presenting its consumer identity, calls `ckbus.credential`. It gets a JWT
  whose subject is a fresh user key and whose grant matches the generator for its
  module id and live generation. It then connects via `ckbus.nonce_sign`.
  - The same child calling `Direct` gets `ckbus_principal_direct` and nothing else.
  - A body claiming another module id is answered for the attested id.
  - A module with no live generation gets `ckbus_generation_not_live`.
  - The stub, or ck-bus's own log, records the observed principal for every answer.
  - `ckbus` is declared `reserved: true`, so a hand-started impostor cannot register
    `ckbus` to receive these calls. That is asserted by the declaration row's refused
    HELLO, and cited here.
- A3 census write and issuance recovery. There is exactly one census key per live
  process, written before its first publish, and a respawn overwrites it at a higher
  generation with epoch 0. The value carries `credential_public`, `user_jwt_id`,
  `spawn_generation`, `credential_epoch`, `schema_versions`, `identities` and `rooms`.
  Crash arms at each boundary of the issuance order:
  - (i) after the high-water fsync and before signing, and (ii) after signing and before
    the census write: the restart leaves no census entry, no usable credential, and
    nothing to roll back. The next `ckbus.credential` issues at a higher epoch.
  - (iii) after the census write and before the answer: the entry names a key nobody
    holds. The child's refetch issues the next epoch and revokes the superseded user.
  - (iv) damaged `epoch_high_water.json` while a generation is live: no epoch is issued
    for it, the refusal is recorded, the file is untouched and named, health is
    down/`Unavailable`, and open connections keep serving. A later generation issues
    normally.
  - Longevity, under an injected sweep clock: a long-lived participant keeps its key.
- A9 grant conformance, live-server half. Under generated grants on a live server:
  - The per-process user may pull and ack its own consumers, get and watch the census
    (including the ordered-consumer create and delete under `KV_CK_{ACCT}_CENSUS`),
    publish to `ck.{acct}.effect.dead` and to a bound room.
  - It is refused (server-side `Denied`) on another identity's ack, pull or info, on a
    consumer create on the four workload streams, on a census write, on an unbound room,
    and on any `$SYS` or sentinel subject.
  - The bus-module user may get, watch, put and delete the census and manage the five
    streams, and is refused every workload publish.
  - The system user may send the kick, the claims update and the claims lookup, and
    nothing else. These subject strings are what the golden records.
  - The default-inbox client connects and is refused at subscribe.

  The golden-commit half (regenerating prefrontal's
  `tests/grants/permission_golden.txt` from this generator) records
  `prefrontal-seat-unnamed`. The subconscious slice fails its own arm if it writes
  outside its worktree.
- A3 revocation. The three steps produce the foundation's observables: `Unavailable` on
  the severed socket, exactly one `$SYS` disconnect event, and `Denied` on an explicit
  reconnect within 5 s. The push in step (1) severs the connection itself, and the
  event's reason is `Credentials Revoked`, not the kick's (slice 6, measured against
  nats-server v2.15.0 in `tests/revocation.rs`). The kick in step (3) is a backstop for
  a connection the push did not sever, and it normally finds nothing: the server
  answers `no such client or leafnode id`, which counts as already gone, and any other
  kick refusal fails the step. Revoking before kicking is the safe order: a kick alone
  lets the client reconnect at once with the same JWT (the control below). A second reconnect after a server restart is still
  refused, proving the claims update persisted. Exactly-once is asserted on the defined
  observables.
  - (i) A kill between any two steps, or after a step and before its progress update,
    ends with one revocation entry and one disconnect event, and replays are no-ops.
    That includes a crash just after the zero-step record.
  - (ii) Progress corrupted after step (1), with the census entry present: the inputs
    are re-derived from the census and the steps replay.
  - (iii) Corrupted after step (2): the file is cleared and nothing is issued.
  - (iv) Corrupted with the census read failing: the module defers.

  Controls:
  - an unkicked participant keeps working;
  - a participant with its trait-layer watch disabled is refused on reconnect all the
    same;
  - a kick without the claims update reconnects successfully;
  - ck-bus merely dropping a key from memory makes the child's next connect fail inside
    `ckbus.nonce_sign` (`ckbus_credential_superseded`), unseen by the server. That is
    the local-versus-server distinction the dropped vault-delete control used to draw;
  - a bus-module user generated without census read fails step (1) outright;
  - an operator-key signature refused fails step (1) and pushes nothing, and the census
    entry is untouched.
- Spawn-stream consumer (kept). Both op names appear in `subc_ops`, and the observed
  list is recorded. Under the six mutation controls of the spawn-stream brief the
  consumer converges. The two refusal codes and their detail keys are asserted
  byte-exact, `ring_bound` is read from the reply, and a lagged stream resubscribes from
  its last cursor. The consumer issues and revokes nothing.
- Spawn reconciliation and census recovery. Reconciliation revokes entries with no live
  generation, and serves live generations with no entry on their first
  `ckbus.credential`. It converges on each mutation control. A ck-bus restart while
  participants keep running, and a respawn during the subscription gap, end with
  exactly one usable census identity per live generation after the refetch. The cursor
  is read back from the fixture store, and a corrupted `spawn_cursor.json` produces a
  fresh snapshot.
- Module health answer (kept). `supervisor.health_probe` for `ckbus` returns the
  byte-exact class at `metrics.class`, written by the module into `health.check`, and
  every arm names that op. The control removing `health.check` advertisement reports
  `Unknown` and is never probed. This row owns the health half of every deferral control
  (bootstrap's refusing signer, the failed census read, a damaged `account.json`): down,
  `Unavailable`, `detail` naming the cause, all through the round trip. It runs under
  the harness sentinel values parsed from the log line, within the 120 s budget.
- A8 sentinel probe. Every down transition is bounded by `3 * period + timeout` from the
  parsed log line.
  - Server up: `bus.health.up` within two periods, with the reply on the module's own
    inbox prefix.
  - Server stopped, reply publish permitted: down/`Unavailable` within the bound, read
    through the probe.
  - Reply publish removed against a healthy server: down/`Denied`.
  - Before the first answer the verdict is the initial down/`Unavailable`. This includes
    the restart arm that persists `up`, stops the broker, restarts ck-bus and queries
    before the first probe.
  - With the Claustrum stub refusing `credential.sign` (`harness-stub`): ck-bus stays
    `running`, `restart_count` is unchanged after three periods, and the class is
    `Unavailable`.
- Dead-letter. A claimant driven to cap exhaustion publishes the record and terms. With
  a crash injected between the two, `c_ckbus_dead` observes one record per message id,
  and the item is never both terminated and unrecorded. Both connections authenticate
  under harness-signer-signed JWTs ck-bus built.
- Membership lifecycle. ck-bus re-issues at the next epoch, overwrites the census key,
  adjusts the room consumer's `filter_subjects` and revokes the superseded epoch, all
  within 10 s of the membership event the foundation names. The client reconnects by
  refetching. Publish is refused before and accepted after for a join, the reverse for a
  leave, and the superseded epoch is refused on reconnect. The spawn snapshot shows the
  original generation throughout. Concurrent and duplicate changes converge on one
  highest epoch. The arm quotes the foundation's membership op and caller from the
  vendored copy, and without that it records `membership-contract-unpinned`.
- Signer outage and restart (the amendment's arms).
  - Kill ck-bus under the supervisor: nats-server's pid from provenance is unchanged,
    connected participants keep publishing and receiving, and a new participant's
    connect fails until ck-bus is back. Twin: restart nats-server with ck-bus up, and
    every participant re-signs through `ckbus.nonce_sign` and reconnects.
  - ck-bus killed and restarted: established connections survive. A participant forced
    to reconnect gets `ckbus_credential_superseded`, refetches, reconnects at the next
    epoch, and its superseded user is revoked. Participants never forced to reconnect
    stay connected.
  - Controls: with ck-bus down and nats-server restarted, every participant is
    disconnected and stays so until ck-bus returns, which is the accepted failure mode
    and the reason the lifetimes must stay independent. ck-bus is never the parent of
    nats-server.
  - Expiry arm, once `user-jwt-ttl-unpinned` is discharged: a JWT is re-issued before its
    `exp`, and an unrefreshed one is refused after it.
- Federation account and isolation (`nats-federation-rig`: local subjects off the leaf;
  account routing). ck-bus's federation user can publish to another machine's inbox and
  subscribe to its own. It cannot publish in the box account, and it never carries an
  effect subject.
  - A hub-side subscription on `ck.{own}.>` receives no local traffic. The control binds
    the leaf to the box account and shows that it would.
  - The local delivery path, once settled, delivers an opened PEER, WAKE or ROOM body to
    the local consumer with no box-account publish by ck-bus. An effect-family frame has
    no path at all.
- Leaf configuration (`harness-stub`). `callosum.hub_read` is read every 60 s and on
  every reconnect. One hub is accepted and two are refused. The pin is the authority:
  a candidate presenting another SPKI is refused before a credential is sent. A
  tombstoned or changed hub withdraws the leaf document, with its disconnect observable
  asserted in the link row within 75 s. No hub row leaves the federation inert and
  health unchanged. The stub's `hub_read` shape is the foundation's (weaker citation)
  until CALLO's served shape is recorded.
- Leaf link, labelled. A message published toward another machine crosses a harness
  hub on the leaf credential, and an inbound dial to the leaf fails. The arm and the
  report name the shape built. Under shape 3, the connect error names the signer when
  the signer is down. No seedless leaf authentication is claimed until the shape is
  chosen.
- Seal outbound. For a recipient in `callosum.peer_keys_read`, the frame on the
  outbox carries no plaintext body and no digest header. Opening it with the recipient's
  fixture seal key yields a signed context covering every bound field. A peer without a
  record gets no frame and a reported wait, with no plaintext fallback, and a broadcast
  to N machines yields N frames and none addressed to a hosted hub. Controls: tampering
  with the destination, the sequence or the body breaks the inner signature, and the
  HPKE `info` differing by one byte fails to open.
- Open inbound. The harness signer serves `credential.open` in CKCRED's contract shape
  from a throwaway KEM key. That shape is pinned by a golden pair built from the RFC
  9180 A.2 base-mode vectors, and the golden check runs against the real binary once
  `kemkey-open-unlanded` clears. The real-binary half also asserts the authorization:
  the `open` grant opens, a `sign` or `read` grant alone does not, a handle is refused,
  and a `Direct` caller gets `not_found`. It asserts the failures too: a wrong key or
  tampered `aad` gets `open_failed`, and a signing-key id gets `kind_not_openable`. A
  valid frame is opened, verified, delivered by the local delivery path, and its
  (sender, sequence) is recorded before the ack. The following are quarantined with their own disposition, never
  acked as delivered: an unsigned frame, a frame signed by a key other than the sender's
  current record, a destination and subject mismatch, a retired generation, and
  garbage. Claustrum down: frames wait unopened and are delivered after it returns.
- Federation sequence and crash. Sequences are per (sender, recipient). A kill after the
  reservation fsync and before the publish ack re-seals the redelivered original with
  the same sequence. The recipient delivers once, sees no gap, and dedupes the duplicate.
  A frame withheld by the harness hub shows up as `fed.gap` naming the sequence. A
  replayed old frame is deduped.
- Split store-and-forward (`nats-federation-rig`: sourcing across a split;
  subject-filtered purge). A message sent while the recipient is disconnected is
  delivered after reconnect, beyond JetStream's duplicate window. A full outbox refuses
  with `Unavailable` naming the limit, never dropping. Purging one recipient's inbox
  leaves the others'.
- Peer removal fence. A peer disappearing from `callosum.peer_keys_read` stops being
  sealed to within one read period, and its later frames are quarantined. The report
  names the fence. Revoking the removed peer's hub leaf credential is not asserted here;
  it is the ceremony's.
- A6/A7 prefrontal re-run. A6 conformance and A7 no-regression stay green on
  prefrontal's rig with the module present, and `bus-module-unlanded` appears in no
  recorded skip. No subconscious slice owns this row, and it records
  `prefrontal-seat-unnamed` until an owner names the seat, the campaign, the path of the
  built binary, and the owner of the rig-side vocabulary enforcement.

## Open questions and source contradictions

Each item is reported, not resolved silently. Where the spec had to act, the item says
what it did and who can overturn it.

1. Record names versus credential ids. The foundation's Credentials lines name vault
   records `nats.operator.{acct}`, `nats.account.{acct}` and `nats.sysaccount.{acct}`, and
   per-process `nats.{module_id}.g{generation}.e{epoch}`. The amendment keeps only root
   keys in the vault, created as `signing:<provider>[:<generation>]`, and CKCRED named
   `signing:ck-bus-account:1`. The per-process grammar now names no record.
   `cortexkit-bus-naming` still constructs `process_record_name` and
   `system_account_record_name` (slice 0 record). This spec uses the credential ids and
   does not use the record-name constructors. A foundation edit should retire the old
   grammar (owner ALF).
2. Grants beyond the account key. CKCRED named ck-bus's grants on the account key only.
   A revocation re-signs the account JWT, which the foundation says the box-local
   operator key signs. So ck-bus needs a `sign` grant on the operator key, and that key
   then signs for a module at runtime rather than only at install. This is unagreed
   with CKCRED and the operator. The system-account and federation-account key ids are
   unagreed too.
3. Who writes the server's static configuration (`server-config-writer-unnamed`). The
   foundation gave install step (4) to "SUBC's installer calling CKCRED". The amendment
   replaced steps (1) to (3) with the ceremony and said nothing about step (4).
4. Restart and in-memory seeds. The amendment says a reconnect "succeeds once the
   signer returns". That was measured with a signer that kept its key. A restarted
   ck-bus has lost every seed, so reconnects succeed only after a credential refetch,
   which needs the client ops (`ckbus-client-ops-unagreed`). This spec specifies the
   refetch. ALF should confirm the amendment's sentence is read that way.
5. JWT expiry is named as half of revocation and has no value (`user-jwt-ttl-unpinned`,
   ALF). Until it is pinned, ck-bus issues no `exp`, and the residual is named in
   Credentials.
6. Cold-start herd. The foundation's 16-per-second admission bound was enforced by a
   vault-side limiter the amendment dropped. Under D every nonce signature goes through
   ck-bus, so ck-bus is the natural admission point. That is unagreed, has no owner, and
   no row here claims it.
7. `{acct}` source. The foundation says `box_<roster_host_id>`. The machine-id design
   (decided 2026-09-23, later) says the daemon's machine id. This spec follows the
   machine-id design. The foundation text should be amended (ALF).
8. `{acct}` stability versus machine-id change. The foundation fixes `{acct}` for the life
   of the box, and the machine-id design lets `adopt` change it at the next daemon
   start, asking ck-bus to start a new account. Slice 4 (ALF) refuses instead while
   the old box account exists, and the operator decides the migration.
   Whether the new account needs new root keys is open, since the credential ids carry
   no account token (CKCRED, operator).
9. Local delivery of opened federation frames. nats-federation r3 says "B's ck-bus opens
   the body and delivers it over B's local bus". The same text, and the project rule,
   say ck-bus holds no publish rights in the box account. Both cannot hold if delivery
   is a ck-bus publish. The spec keeps the publish restriction and names a server-side
   account import as the candidate, pending `nats-federation-rig` and
   `fed-foundation-amendment-unlanded` (ALF, SUBC).
10. Outbound capture. r3 says A publishes `ck.{b}.peer.…` and B's stream catches it
    unchanged. On A, the foundation's disjoint bindings cannot durably capture another
    machine's `ck.{other}.…` without overlapping the local streams. Participants also have
    no publish grant outside their own `{acct}`. Both belong in the owed foundation
    amendment (ALF).
11. Per-sender sequence. r3 numbers per sender. With more than two machines, a
    per-sender counter makes every recipient report gaps for frames sent elsewhere. This
    spec numbers per (sender, recipient). CKCRED and ALF can overturn it.
12. Rotation wording. CKCRED first said rotation is "a new generation id, not a
    replace". It then said rotation uses `mint-signing-key --replace` only, with
    `record_version` non-monotonic across delete-and-remint and restore. The spec
    follows the later statement. The foundation amendment's
    `signing:<provider>[:<generation>]` still reads like the earlier model.
13. The `msgsig` id. CKCRED gave `signing:msgsig:<host>:1`, and later `signing:msgsig`.
    `<host>` is undefined. This spec reads it as the machine id, pending CKCRED.
14. The leaf credential. The foundation's per-host leaf record `nats.leaf.{roster_host_id}`
    is "minted at pairing" in the vault. nats-federation r3 makes it a key of the hub's
    per-user account. Neither says whether this machine's vault holds the leaf seed,
    which decides the leaf-signing shape (CALLO, CKCRED, SUBC).
15. HPKE `info` and `aad`. r3 puts `sender_machine_id|recipient_machine_id` in `info`
    as binding. CKCRED's `credential.open` contract says the vault interprets neither,
    so the binding that counts is the inner signature, which this spec requires the
    opener to check.

## Chair rulings (normative index)

The rulings are folded into the sections above. This index lets a reviewer see what
was decided and stop re-litigating it. Where an index line and a section disagree, the
SECTION governs.

- R2: every gate has an owner, named in Intent's table. Commons changes are authored by
  ALF and merged by SUBC. (r1's R2 named CKCRED as owner of four vault ops. CKCRED never
  agreed, and those gates are deleted.)
- R3: slices are strictly sequential, fences are disjoint, and `src/main.rs` and
  `tests/harness/mod.rs` are integration refs taking one `mod` line and one wiring
  call per slice.
- R4: every health arm reads the class through `supervisor.health_probe`.
- R5: recovery that destroys or clears anything needs a successful authoritative read.
  An unreadable census concludes nothing.
- R6: the supervised pid is read from `supervisor.provenance`, narrowed, at
  `daemon_observed.pid`, by the three-outcome rule.
- R7: the argv read is `ps -ww -o args= -p <pid>`. Unix scope is Linux and macOS, and
  `a1-signal-unix-only` records Windows.
- R8: the A1 controls are three argv-ignoring, never-registering stand-ins: under
  `protocol: "none"`, under the default protocol, and a SIGTERM-ignoring one as the
  negative control (amended for #125, when an unregistered subc module began being
  signalled at drain start).
- R9: WITHDRAWN in r2. The root-record lookup and mint-on-absence rule has no subject
  under design D, because roots are ceremony keys and ck-bus mints nothing in the vault.
- R10: the route targets in acceptance are the real ids `claustrum` and `callosum`,
  served by harness modules registered under those ids. This discharges
  `route-target-ids-unnamed`. Amended by R12.
- R11: `stub-reply-shape-unrecorded` is a fire-time condition naming the op, and the
  stub shape table is data.
- R12 (r2): three serving sides. `harness-stub` serves shape. `harness-signer` serves
  real signatures in claustrum's exact wire shape, from throwaway fixture keys. A
  signature is a pure function of key and bytes, so a pass proves the JWT ck-bus built
  is accepted by a real `nats-server`, and proves no vault authority.
  `claustrum-binary` alone serves authorization rows, and its absence is a loud skip,
  never a pass. Every row states its side.
- R13 (operator, 2026-09-24): leaf signing is shape (3), a Go binary embedding
  `nats-server` whose `RemoteLeafOpts.SignatureCB` gets each connect signature from the
  vault, so the leaf credential's seed never touches disk. The rig settled that stock
  `nats-server` cannot do this: it reads only a creds file, and re-reads it at every
  reconnect. The Go binary is built, signed and released with the fleet's other
  binaries. Still open under `leaf-signing-shape-unchosen`, for SUBC and CKCRED: how
  the callback reaches the vault. SUBC's recommendation is that the Go binary be a subc
  module with its own launch nonce and an exact `sign` grant on the leaf key, which
  needs a minimal Go subc client (handshake, HELLO, route.open, request). The
  alternative, routing the callback through a local ck-bus socket, turns ck-bus into a
  signing oracle for any process running as the same user.
  Settled with CKCRED the same day. A third option, the Go binary as ck-bus's child
  signing over an inherited pipe, was weighed and rejected: the embedded server is the
  whole bus, not only the leaf, so tying it to ck-bus's lifetime would drop every
  module's bus connection on each ck-bus restart. The measured property is that with
  ck-bus down only new connections wait. So the Go binary is a subc-declared module
  with its own launch nonce, and the exact `sign` grant on the leaf key goes to its own
  reserved principal, never to `reserved:ckbus`. Two obligations carried over from the
  pipe option: it signs only nonce-shaped input (a length cap and the NATS nonce
  charset), and its vault connection is CLOEXEC toward anything it spawns.
  `leaf-signing-shape-unchosen` is discharged; the module id is named when slice 12 is
  written. The Go module's subc client presents ConsumerIdentity {module_id,
  launch_nonce} on its route.open. Slice 12's acceptance includes one real
  `credential.sign` arriving as `reserved:<that id>`, confirmed by CKCRED from the vault's
  first-use row. Without the identity the call arrives as `direct` and is refused as
  not_found, which is indistinguishable from a missing grant.
- R14 (operator, 2026-09-24): revocation uses a narrow signing key plus short-lived
  user tokens. The operator's root identity key stays out of daily use; it vouches for
  an operator signing key that ck-bus alone may sign with (exact `sign` grant to
  `reserved:ckbus`), and ck-bus re-signs the account JWT's revocation list with that key.
  If the signing key is ever exposed, the root key removes it from the operator JWT.
  User JWTs carry a short expiry that ck-bus renews while the module runs, so a token
  that escapes revocation stops working at expiry. This resolves open question 2. The
  key id and the expiry value are unagreed: the key needs CKCRED and the operator (a
  ceremony like `signing:ck-bus-account:1`), and the expiry is `user-jwt-ttl-unpinned`
  (ALF), with 15 minutes proposed by SUBC. Both gate slice 6 only.
