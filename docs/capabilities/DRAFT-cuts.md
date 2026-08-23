# Capability cuts — central draft (owners correct)

Status: DRAFT. Drafted centrally by SUBC per assembly decision; every cut here
is a proposal awaiting its owner's correction, because a central drafter sees
served surfaces (docs/fleet-surface.md) and *known* consumer joins — the owner
and the consumers know the joins I cannot see. Flags mark where I am guessing.
Cuts reference the grammar in docs/specs/capability-grammar.md.

Notation: **IN** = ops inside the umbrella (the replaceable interface).
**PRIVATE** = module's own surface, outside the umbrella. Tests: R =
replacement ("replaced as one thing"), C = consumer ("no single consumer uses
all of it" — pass means not-too-big), T = transaction ("no atomic write spans
umbrellas").

---

## claustrum → `credentials-provider/v1`

- IN: `credential.get`, `credential.get_many`, `credential.status`,
  `credential.sign`, `credential.public_key`, `credential.report_auth_failure`
- PRIVATE: admin ops (`admin.reactivate`…), enrollment/vault ceremonies, `ck-auth` CLI surface.
- Consumers: broca, prefrontal-core (push keys, GitHub App), plexus, insula, mcp-stdio-adapter (planned).
- Tests: R pass, C pass, T pass. The cleanest cut in the fleet.

## astrocyte → `spend-metering/v1`

- IN: `spend.report`, `budget.verdict`
- PRIVATE: `spend.anomalies`, `spend.anomaly.resolve`, `spend.anomaly.accept` (operator triage).
- Consumers: prefrontal (budget gates), operator surfaces.
- Tests: all pass. Owner already stated this cut; recorded verbatim.

## broca → two umbrellas

**`llm-session-runner/v1`**
- IN: `session.send`, `session.read`, `run.status`, `run.cancel`
- FLAG(broca): `session.import` / `session.retract` — operator/CLI lane or part
  of the runner interface? I lean private (ck-import is an operator tool).
- Consumers: prefrontal (substrate router), alfonso-tui, phone read path.

**`usage-fact-producer/v1`** (shared interface; second implementation: plexus)
- IN: `usage.export` — paged export of immutable insert-once usage records,
  resumable cursor, stable per-record identity.
- FLAG(broca+astro): `cap.install` / `spend.delta` — I cannot tell whether
  these belong to a spend-caps interface or are private plumbing between broca
  and astrocyte. Owners decide.
- Tests: R pass, C pass; T pass (export is read-only).

## plexus → two umbrellas

**`connectors/v1`**
- IN: `connections`, `catalog`, `invoke`, `requests`, `events`
- PRIVATE: all `plexus.admin.*` (tickets, grants, policies, repin — operator ceremonies).
- Consumers: agents (via facade), prefrontal events lane (ALF).

**`usage-fact-producer/v1`** — second implementation, same corpus as broca's
(consumption-record contract already signed with ASTRO).
- Tests: R pass, C pass, T pass.

## magic-context → two umbrellas

**`context-transform/v1`**
- IN: `transform`
- Consumers: broca (opt-in transform module), thalamus legs.

**`context-tools/v1`**
- IN: `ctx_reduce`, `ctx_memory`, `ctx_expand`, `ctx_search`, `ctx_note`
- Consumers: agents via plugins/facade.
- Split rationale (consumer test): transform's consumer is machinery;
  ctx_* tools' consumer is agents — no single consumer uses both surfaces.
- FLAG(MC): does the "simplest compaction module" story need only
  context-transform, or also a minimal ctx_* set? If a minimal harness package
  wants notes/search without the transform lane, the split earns its keep;
  if the two always ship together, merge them and I am wrong.

## fusiform → `model-catalog/v1`

- IN: `catalog.get`, `catalog.history`
- FLAG(FUSI): `catalog.status` (diagnostics — private?) and `catalog.correct`
  (operator ceremony — private?). I lean both private.
- Consumers: broca (registry resolution), astrocyte (price history), ck models.
- Tests: R pass, C pass, T pass.

## insula → `provider-quota/v1`

- IN: `usage.get`
- Consumers: ck quota, prefrontal-routing (quota_status), broca.
- Tests: trivially pass. Smallest umbrella in the fleet.

## engram → `backup-provider/v1`

- IN: `backup.status`, `backup.run`, `backup.publish`, `backup.verify`,
  `restore.run`, `restore.unit`
- PRIVATE: enrollment/recovery/revocation ceremonies (`engram.enroll*`,
  `engram.recover*`, `engram.revoke`, `engram.purge`, `engram.unenroll`),
  retention/GC ops, `engram.roots`, tombstone backfill.
- FLAG(ENGRAM): the `session.*` family (admit, seed_fence, handoff,
  force_take, park_repair, fork_publish, retire…) — per-session backup units
  with at least one external consumer I cannot name confidently. Own umbrella
  (`session-store/v1`?), part of backup-provider, or private?
- FLAG(ENGRAM): enrollment is ceremony-shaped but a *replacement* backup
  module still needs some enrollment story — is the ceremony part of the
  interface or implementation freedom? I lean implementation freedom (the
  card documents "must provide an enrollment path", ops unpinned).
- Tests: R pass, C pass; T needs ENGRAM's confirmation (capture/publish
  atomicity is internal).

