# Cloud Usage Accounting: the CloudUsageFact contract

Status: DRAFT r10 (r9 corrected: a duplicate is chained but never re-applied, keyed by the correction's own id [#44]; r9 = a re-sent correction is a duplicate [#41]; r8 = batch-level fields are acknowledged in the reply [#37]; r7 = the `metered_since` wire as shipped [#30][#33]; r6 = the producer half of `metered_since` [#29]: durable start, folds begin there, partial-hour reason; r5 = `metered_since`, the lower bound on a service's metering [#27]; r4 = the first-producer rulings [#21][#22]: byte_hours arithmetic, conflict events never settle, deny-unknown-fields enforced per fact, DO-SQL rows counted, class A retry undercount; ASTRO seat review pending) — custody SUBC, seats ASTRO (domain owner) / ENGRAM (first
producer) / CKCRED (cloud infra custody). Ufuk directive 2026-07-20: build the non-politic
half of cloud cost accounting first — per-account metering, spend visibility, and
user-set limits. Invoice/billing folds are OUT OF SCOPE here (a later doc consumes this
one's ledger).

## 0. Problem and posture

CortexKit's cloud offerings consume real money per account: today engram (R2 storage, DO
compute, class A/B ops, egress), soon federation rendezvous/relay edge compute, org
daemons, and cloud AI. Users must be able to SEE what they consume (honestly, itemized)
and BOUND it (self-set limits). The platform must be able to account for every cloud
dollar per account.

Posture, inherited from the fleet's metering discipline:

- **Metered at the point of service, cloud-side, authoritative.** The client is never
  consulted for a billing-grade fact. (This is the structural opposite of the local
  plugin-usage lane; the two must never be conflated.)
- **Facts are raw quantities, never costs.** Pricing is a separate versioned rate-card
  snapshot applied at read time. Rate changes never rewrite history.
- **Honest cost states only** (astrocyte doctrine): priced / unpriced / not-yet-priced.
  A quantity with no rate row reads UNPRICED, never a fabricated number.
- **Append-only, hash-chained, idempotent** — the CKCRED audit-ledger discipline.

## 1. The fact

One canonical shape, every cloud service emits it, nothing else is billing-grade:

```jsonc
{
  "schemaVersion": "cortexkit-cloud-usage-fact/v1",
  "accountId": "01KXJJHK4V6DB42W1YAN2XD9QN",   // CKCRED account ULID — THE attribution key
  "service": "engram",                           // emitting service id, closed registry (§5)
  "resource": "r2_storage_byte_hours",           // closed per-service resource registry (§5)
  "quantity": "1213462838.0",                    // decimal STRING (commons money/quantity discipline, no floats)
  "unit": "byte_hours",                          // pinned per resource; unit mismatches reject
  "periodStart": "2026-07-20T09:00:00Z",         // UTC hour bucket, inclusive
  "periodEnd": "2026-07-20T10:00:00Z",           // exclusive
  "factId": "engram:r2_storage_byte_hours:01KXJJ…:2026-07-20T09",  // deterministic (§2)
  "emittedAt": "2026-07-20T10:00:14Z",
  "meterVersion": "engram-worker@45834b27"       // provenance: which code measured this
}
```

Field rules:

- `quantity` is a canonical ASCII decimal string (the cortexkit-model-catalog money
  discipline; half-even rounding only at display/pricing boundaries, never in facts).
- deny-unknown-fields; unknown `service`/`resource`/`unit` values REJECT at ingest
  (closed registries, §5) — a typo'd resource must fail loud, not mint a new meter.
  (r4) Ingest enforces this per fact: a fact carrying a field it does not know is
  rejected with a named reason, never accepted with the field dropped, so a producer
  that drifts ahead of the ledger learns it from its first batch.
- `meterVersion` makes calibration drift diagnosable after the fact (which build
  measured this hour?).

## 2. Idempotency and bucketing

- **Hour buckets, UTC, aligned.** One fact per (service, resource, account, hour).
  Hourly is fine-grained enough for limits and coarse enough that a month is ~720 rows
  per meter.
- **factId is deterministic** over (service, resource, accountId, periodStart). Re-emission
  (worker retry, crash replay) is an idempotent upsert; a re-emission with a DIFFERENT
  quantity for an existing factId is a LOUD conflict (`fact_conflict`), recorded and
  alerting — it means a meter double-ran with different readings, which is a bug, never
  averaged away. Projection stays first-write-wins on conflict.
- **RESTATEMENT (r2, CKCRED)**: calibration will eventually prove some approximated meter
  wrong over already-emitted hours; the correction path is a NEW emission event
  `{kind: "correction", supersedes: factId, quantity, reason, meterVersion}` — the
  projection updates to the corrected quantity CARRYING correction provenance (surfaces
  render "restated"), the log keeps original + correction forever, and corrections are
  themselves conflict-checked (correcting a correction chains explicitly). History is
  never edited; known-wrong facts are never left uncorrected.
  (r9, corrected in r10) A correction is idempotent the same way an emission is, keyed
  by the correction's own factId: re-sending the same correction is answered
  `duplicate`, is chained as a `duplicate` link, and is never applied to the projection
  again; the same correction id with different content is a `conflict`. The chain is
  the record of every event received (so a re-send after a lost reply stays visible)
  and the projection is where idempotency lives. Two corrections with different ids and
  identical content are two events, and the second is a new restatement. Facts already emitted before
  `metered_since` existed are not restated just to add a partial-hour reason: the read
  path derives "partial" from `metered_since` itself.
- **Late facts are legal** (a worker may fold an hour late); consumers read
  watermark-style (facts through hour H complete when the service's emission watermark
  passes H). Each service publishes its watermark as part of emission.
- **(r5) The watermark is only an upper bound.** "Complete through H" cannot say when
  metering began, so on its own a reader takes every hour before a service started
  metering as complete with zero usage, which is false: those hours were not measured.
  Each service therefore also publishes `metered_since`, the UTC instant its current
  metering began for that account, beside the watermark. Readers render hours that end
  at or before `metered_since` as "not metered", never as zero, and the hour containing
  it as partial for every meter, not only storage. `metered_since` is set once, when an
  account's metering first begins, and never moves later: it describes where the
  record starts, not gaps inside it, and a producer that loses metering state mid-record
  is a correctness incident handled by restatement (above). The ledger cannot tell "no
  usage" from "not measured" by itself; this field is how it learns.
- **(r6) Producer obligations for `metered_since`.** The producer records the instant
  durably when an account's metering first begins, before serving the request that
  starts it, and every meter counts from that instant: a fold (derived or sampled)
  starts at `metered_since`, never at its own first call, because a fold that starts
  late silently shortens its first hour. A fact for the hour containing `metered_since`
  carries a `reason` naming the instant, so the fact itself says it is partial. A
  derived meter may backfill the gap between `metered_since` and its first fold as a
  late fact only when its state provably did not change in that gap (for engram's
  storage, no used_bytes mutation recorded between the two instants); otherwise the
  gap stays partial and says so.
- **(r7) The `metered_since` wire.** A batch may carry `meteredSince` beside
  `watermarkThroughHour`: a UTC instant at second precision (`2026-09-25T02:27:16Z`),
  not required to fall on an hour boundary. The ledger keeps one value per
  (service, account), set once: the first value is recorded, the same value again is a
  no-op, and a different value, earlier or later, is not applied. When the batch
  carried the field, the reply adds
  `meteredSince: {outcome: recorded | unchanged | refused, stored: <instant>}` beside
  the per-fact `outcomes`. A refusal never touches the per-fact outcomes, so the facts
  still land. A refusal always means the producer lost its own metering state or has a
  bug: the producer surfaces it as an incident, the same way it treats a flagged fact,
  and never re-sends a new value. The read path returns `metered_since` on each
  service's watermark row, null when the service never published one. Where a producer
  did not record the exact first-request instant (engram's first account, metered from
  the deploy of the metering Worker), it publishes the earliest instant counting could
  have begun, which is an honest lower bound.
- **(r8) Batch-level fields are acknowledged, not refused.** Deny-unknown-fields applies to
  each fact, not to the batch envelope: refusing a whole batch over an envelope field
  would hold valid facts back. Instead, every batch-level field the ledger applies is
  acknowledged in the reply (as `meteredSince` is), so a producer tells "applied" from
  "ignored by a ledger that predates the field" by the acknowledgement's presence, and
  keeps re-sending an idempotent field until it is acknowledged. A new batch-level field
  must therefore be idempotent and must come with its acknowledgement.

## 3. Meter classes (how quantities are obtained)

Three classes, declared per resource in the registry:

- **counted**: the serving code increments counters in-band (class A/B ops, egress bytes,
  chunks stored). Exact by construction.
- **derived** (r3): exact-by-construction FOLD over transactional state — the state
  changes only under recorded mutations, so past-period quantities derive exactly and
  retroactively (engram's R2 gauge in bytes × 1 hour per completed hour, which is the
  byte_hours unit; r3 said "× 3600", which would be byte-seconds. Folded lazily at
  next wake as late facts). Distinct from sampled: no cadence bound, exactness inherited
  from the state's transactionality.
- **sampled**: point-in-time gauge folded over the hour (R2 bytes stored -> byte_hours).
  Exactness bounded by sample cadence; cadence declared in the registry.
- **approximated**: self-measured proxy for a provider-billed quantity (DO wall-clock
  GB-s — Cloudflare bills active-duration wall clock, which the worker can only
  approximate from its own timestamps). MUST carry a calibration bound (§6) before any
  user-facing surface cites it as cost.

## 4. Ledger custody and storage

- The **usage ledger lives in the account-service cloud infra** (CKCRED custody): it is
  account-scoped, must survive any single service, and the account service already has
  D1 + per-account DOs + JWKS + the org layer the ledger will need for team mode.
- **Storage is TWO structures (r2, CKCRED — resolves the append-only-vs-upsert tension)**:
  (a) `usage_emissions` — append-only, hash-chained per account; EVERY emission event
  lands verbatim (first emission, idempotent re-emission, conflict, correction), chain
  head updated in the same guarded batch. (b) `usage_facts` — the current-state
  PROJECTION with §2's upsert semantics, derivable from the log at any time. Consumers
  read the projection; auditors read the log. D1 rows suffice at these volumes.
  HONESTY CAVEAT (stated, not implied away): the chain is tamper-EVIDENT for interior
  edits/reorders but NOT rollback/truncation-resistant without an external anchor — an
  operator with D1 write access can drop a suffix. Acceptable v1 threat posture (bugs
  and drift, not a hostile platform); an external anchor can be added later.
- **Emission path (r2, both seats converged): Cloudflare SERVICE BINDING** — emitting
  worker → account service, worker-to-worker within one Cloudflare account: no public
  HTTP surface, no bearer credential, no rotation, unforgeable within the account by
  construction. THE ATTRIBUTION PIN: ingest stamps `service` FROM THE BINDING IDENTITY
  (one binding per emitting service); a payload `service` value must MATCH the binding
  or reject `service_mismatch` — a buggy/compromised service structurally cannot mint
  facts as another service. `accountId` remains the only attribution the emitter
  asserts. Usage accrues whether or not the user is logged in anywhere. Future
  cross-account emitters (org daemons) use the fleet service-JWT profile — same claim
  shape, different transport; NOT built now.
- **Producer-side buffering (r2, ENGRAM)**: emitters buffer facts durably at the point
  of measurement (engram: a pending-facts table + emitted-watermark register in the
  account DO's SQL state, committed in the SAME transaction as the mutation being
  metered — crash/replay can neither lose nor double a fact; factId upsert absorbs
  replays). The buffer schema carries `{kind: emission|correction, supersedes?, reason?}`
  from day one so restatements ride the same drain. Emission to the ledger activates
  independently of measurement — late facts are legal, so producers instrument before
  the ingest endpoint exists and drain when it does.
- **Batch ingest outcome vector (r3, CKCRED [#10])**: emission is a batch, idempotency
  is PER-FACT — the ingest response is a per-factId outcome vector
  `[{factId, outcome: accepted | duplicate | conflict | rejected(reason)}]`, never a
  batch-level status. The producer's emitted-watermark advances past a fact ONLY on
  accepted|duplicate (both mean the ledger durably holds the event); conflict|rejected
  facts stay buffered, flagged for inspection, never silently retried into the same
  conflict. (r4) The ledger chains a `conflict` event as an audit record that a
  conflict happened, never as settled usage: the projection keeps the first write, and
  limits and the read path count only projected quantities. Partial success is normal (one malformed fact never wedges a batch). The
  ingest applies the whole batch in ONE guarded write (log appends + projection upserts
  + chain head) so a mid-ingest crash is all-or-nothing and the outcome vector is
  truthful by construction.
- **Read path**: account-authenticated self-read (verified by the same account-JWT
  verification the org endpoints use) and org-admin rollups later via the org layer.
  Responses carry per-service emission WATERMARKS and (r5) `metered_since` so consumers
  render "complete through hour H" and "not metered before T" honestly rather than
  implying a live total or a zero history. ASTRO pulls through the same read
  path.

## 5. Closed registries

Two registries, versioned in this doc (amendment = seat-reviewed doc change):

**Service registry**: `engram` (first), `rendezvous`, `relay`, `wernicke`, `org-daemon`
(reserved, not yet emitting).

**Resource registry v1 (engram)** (r2: ENGRAM's producer review added the DO-SQL class —
stage-2 economics showed ROW WRITES, not R2, are the dominant engram cost driver; omitting
them would show users pennies of R2 while hiding the actual dollar driver, the precise
dishonesty this doc exists to prevent):

| resource | unit | class | notes |
|---|---|---|---|
| `r2_storage_byte_hours` | byte_hours | derived | the DO's transactional used_bytes gauge changes only at reserve/finalize/expire/GC; byte_hours for completed hours derive exactly at next wake (late facts) — no hourly DO wakes, idle accounts fold lazily |
| `r2_class_a_ops` | count | counted | writes/lists; (r4) engram counts Upload claims at the DO, so a retried object PUT the DO never sees is a known undercount, stated in the meter notes |
| `r2_class_b_ops` | count | counted | reads |
| `do_sql_rows_written` | count | counted | (r4) from SQLite's own per-cursor row counters on every request; r3 had a drain-plan proxy |
| `do_sql_rows_read` | count | counted | (r4) as above |
| `do_storage_byte_hours` | byte_hours | sampled | DO SQL state size |
| `do_compute_gb_s` | gb_seconds | approximated | wall-clock active duration proxy |
| `do_requests` | count | counted | |
| `worker_requests` | count | counted | Workers-standard billing |
| `worker_cpu_ms` | ms | approximated | |
| `egress_bytes` | bytes | counted | worker-measured response bytes |

Storage-gauge honesty tiers (ENGRAM): DO gauge (exact, free) → periodic R2 list-by-prefix
reconciliation (catches divergence; quarantined/delete-pending/preimages/wrapper/marker
are all billed bytes and MUST be in the gauge) → monthly Cloudflare billing calibration
(ground truth). Known gauge divergences (reserve-before-upload, 24h expiry refunds) are
recorded in the calibration notes.

**ZERO-KNOWLEDGE FENCE (r2, ENGRAM — contract-level, applies to every service)**: facts
carry account + blind quantities ONLY. No resource dimension may ever be content-shaped
(no per-catalog/per-path/per-session server-side breakdowns — for zero-knowledge
services the server structurally cannot know them, and the registry must never create
pressure to learn them).

## 6. Calibration (the step that makes the numbers honest)

Before any approximated/sampled meter feeds a user-facing cost figure: run the meter
against a real account for a full billing week and reconcile against Cloudflare's own
billing/analytics as ground truth. Record the observed error bound in this doc per
resource. Surfaces cite the bound where material ("~±5% estimate" on DO compute). A
meter whose error is unbounded stays UNPRICED on user surfaces. First calibration run:
Ufuk's real account (the only real cloud consumer), engram-worker, starting when ENGRAM
instruments. Re-calibrate on meterVersion changes to measurement code.

## 7. Pricing (separate artifact, applied at read)

- A versioned **rate-card snapshot** (Cloudflare public pricing first) maps
  (service, resource, unit) -> rate, exactly as astrocyte's models.dev snapshot maps
  models -> token prices. Same honest states; same snapshot-pinning discipline.
- Cost is computed at read/fold time: `quantity × rate`, decimal-string arithmetic,
  half-even at the nanodollar boundary (commons PR #4 discipline).
- Rate-card custody: ASTRO (it is the pricing brain); the snapshot lives beside the
  models.dev snapshot.

## 8. Limits (user-set, enforced at the point of service)

- Limits are per-account, per-service, user-authored:
  `{service, resource | "monthly_cost_usd", bound, posture}`.
- **Enforcement is cloud-side at the serving service** (engram's per-account DO is the
  natural chokepoint), reading its own emitted facts + the account's limit config.
  The account service stores limit config (authored from the app/CLI, account-authed);
  services pull it on the same cadence as their emission watermark.
- **Fail postures are per-resource and safety-biased, declared in the registry**:
  - storage cap -> reject NEW captures/uploads; never delete existing data to get under
    a cap.
  - compute/ops cap -> degrade (defer non-essential work, stretch cadences) before
    refusing; refusal is loud and typed (`usage_limit_reached`, carrying the limit and
    the period).
  - No limit configured = unlimited (metering is always on regardless).
- Limits math uses the same facts users see — one truth, no shadow meter.
- **v1 reality (r2, ENGRAM)**: the storage cap is ALREADY BUILT — engram's reserve op
  enforces used_bytes vs quota_bytes with exactly this posture (reject new, never
  delete); user-set storage limits are quota_bytes becoming account-author-writable
  through the limit config. Ops/compute caps come after calibration establishes normal
  ranges.

## 9. Local surfacing

- ASTRO pulls the account's cloud facts (read path, §4) and joins them with local AI
  spend into ONE view: `ck spend` (or `ck astro …`) shows local AI + cloud usage,
  priced where honest, with the same three-state honesty everywhere.
- The Swift app/CK app reads the same rollup. Display-lane only; no local surface is
  ever the enforcement point.

## 10. Explicitly out of scope here

- Invoice generation, the ×1.3, multipliers, and anything token/economy-shaped (a later
  doc folds THIS ledger into invoices; this doc's ledger is valuable standing alone).
- Local plugin-usage attribution (forgeable-by-design, per-account-normalized — a
  different lane with different trust; never mixes with this ledger).
- AI inference costs (astrocyte already owns those locally; cloud AI joins the service
  registry when we host inference).

## 11. Build sequence

1. Seat review of this contract (ASTRO domain, ENGRAM producer, CKCRED custody).
2. ENGRAM: instrument engram-worker emission (counted + sampled meters first; DO
   approximation behind a meterVersion tag) — can ride the current worker lane.
3. CKCRED: ledger ingest + storage + read path in the account service.
4. Calibration week on the real account (§6); record error bounds here.
5. ASTRO: rate-card snapshot + `ck spend` cloud lane.
6. Limits config + enforcement at engram's DO (§8 postures).
7. Later doc: invoice fold.
