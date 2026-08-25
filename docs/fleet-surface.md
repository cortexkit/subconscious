# Fleet surface: every module and every op exposed over subc

Op lists are machine-extracted from the live daemon (`catalog.list`, daemon 0.7.0,
2026-08-23). One-line descriptions: tool descriptions come verbatim (truncated)
from module manifests; management-surface ops carry no wire descriptions, so
those lines are curated by hand and can drift — regenerate the op *lists* any
time from `clients/subc-client` (unset `SUBC_MODULE_ID`/`SUBC_LAUNCH_NONCE`):

```ts
import { SubcClient } from "./src/index.ts";
const c = await SubcClient.connect({ connectionFile: process.env.HOME + "/.local/share/cortexkit/run/subc-connection.json" });
console.log(JSON.stringify(await c.catalogList(), null, 2));
c.close();
```

`[q]` = query, `[m]` = mutate, `[tool]` = tool-provider tool (agent-facing).
Every module also serves the standard control ops `route.bind`, `route.status`,
`health.check` — not repeated per module.

---

## subc (daemon itself — channel-0, not a module)

The router. State-free by design; these ops are served by the daemon directly.

- `server.describe` [q]: daemon version/commit, uptime, connection counts, route concentration metrics.
- `catalog.list` [q]: all registered modules with manifests (roles, ops, tools, version) — this file's source.
- `catalog.update` [m, module-originated]: in-place provider-role update without disturbing bound routes.
- `route.open` [q/m]: open a route to a module; carries project root, consumer identity, reverse capabilities.
- `route.poll` [q]: route status answered from daemon cache, zero frames to the module.
- `supervisor.list` [q]: supervised module states, restart budgets, last exit.
- `supervisor.restart` [m]: drain (30s default, per-module/per-call override, `--now`) then respawn; acks at initiation.
- `supervisor.reload` / `supervisor.set_enabled` [m]: config reload; enable/disable (re-enable resets restart budget).
- `supervisor.rescan` [m]: reconcile module set against config; `--dry-run` preview; refuses absent config.
- `supervisor.health` [q] / `supervisor.health_probe` [m]: cached health snapshots / fresh one-shot probe.
- `supervisor.provenance` [q]: daemon build/process facts beside each module's separately declared build facts; accepts an optional exact module filter.
- `supervisor.routes` [q]: live route census — who holds routes to what, attested principals, ages, drain reasons.
- `supervisor.stderr_tail` [q]: bounded per-module stderr ring with restart boundaries.
- `supervisor.terminals` [q]: bounded per-module exit history (codes, signals, dispositions); readable even when the module's supervision task is wedged.
- Channel-0 pushes: `route.closing` / `route.closed` (drain lifecycle, terminal flag).

### Provenance reads

`supervisor.provenance` has two source-separated layers. `daemon` contains the
daemon's embedded build identity (`build_git_sha`, `build_lock_digest`) and its own
pid, start time, and running-image evidence. Each module entry contains
`module_declared`, copied from the module's optional HELLO manifest block, and
`daemon_observed`, captured by the supervisor at spawn and read from the running
process. A missing manifest block is `unverifiable`; it is not an empty build claim.

The module declaration may contain `build_git_sha`, `build_lock_digest`,
`wire_crate_version`, and `store_schema_version`. The daemon never promotes those
values into its observed layer. `supervisor.provenance {}` reads the whole box;
`supervisor.provenance { module_id }` reads one module.

On Linux, running-image evidence is SHA-256 over open handles for `/proc/<pid>/exe`
and the captured spawn path. On macOS it is a spawn-inode comparison only, weaker
than a hash by design. Unsupported platforms return a typed unavailable result, not
a placeholder digest. Linux digesting is a cold read on the first observation; a
bounded process-local cache holds 64 file identities and clears when full.

`ck provenance <module>` preserves these source labels in human output. With `--json`
it emits the typed response unchanged. This surface does not implement `origin_delta`,
`buildable_at_head`, deploy, git, or network logic.

## aft — agent tool substrate (21 tools)

Indexed code perception + editing for agent harnesses.