## entorhinal → `project-registry/v1`

- IN: `resolve`, `resolve_project_id`, `enumerate`, `register`,
  `assign_workspace`, `upgrade_implicit`, `remove`, `verify`
- PRIVATE: `journal_tail`, `rebuild`, `seed_import` (operator/recovery),
  `projects.session_liveness` (FLAG: ALF-facing — interface or plumbing?).
- Consumers: prefrontal (identity joins), MC (planned resolver), astrocyte (planned).
- Tests: R pass, C pass, T pass.

## prefrontal-core → FIVE umbrellas (the big cut; ALF corrects)

Self-consumed plumbing (delivery sweeps, claim machinery, handoff attempts,
manager provider-liveness, init.*, evidence/council/campaign internals — the
majority of the 247) stays PRIVATE. External consumers I can name: AFT
(status.line, gh.route), phone via fed (asks, transcript, rooms-read, board),
ASTRO (projects.overview, attribution contract), CEREB (admission facts),
condition-runner/wake lane, every agent seat (work/ask/rooms/peer tools via
plugin — FLAG(ALF): plugin lane is self-consumption by my read; if any of it
is meant to be third-party-callable it moves into an umbrella).

**`identity-authority/v1`**
- IN: `agent.resolve`, `agent.resolve_name`, `agent.list`, `agent.peer_roster`,
  `projects.overview`, attribution surface (path-or-triple → ids, as_of).
- PRIVATE: create/rename/dispose/merge/materialize/residence/GitHub-identity
  mint (lifecycle ceremonies).
- The holds-authority case; drain artifact (identity export) rides this card.

**`ask-ledger/v1`**
- IN: `ask.record`, `ask.get`, `ask.persist_answer`, `ask.resolve_user_ask`,
  `ask.list_pending_for_user`, `ask.request_clarification`,
  `ask.attachment_content`
- PRIVATE: the ~20 sweep/claim/consume/renudge ops.

**`work-graph/v1`**
- IN: `work.create`, `work.get`, `work.list`, `work.children`, `work.deps`,
  `work.list_ready`, `work.claim`, `work.set_status`, `work.settle`
- PRIVATE: campaign/dispatch/mint machinery.
- FLAG(ALF): hire.* and manager.* — delegation lifecycle: own umbrella
  (`delegation/v1`) or private? I lean private-for-v1 (consumer is the
  plugin lane itself).

**`rooms/v1`**
- IN: create/invite/rsvp/enter/leave/post/signal/polls/stage/adjourn/ack/
  read/read_for_user/list/board (the surface CKIOS + seats already drive).
- PRIVATE: hint_wait? (FLAG: phone poller uses it — if so, IN).

**`status-line-holder/v1`**
- IN: `status.publish`, `status.line` (the AFT contract, already speced).

- FLAG(ALF): `policy-cascade/v1` (policy.resolve/subscribe + set/delete/park)
  — sixth umbrella or fold into identity-authority? The cascade contract
  already has its own vectors, which argues sixth.
- Tests: R pass per umbrella; C pass (no consumer spans all five); T PASS
  as cut (panel r2): settlement spans work-graph (IN) + hire/evidence
  (PRIVATE) — private-to-umbrella coupling is implementation freedom.
  Recorded as a FORWARD CONSTRAINT on the work-graph card instead: promoting
  hire.*/manager.* delegation to its own umbrella re-triggers the transaction
  test on work.settle, which must then be re-cut or exposed as a compound op.

## prefrontal-routing → `model-routing/v1`

- IN: `route.select`, `route.select_panel`, `route.resolve_policy`,
  `route.set_decision_outcome`, `route.record_usage`
- FLAG(ALF): model_fact/model_upsert/cooldown/quota_status — interface or
  internal state ops shared with prefrontal-core only?
