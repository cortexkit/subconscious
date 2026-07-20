# Cloud Usage Accounting: the CloudUsageFact contract

Status: DRAFT r1 (seat review) — custody SUBC, seats ASTRO (domain owner) / ENGRAM (first
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
  averaged away.
- **Late facts are legal** (a worker may fold an hour late); consumers read
  watermark-style (facts through hour H complete when the service's emission watermark
  passes H). Each service publishes its watermark as part of emission.

## 3. Meter classes (how quantities are obtained)

Three classes, declared per resource in the registry:

- **counted**: the serving code increments counters in-band (class A/B ops, egress bytes,
  chunks stored). Exact by construction.
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
- Storage shape (CKCRED to refine in seat review): per-account append-only fact rows +
  a per-account hash chain head (the audit-chain discipline), with hour-bucket upsert
  semantics per §2. D1 rows are fine at these volumes (~thousands of rows/account/month).
- **Emission path**: services emit facts to the account service over an authenticated
  service-to-service call (worker-to-worker; the service identity is the emitter, no
  user token involved — usage accrues whether or not the user is logged in anywhere).
- **Read path**: account-authenticated (the user's own facts) and org-admin (org
  member rollups, later). ASTRO pulls through the same read path.

## 5. Closed registries

Two registries, versioned in this doc (amendment = seat-reviewed doc change):

**Service registry**: `engram` (first), `rendezvous`, `relay`, `wernicke`, `org-daemon`
(reserved, not yet emitting).

**Resource registry v1 (engram)**:

| resource | unit | class | notes |
|---|---|---|---|
| `r2_storage_byte_hours` | byte_hours | sampled | fold of stored bytes over the hour |
| `r2_class_a_ops` | count | counted | writes/lists |
| `r2_class_b_ops` | count | counted | reads |
| `do_compute_gb_s` | gb_seconds | approximated | wall-clock active duration proxy |
| `do_requests` | count | counted | |
| `egress_bytes` | bytes | counted | worker-measured response bytes |

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