- `status` [tool]: AFT status, index health, cache usage.
- `bash` [tool]: shell execution with output compression, background tasks, PTY.
- `read` / `write` / `edit` / `apply_patch` [tool]: file IO with undo backups and formatter integration.
- `grep` / `glob` [tool]: regex content search / filename matching.
- `search` [tool]: unified indexed search (concepts, identifiers, regex, literals, filenames).
- `outline` / `zoom` [tool]: structure of files/dirs/URLs; full source of named symbols.
- `inspect` [tool]: blocking-fresh codebase health (diagnostics, dead code, TODOs, dupes).
- `callgraph` [tool]: callers/impact/trace ops over a real call graph.
- `conflicts` [tool]: all git merge conflicts, line-numbered.
- `ast_search` / `ast_replace` [tool]: AST-aware pattern search/rewrite.
- `delete` / `move` [tool]: file removal/rename with undo backups.
- `import` [tool]: language-aware import add/remove/organize.
- `refactor` [tool]: symbol move/extract/inline with cross-file import updates.
- `safety` [tool]: undo/history/checkpoint/restore.

## astrocyte — AI spend accounting (5 ops)

- `budget.verdict` [q]: is this spend within budget (the gate other modules ask).
- `spend.report` [q]: spend by provider/model/account over a window.
- `spend.anomalies` [q]: flagged spend anomalies.
- `spend.anomaly.resolve` / `spend.anomaly.accept` [m]: close an anomaly as fixed / accept as expected.

## broca — LLM runner (9 ops)

Durable-WAL model execution substrate.

- `session.send` [m]: start/continue a turn on a session (the main entry).
- `session.import` [m]: import an external session transcript into a broca session.
- `session.retract` [m]: retract session content.
- `run.cancel` [m] / `run.status` [q]: cancel / inspect an in-flight or recorded run.
- `session.read` [q]: page a session transcript (serves `mid` identity + lineage state).
- `usage.export` [q]: token/cost usage records for consumers (astrocyte).
- `cap.install` [m]: install a spend cap.
- `spend.delta` [m]: record a spend delta against a cap.

## callosum — federation (2 ops + fed: namespace)

Device-to-device WAN/LAN transport (rendezvous, Noise IK). Most of its surface
is the `fed:` reserved namespace toward enrolled devices, not catalog ops.

- `fed.effect_status` [q]: settlement status of a federated effect.
- `callosum.push_device_read` [q]: read a device's push registration (seal keys).

## cerebellum — computer/browser actuation (10)

- `computer.screenshot` [tool]: screen capture (consent-gated).
- `computer.wait` [tool]: predicate/duration wait between actions.
- `computer.open_target` [q]: open/lease an app target for observation.
- `browser.launch` / `browser.status` / `browser.close` [tool]: route-owned Chromium session lifecycle.
- `browser.navigate` [tool]: navigate through the enforcing connector.
- `browser.snapshot` [tool]: structured accessibility/DOM snapshot.
- `browser.click` [tool]: click a retained structured reference.
- `browser.open_grant` [q]: browser-plane grant acquisition.

## claustrum — credentials vault (6 ops)

Possession-only bearer-handle read surface; reserved module.

- `credential.get` / `credential.get_many` [q]: resolve credential(s) by handle.
- `credential.status` [q]: vault health, credential states (active/needs_reauth).
- `credential.sign` [q]: sign with a vault-held key (private half never leaves).
- `credential.public_key` [q]: fetch a signing key's public half.
- `credential.report_auth_failure` [m]: report a credential failing upstream (drives needs_reauth).

## condition-runner — keyless wake predicates (7 ops)

Isolated by design: no vault access, no encryption keys.

- `runner.health` [q]: health + last completed evaluation stamp.
- `runner.evaluate` [q]: evaluate a registered condition now.
- `runner.register` / `runner.unregister` [m]: install/remove a watched condition.
- `runner.fires_list` [q] / `runner.fires_ack` [m]: fired conditions; acknowledge consumption.
- `runner.version` [q]: build identity.

## engram — encrypted cloud backup (33 ops)