- Consumers: prefrontal-core (the router's only caller today?). FLAG: if
  truly single-consumer-and-sibling, this may be plumbing, not an umbrella —
  the consumer test cuts both ways.

## synapse → two umbrellas

**`embeddings/v1`**: `embed.query`, `embed.batch`, `embed.result`, `rerank.score`
**`local-llm/v1`**: `microllm.oneshot`
- PRIVATE: model lifecycle (load/unload/status/list), probes, aliases, cache,
  admission, approvals (operator + engine management).
- Consumers: MC (embeddings), wake/persona legs (microllm).
- FLAG(SYNAPSE): job.resume — batch-consumer-facing or internal?

## aft → three umbrellas (the Discord-package cut)

**`agent-tools-core/v1`**: `read`, `write`, `edit`, `bash`, `grep`, `glob`
— the "minimal tool provider" a lightweight package replaces first.
**`code-intel/v1`**: `search`, `outline`, `zoom`, `inspect`, `callgraph`,
`ast_search`, `ast_replace`, `refactor`, `import`, `apply_patch`, `conflicts`, `move`, `delete`
**`file-safety/v1`**: `safety` (undo/checkpoint/restore), backup semantics.
- FLAG(AFT): the three-way split is my proposal for exactly the minimal-
  package story (a simple provider serves core without 12 indexes); if the
  undo integration makes core+safety inseparable, merge those two.
- `status` private (diagnostics).
- Tests: R pass (a minimal provider replaces core alone), C pass (harness
  agents use core+intel; simple scripts use core only), T FAIL as drafted
  (panel r2): the undo-record format is written by core ops (write/edit/
  apply_patch/delete/move create backups) and consumed by safety ops, but
  pinned by NEITHER umbrella — a hidden third interface. AFT owns the call,
  before corpora are minted: (a) merge core+safety (nobody replaces half an
  editor), or (b) keep the three-way cut and pin the undo-record contract as
  a join key on the file-safety card with its own corpus vectors. The audit
  must cover code-intel mutators (refactor/import/ast_replace) too.

## cerebellum → two umbrellas

**`computer-actuation/v1`**: `computer.screenshot`, `computer.wait`,
`computer.open_target` + the semantic actuation surface as it ships.
**`browser-actuation/v1`**: `browser.launch/status/close/navigate/snapshot/click`,
`browser.open_grant`.
- Consumers: agents via facade; consent plane rides prefrontal admission facts.
- FLAG(CEREB): grants ops in or out; split confirmed by the plane split already in the module.

## condition-runner → `wake-conditions/v1`

- IN: `runner.register`, `runner.unregister`, `runner.evaluate`,
  `runner.fires_list`, `runner.fires_ack`
- PRIVATE: `runner.health`, `runner.version` (diagnostics).
- **`must_never_reach: ["credentials-provider/v1"]`** — the grammar's first
  deny-edge, machine-checking the keyless-by-design isolation.
- Consumers: prefrontal wake lane.

## thalamus → `ai-gateway/v1`

- IN: `session.resolve`, `session.command.enqueue`
- PRIVATE: `proxy.status` (diagnostics).
- Consumers: subc-mcp (prompt backends).
- FLAG(THALAMUS): the real interface may be the HTTP proxy plane, which the
  catalog does not carry — is the subc surface the umbrella or an appendix?

## callosum → `federation-transport/v1` (deferred cut)

- The device-facing surface rides the reserved `fed:` namespace and
  per-device grants, outside catalog ops; the two catalog ops
  (`fed.effect_status`, `callosum.push_device_read`) are adjuncts.
- FLAG(CALLO): v1 of the grammar excludes fed surfaces; this cut waits for
  that ruling rather than pretending two ops are the interface.

## wernicke → `persona-binding/v1`

- IN: `persona.bind_set`, `persona.bind_clear`, `persona.posture_clear`
- Consumers: prefrontal persona lane.
- FLAG(WERNI): single-consumer-sibling — same plumbing-vs-umbrella question
  as prefrontal-routing.

## subc-mcp, prefrontal-host:* — no umbrellas

Gateway/bridge surfaces: consumers, not providers. The host-op surface is a
session-scoped bridge protocol, not a replaceable fleet capability (v1).

---

## Cross-cutting flags for the correction round

1. **Plumbing-vs-umbrella for single-sibling consumers** (prefrontal-routing,
   wernicke, engram session.*): settled r2 in the DECIDABLE form (the r1
   "either side" wording was ambiguous between at-least-one and both-sides
   readings with opposite verdicts): umbrella IFF provider-side replacement
   keeps the seam AND (a second consumer plausibly exists OR the seam is a
   declared minimal-package boundary). Under this rule prefrontal-routing and
   wernicke resolve to PRIVATE PLUMBING for v1, op lists recorded as draft
   cards so later promotion is cheap.
2. **Ceremonies**: enrollment/admin ops stay outside umbrellas everywhere in
   this draft; the capability card documents "an X path must exist" without
   pinning ops. Confirm per module.
3. **Diagnostics**: status/health/version ops are private everywhere
   (replacements bring their own diagnostics; ck health is the common plane).
4. **Corpus authorship order** (post-correction): claustrum, astrocyte,
   usage-fact-producer first (cleanest, and the third has two live
   implementations to test the corpus against from day one).