- `backup.status` [q]: enrollment, staged generations, publish head, capture state.
- `backup.run` [m] / `backup.publish` [m]: capture a generation / publish staged to cloud.
- `backup.verify` [q]: verify backup integrity.
- `restore.run` / `restore.unit` [m]: full / single-unit restore.
- `gc.run` [m] / `gc.unquarantine` [m]: cloud garbage collection; release quarantined objects.
- `engram.enroll` + `enroll_request/challenge/approve/complete` [m]: account/device enrollment ceremony.
- `engram.recover` / `engram.recovery_confirm` [m]: recovery-code account recovery.
- `engram.revoke` / `engram.unenroll` / `engram.purge` [m]: device revocation; leave account; wipe cloud data (fail-closed local confirmation).
- `engram.retire` / `engram.retention_set` / `engram.rescue` [m]: retire generations; retention policy; rescue data.
- `engram.generation_retirement_backfill` [m]: backfill tombstones for historically retired generations.
- `engram.roots` [q]: enrolled store roots.
- `session.*` (admit, stop_eval, seed_fence, status, register, handoff, force_take, park_repair, fork_publish, retire): session-store lifecycle for per-session backup units.

## entorhinal — project registry (12 ops)

- `resolve` / `resolve_project_id` [q]: path or triple → stable project identity.
- `enumerate` [q]: list registered projects (with workspace ids, tags).
- `register` [m] / `remove` [m]: register / remove a project.
- `assign_workspace` [m]: move a project between workspaces.
- `upgrade_implicit` [m]: promote an implicit (path-derived) project to registered.
- `seed_import` [m]: bulk-import registrations.
- `journal_tail` [q] / `verify` [q] / `rebuild` [m]: journal inspection; store verification; rebuild from journal.
- `projects.session_liveness` [m]: session liveness signal for registry consumers.

## fusiform — AI model capability catalog (4 tools)

- `catalog.get` [tool]: the model catalog (existence, capabilities, cost), optionally as-of a past instant.
- `catalog.history` [tool]: every recorded era for one fact with observation windows.
- `catalog.status` [tool]: recent polls, catalog version, row counts.
- `catalog.correct` [tool]: mark a past window of fusiform's own record wrong; reads inside it refuse.

## insula — provider quota (1 op)

- `usage.get` [q]: per-provider quota/usage snapshots (windows, resets, spend pools, error classes) — the `ck quota` source.

## magic-context — context engine (6 tools)

- `transform` [tool]: cache-stable context transform (fold compacted history, apply frozen reductions).
- `ctx_reduce` [tool]: acknowledge tagged reduction requests.
- `ctx_memory` [tool]: durable project memories (write/update/archive/merge/get).
- `ctx_expand` [tool]: recover compacted conversation ranges from historian chunks.
- `ctx_search` [tool]: keyword search over memories, notes, history.
- `ctx_note` [tool]: durable session notes with optional surface conditions.

## plexus — external connectors (5 tools + 9 admin ops)

- `connections` [tool]: list/manage principal-scoped connector connections.
- `catalog` [tool]: discover vendors and reviewed action catalogs.
- `invoke` [tool]: invoke one governed connector action.
- `requests` [tool]: pending ask-first connector requests.
- `events` [tool]: polling subscriptions + durable connector event log (GitHub watch lane).
- `plexus.admin.issue_ticket` / `grant` / `revoke_grant` / `list_grants` [m/q]: binding tickets and grants.
- `plexus.admin.set_policy` / `list_policies` / `remove_policy` [m/q]: per-connection policy rows.
- `plexus.admin.review_action` / `repin_action` [q/m]: action-schema review and repinning.

## prefrontal-core — executive (247 ops; grouped by namespace)

The decision plane: agents, work, asks, rooms, wakes, delegation. Twenty-nine
namespaces; per-op lines for the big ones, one line for the rest.

**agent.* (19)** — registry of fleet agents: `create`, `resolve`, `resolve_name`,
`list`, `peer_roster`, `flip_status`, `deliver` (message delivery), `rename`,
`update_tag`, `set_github_identity`, `github_identity`, `set_sleep`, `wake`,
`update_wake_policy`, `dispose`, `merge`, `materialize`, `flip_residence`,
`rebind_machine`. Identity authority for the fleet (the holds-authority case).

**work.* (27)** — work graph: `create`, `get`, `list`, `children`, `add_dep`,
`deps`, `deps_from`, `child_rollup`, `list_ready`, `is_ready`, `claim`,
`set_status`, `mint_graph` (campaign → graph), `close_epic`,
`cancel_campaign_item`, `stamp_merged`, `settle`, `restamp`, `claim_dispatch`,
`dispatch_intent`, `mark_dispatch_launched`, `link_execution`, `executions`,
`resolve_alias`, `list_scoped`, `show_composite`, `delivery_snapshot`.

**ask.* (27)** — ask ledger: `record`, `get`, `attachment_content`,
`request_clarification`, `append_context`, `get_active_for_task`,
`get_current_for_task`, `list_pending_for_parent`, `list_answered_unconsumed`,
`plan_deliver_sweep`, `list_pending_for_user`,
`list_pending_user_asks_for_proceed`, `list_reclaimable_user_asks`,
`persist_answer`, `clear_answer`, `mark_consumed`, `mark_answer_delivered`,
`clear_consumed`, `mark_auto_proceeded`, `clear_auto_proceeded`,
`reassign_owner`, `claim_dead_owner_answer`, `claim_unowned_answer`,
`resolve_user_ask`, `mark_canceled_for_task`, `mark_canceled_by_request_id`,
`mark_renudged`.

**hire.* (25)** — hiring/delegation ledger (v0 + v1): create/get/list/fire/
archive/evaluations, plus v1 delegation lifecycle (`delegate`,
`begin_delegation`, `settle_delegation`, `get_delegation`, `list_delegations`,
`invite`, `evaluate`).

**rooms.* (22)** — meetings and channels: `create`, `invite`, `rsvp`, `enter`,
`join`, `leave`, `post`, `signal`, `poll_open/vote/close`,
`grant_stage`/`release_stage`, `agenda_advance`, `adjourn`, `ack`, `read`,
`read_for_user`, `hint_wait`, `list`, `list_for_user`, `board`.

**manager.* (33)** — subagent/task orchestration: `launch`, `prompt`, `cancel`,
`finalize`, `discard`, `ingest_event`, `run_tracer`, `gate_eval`, `get_intent`,
`task_state`, `get_task`, `get_task_by_session`, `list_tasks_for_parent`,
`get_result`, `is_subagent_session`, `list_subagent_sessions`,
`wake_parent_for_ask`, `run_sweep`, `run_channel_wake_sweep`, provider
registration/liveness (`register_provider`, `heartbeat_provider`,
`provider_alive`, `set_provider_health`, `nodark_candidate`, `nodark_snooze`,
`force_state`, `status`), and config setters (`set_concurrency`,
`set_worktrees_config`, `set_substrate_config`, `set_attachments_config`,
`set_projects_registry_config`, `set_cadence_config`).

**peer.* (15)** — cross-project messaging: `upsert_peer`, `get_peer`,
`list_peers`, `enqueue_message`, `get_message`, `list_messages`,
`claim_undelivered`, `mark_delivered`, `mark_read`, `list_inbox`,
`count_inbox`, `get_inbox_message`, `discard_message`, `mark_delivery_failed`,
`reset_claim`.

**init.* (12)** — seat bootstrap and head-session handoffs: `get_state`,
`resolve_phase`, `claim`, `save_global`, `hire_head_with_handoff`, handoff
get/list/deliver/attempt/claim/fail ops.

**wake.* (11)** — scheduled wakes: policy set/delete/list, `effective`,
schedule set/delete/list, `fires_list`, `fire_ack`, `create`, `author`.

**board.* (9)** — the user-facing board: `post`, `retire`, `update`, `detail`,
`ask`, `state`, `health`, `register_primary`, `tee_counters`.

**policy.* (6)** — fleet config cascade: `resolve`, `set`, `delete`, `list`,
`park`, `subscribe` (held stream; `revision_bump` pushes).

**athena.* (6)** — consult/campaign engine: `consult`, `campaign_raw`,
`list_consults`, `get_consult`, `spec_status`, `cancel`.

**attachment.* (4)** — chunked upload: `begin`, `append`, `status`, `commit`.

**session.* (4)** — `subscribe`, `enqueue_user_message`, `transcript_page`,
`attachment_thumbnail` (phone read surface).

**evidence.* (3)** — `open_gather`, `close_gather`, `get_gather`.
**council.* (3)** — `prepare_prompt`, `finalize`, `launch_members`.
**persona.* (3)** — `seat_invite`, `seat_dismiss`, `reference_get`.
**github_identity.* (2)** — `mint_begin`, `mint_complete` (agent GitHub App identity).
**knowhow.* (2)** — `search`, `get` (curated skills).
**status.* (2)** — `publish` (module status segments), `line` (composed status line).

Singles: `gh.route` [m] (governed GitHub verb routing), `harness.capabilities`
[q], `llm.oneshot` [m], `gather.context` [m], `sidekick.classify` [q],
`campaign.evidence_query` [q], `observe.recent_runs` [q], `projects.overview`
[q] (fleet idleness source), `comment_clarity.extract` [q].

**Tools (3)**: `ask` (worker → parent blocking question), `record_evidence`,
`record_learning` (campaign evidence/learning capture).

## prefrontal-host:* (one per live host session; 10 ops each)

Per-session OpenCode host bridges, registered dynamically (34 live at capture —
count varies with sessions; each serves the same surface):

- `host.ping`, `host.session_status`, `host.session_list`,
  `host.session_transcript`, `host.session_transcript_page`,
  `host.session_attachment_path`, `host.session_todos`, `host.session_exists`,
  `host.permission_catalog`, `host.execute_effect`.

## prefrontal-routing — model routing (10 ops)

- `route.select` [m] / `route.select_panel` [m]: pick model(s) for a task/panel.
- `route.resolve_policy` [q] / `route.model_fact` [q] / `route.model_upsert` [m]: routing policy and model facts.
- `route.set_decision_outcome` [m] / `route.record_usage` [m]: feed outcomes back into routing.
- `route.record_cooldown` [m] / `route.cooldown_status` [q] / `route.quota_status` [q]: provider cooldowns and quota state.

## subc-mcp — MCP gateway (no routable ops)

Registers for supervision only (control ops); its surface is outbound — it
composes other modules' tools into MCP for external hosts (Claude Code, etc.)
under the tool-surface policy (narrowing merges, default-deny, `ack_only`,
`surface_mode: "search"`), plus `/mcp__subc__status` and `/mcp__subc__wrapup`
prompts.

## synapse — local inference (23 ops)

- `embed.query` / `embed.batch` / `embed.result` [q]: embeddings (sync + async batch).
- `rerank.score` [q]: reranking.
- `microllm.oneshot` [q]: small local LLM one-shot.
- `model.load` / `model.status` / `model.unload` / `models.list` [m/q]: model lifecycle.
- `job.resume` [m]: resume an interrupted batch job.
- `probe.start` / `probe.status` / `probe.report` [m/q]: engine probing/benchmarks.
- `aliases.check_index` [q] / `alias.declare` / `alias.retract` [m]: model alias registry.
- `cache.pin` / `cache.gc` [m]: model cache management.
- `admission.status` [q]: admission/queue state.
- `approvals.*` (migrate_owned_decode, enable, disable, emergency_rollback) [m]: decode-approval controls.

## thalamus — AI proxy gateway (3 ops)

- `proxy.status` [q]: gateway status.
- `session.resolve` [q]: instance token → composite conversation key.
- `session.command.enqueue` [m]: enqueue a session command (e.g. wrapup fold).

## wernicke — chat gateway (3 ops)

- `persona.bind_set` / `persona.bind_clear` [m]: bind/unbind a persona to a chat surface.
- `persona.posture_clear` [m]: clear posture state.

---

*38 of 56 catalog entries at capture were `prefrontal-host:*` session bridges;
the module count proper is 18 supervised + the daemon. `insula`'s op list is
served via its management surface; `callosum`'s device-facing surface rides the
reserved `fed:` namespace and per-device grants in `fed-profile.json`, not the
catalog.*
