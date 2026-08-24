# Cloudflare OS v2 — survey

Surveyed 2026-08-24 from the read-only `cloudflare-os` checkout at commit
`1ef6020a42fbabb6d27dd1063db3a075ba95c974`. The survey brief records the v2
release as 2026-08-21; the repository describes it as an August 2026 early-
access release, a complete v2 rewrite, and explicitly warns that it still has
rough edges (`README.md:44-48`).

The short verdict: Cloudflare OS is the strongest company-fleet analogue we
have seen, but not because it has discovered a stronger daemon. It has built a
cloud application kernel around three excellent product ideas: user-authored
sandboxed apps, attenuated service capabilities, and a first-class action log.
Its GitHub Gatekeeper's optimistic simulation is genuinely novel. Its reject
path is also incomplete in exactly the dangerous place: simulated downstream
work is not unwound, a requested restart is dropped, and dependent Workshop
action cards can diverge from the Gatekeeper's own action state.

Evidence labels used below:

- **CODE-READ** means the behavior is present in executable source at this
  checkout.
- **README CLAIM** means the repository claims it, but code does not fully prove
  the boundary.
- **PLANNED** means it appears in `plans/`; it is not treated as shipped unless
  current code independently confirms it.
- **UNVERIFIED** means the available source did not establish the claim.

CortexKit descriptions in the comparison sections come from the survey brief;
this pass re-audited Cloudflare OS, not our own fleet.

## 1. System map

### Planes and packages

| Plane | Shipped pieces | What it does |
|---|---|---|
| Public origin | `router` | Scans `GATEKEEPER_*` bindings, forwards `/gatekeeper/<name>/*` to an integration Worker, `/api/*` to the backend, and otherwise serves the SPA (`packages/router/src/index.ts:1-9`, `packages/router/src/index.ts:24-59`). |
| Shell | `workshop-frontend` | Pure client-side React/Vite SPA speaking Cap'n Web over a persistent WebSocket (`AGENTS.md:8-20`). Its production assets are built before Worker bundling and carried by the router (`scripts/release/build-release.ts:76-103`). |
| Kernel | `workshop-backend` + `workshop-shared` | Backend Worker plus public RPC/types. The repository itself designates this as the high-scrutiny kernel (`AGENTS.md:14-20`; `REVIEW.md:10-23`). |
| Integration/capability plane | `gatekeeper-*` | Separate Workers for Cloudflare, Confluence, Context, Email, GitHub, Google, Home Assistant, Linear, MCP, MCP Portal, Notion, Scheduler, Slack, Spotify, Supabase, and ZoomInfo. Release discovery classifies every top-level package with `wrangler.jsonc`, with `gatekeeper-*` as Gatekeeper units (`scripts/release/manifest-lib.ts:290-320`). |
| Shared libraries | `backend-utils`, `configurator-ui`, `error-reporting`, `mcp-shared`, `typed-storage`, `workshop-shared` | RPC contracts, storage facade, logging/error helpers, configurator UI types, and MCP implementation. `mcp-shared` is deliberately a library shared by two Gatekeeper Workers, not a Worker itself (`AGENTS.md:21-29`). |
| User applications | Gadgets and Blueprints | A Gadget is code loaded as a Dynamic Worker Durable Object facet; a Blueprint packages reusable app code. The README's OS analogy calls these processes and executables (`README.md:93-108`). |

The release unit is therefore not one monolith. The generator dry-runs Wrangler
for every deployable package and stores each bundle in a release manifest
(`scripts/release/build-release.ts:110-135`). Installed Gatekeepers are supplied
to the backend and router as expanded service bindings
(`scripts/release/manifest-lib.ts:400-423`). Context and Scheduler are special:
they are preinstalled singleton/ambient Gatekeepers, while Email ships but is
not customer-installable (`scripts/release/manifest-lib.ts:250-274`).

### Runtime path

```text
browser SPA
  -> router Worker
     -> workshop-backend Worker
        -> UserDurableObject (identity, accounts, library)
        -> OverseerDurableObject (one workspace)
           -> Gadget Dynamic Worker facet(s)
           -> Gatekeeper facet(s)
              -> separately deployed Gatekeeper Worker/account capability
                 -> provider API
```

The README claim that every workspace is a Durable Object and every Gadget is a
Dynamic Worker facet is code-confirmed by the Loader/facet path: the Overseer
loads `server.js`, injects an environment, disables global outbound, and mounts
the exported `Gadget` class under a per-gadget facet name
(`README.md:114-118`; `packages/workshop-backend/src/overseer.ts:3870-3939`,
`packages/workshop-backend/src/overseer.ts:3942-3994`). Gatekeepers are also
installed as workspace facets, but their implementation classes come from
separate bound Workers (`AGENTS.md:23-29`).

### Where state lives

| State | Location and boundary |
|---|---|
| User/account state | SQLite-backed `UserDurableObject`: profiles, sessions, connected-account capabilities, gadget/library indexes, billing, and quotas. The backend Wrangler migration declares the user, workspace, and admin DO classes as SQLite classes (`packages/workshop-backend/wrangler.jsonc:45-63`). |
| Workspace state | One `OverseerDurableObject` typed store per workspace: gadget/gatekeeper registries, actions, chats, compaction checkpoints, active turns, collaborators, attachments, code-change streams, and the workspace git object database (`packages/workshop-backend/src/overseer.ts:945-1015`, `packages/workshop-backend/src/overseer.ts:1017-1150`, `packages/workshop-backend/src/overseer.ts:1161-1249`). |
| Committed Gadget code | Real SHA-1/zlib git loose objects in the Overseer DO; `GadgetRecord.commitId`, blueprint records, and chat pins are the refs. There is no `HEAD`, branch, tag, or ref table (`packages/workshop-backend/src/git-store.ts:1-36`, `packages/workshop-backend/src/git-store.ts:56-73`). |
| Uncommitted chat code | A revisioned operational-transform change stream. The current plan records the shipped design as CodeMirror `ChangeSet`/Jupiter OT with the Overseer as authoritative sequencer (`plans/git-storage.md:1101-1117`, `plans/git-storage.md:1150-1194`); current storage has durable `chatChanges`, client dedupe, and generation-boundary rows (`packages/workshop-backend/src/overseer.ts:1161-1182`). |
| Gadget private data | Each loaded `Gadget` is itself a Durable Object class and receives normal DO KV/SQLite APIs; the platform prompt promises private storage (`packages/workshop-backend/src/agent.ts:545-563`). The schema is user-authored and therefore not knowable centrally. |
| Gatekeeper state | Provider-specific account/resource DOs. GitHub stores its OAuth grant in its account DO and staged/pending/provisional actions in the resource DO (`packages/gatekeeper-github/src/github.ts:1045-1113`, `packages/gatekeeper-github/src/github.ts:1938-2013`). Scheduler stores an account's schedules and persistent callback capability in `ScheduleDriver` (`packages/gatekeeper-scheduler/src/schedule-driver.ts:36-70`, `packages/gatekeeper-scheduler/src/schedule-driver.ts:82-145`). |
| Deployment/global data | Blueprint metadata and admin snapshots in Workers KV, blueprint content in R2, avatars in KV (`packages/workshop-backend/src/env.d.ts:27-36`; `packages/workshop-backend/src/blueprint-archive.ts:98-140`). |

Local `pnpm run-local` runs the whole stack under Wrangler/workerd, but the
README explicitly says this path is not for production; the alternate supported
path deploys to a Cloudflare account (`README.md:21-32`). The README also says it
can run entirely on self-hosted workerd (`README.md:114-120`), but no production
self-host deployment procedure is present here: **UNVERIFIED as an operational
claim**, beyond the code's workerd compatibility.

## 2. Agent loop

### Turn lifecycle

1. **Durable start.** Starting a turn registers a running agent and writes an
   `activeAgents` record before launching the async turn, so a server restart can
   discover and resume it (`packages/workshop-backend/src/overseer.ts:5591-5613`;
   the collection is declared at `packages/workshop-backend/src/overseer.ts:1114-1118`).
2. **Admission and model resolution.** The Overseer reconciles orphaned
   provisional Gadgets, materializes pre-existing edits, enforces the optional
   free-tier/BYOK decision, computes session affinity, and resolves a model
   handle (`packages/workshop-backend/src/overseer.ts:5646-5696`).
3. **Replay.** `runAgent()` reconstructs the chat-local binding map, committed
   Gadget trees at their git pins/heads, durable OT changes, tool results, agent
   callback values, and provider-native assistant snapshots
   (`packages/workshop-backend/src/agent.ts:969-1048`,
   `packages/workshop-backend/src/agent.ts:1984-2058`). Client-visible chat rows
   and model-facing assistant snapshots are intentionally separate so encrypted
   reasoning signatures and true provider/model provenance survive restarts
   without being sent to clients (`packages/workshop-backend/src/agent.ts:229-275`;
   `packages/workshop-backend/src/overseer.ts:1197-1206`).
4. **Prompt and tools.** The loop builds a byte-stable static system-prompt slot
   plus a workspace-specific slot, then exposes file/Gadget, connection,
   `executeCode`, and callback tools. The system explicitly tells the model that
   its workspace contains named Gadgets and external-resource bindings
   (`packages/workshop-backend/src/agent.ts:535-574`,
   `packages/workshop-backend/src/agent.ts:2089-2097`).
5. **Run.** It calls pi-agent-core's low-level `runAgentLoopContinue` with
   sequential tool execution and a 30-step hard cap. It stops after cancellation,
   a connection request, an action requiring a decision, or completion of all
   callback obligations (`packages/workshop-backend/src/agent.ts:3128-3156`).
6. **Persist at barriers.** Completed model steps and tool calls are appended to
   the chat log; code edits are durable streaming rows and are flushed into a
   `changes` message. The final flush is unconditional—even cancellation keeps
   completed edits rather than reverting the turn
   (`packages/workshop-backend/src/agent.ts:3157-3179`).
7. **Callback completion and teardown.** Callback-initiated turns get one nudge
   if callbacks remain, then reject stalled callbacks. Finally, active-agent
   metadata is cleared atomically with the running registry; queued callbacks
   may immediately start another turn (`packages/workshop-backend/src/overseer.ts:5700-5766`,
   `packages/workshop-backend/src/overseer.ts:5812-5859`).

This is a single-user-facing chat agent architecture with callable spawned
agents, not a peer fleet. `AgentSpawnerGatekeeper` can create another chat agent
with a configured resource environment and return either fire-and-forget or a
callable capability (`packages/workshop-backend/src/overseer.ts:11121-11148`,
`packages/workshop-backend/src/overseer.ts:11172-11195`). There is no durable
peer graph, delegation protocol, independent supervisor, or multi-binary fate
separation in that mechanism.

### Model dispatch

`ModelHandle` closes over provider transport, credentials, AI Gateway metadata,
session affinity, and response metadata; callers see one stream function
(`packages/workshop-backend/src/ai-models.ts:58-104`). The concrete API map is:
Anthropic Messages, OpenAI Responses, Google Generative AI, and
OpenAI-compatible completions for Workers AI and Ollama
(`packages/workshop-backend/src/ai-models.ts:117-151`,
`packages/workshop-backend/src/ai-models.ts:507-645`).

Routing has three modes: connected-user AI Gateway/BYOK first, deployment AI
Gateway second, and direct provider credentials otherwise
(`packages/workshop-backend/src/ai-models.ts:350-375`). Gateway calls retain each
provider's native API rather than using the cross-provider compatibility layer,
so Anthropic cache/thinking and OpenAI encrypted reasoning survive
(`packages/workshop-backend/src/ai-models.ts:167-248`). One-shot title, binding,
compaction, and model-binding calls use `completeText()` with thinking disabled;
provider error-shaped final messages are converted back to exceptions
(`packages/workshop-backend/src/ai-invoke.ts:15-28`,
`packages/workshop-backend/src/ai-invoke.ts:45-80`).

### History, git, and OT

The latest storage design is not “chat messages in git.” Canonical messages
remain ordered typed-storage rows keyed by `chatId.sequence`; model snapshots,
callbacks, compaction state, and OT edits occupy separate collections
(`packages/workshop-backend/src/overseer.ts:1093-1206`). Git stores Gadget source
history. OT stores the uncommitted branch-like delta, and accept writes new git
commits. The object store is real git plumbing with nested trees and content
addressing (`packages/workshop-backend/src/git-store.ts:210-262`). Diverged
mainline/chat trees use an explicit three-way merge with conflict markers rather
than CRDT merging (`packages/workshop-backend/src/git-store.ts:443-474`).

This is a strong design: source history is content-sized and interoperable,
while high-frequency edits are sequenced by the DO. Its liabilities are also
explicit: loose objects only, no GC, no packfiles, and a roughly 2 MiB
single-object assumption (`packages/workshop-backend/src/git-store.ts:23-36`).

### Compaction: theirs versus Magic Context

Cloudflare OS compacts automatically at 85% of the model's input budget and
chooses a retained tail around 30%; the remaining prefix is summarized by the
same model under a prompt-injection-resistant handoff instruction
(`packages/workshop-backend/src/agent-compaction.ts:8-21`,
`packages/workshop-backend/src/agent-compaction.ts:39-60`). It keeps canonical
history for UI paging, but replay begins at the checkpoint boundary
(`packages/workshop-backend/src/agent-compaction.ts:8-10`). The checkpoint does
more than hold prose: it folds bindings, callback-name allocation, git pins,
epoch, next change ID, and composed still-proposed code changes into structured
state (`packages/workshop-backend/src/agent-compaction.ts:368-461`). Pending
connection requests and retained reverts constrain the cut so live state does
not disappear behind a summary (`packages/workshop-backend/src/agent-compaction.ts:200-217`,
`packages/workshop-backend/src/agent-compaction.ts:346-365`). `/compact` uses the
same path and ends without prompting the coding agent
(`packages/workshop-backend/src/agent.ts:2240-2323`).

| Property | Cloudflare OS | CortexKit Magic Context transform lane |
|---|---|---|
| Unit of reduction | Whole old conversational prefix, converted into one semantic handoff. | Spent/tagged model context and large tool material transformed or released at the harness boundary. |
| Loss mode | Model-generated and therefore semantically lossy; mitigated by canonical history plus structured checkpoint state. | More mechanical and source-preserving for the content it transforms; it does not pretend to semantically replace every old turn. |
| Autonomy | Automatic by token threshold, plus `/compact`. | Harness/transform-lane policy decides what becomes discardable or transformed. |
| Application coupling | Deeply coupled to chat bindings, pins, epochs, provisional changes, and connection state. | Fleet infrastructure is decoupled from any one agent application's storage schema. |
| Honest edge | Better at producing a coherent “continue the same task” narrative. | Better at auditability and avoiding an LLM summary becoming hidden durable truth. |

The two are complements, not substitutes. The Cloudflare approach is better
when a long conversation genuinely needs semantic condensation; ours is safer
for high-volume tool evidence. The concrete borrow is their split checkpoint:
semantic handoff **plus independently reconstructed machine state**, never a
summary alone.

## 3. Gatekeepers deep dive — the headline

### Contract shape

A Gatekeeper separates account authority from resource authority. A connected
account returns a resource-specific Durable Object class; the backend mints it
through the one admin-policy chokepoint, and the workspace installs that class
as a facet (`packages/workshop-backend/src/user.ts:1666-1690`;
`REVIEW.md:25-41`). The facet receives an `ApprovalQueue` capability with only
three operations: authorize an observation, submit an action, or bind a hook
(`packages/workshop-backend/src/overseer.ts:11069-11103`).

That is the real security idea: provider credentials stay in the account
Worker, application/agent code receives only the Cap'n Web session object, and
the session itself can ask the kernel to record/approve effects. The general
Gatekeeper contract requires all actions—even potentially auto-approved ones—to
be submitted (`packages/workshop-shared/src/gatekeeper.ts:800-829`).

### GitHub worked example: OAuth and narrowing

**OAuth.** GitHub requests the broad classic `repo` scope plus identity/email;
sign-in-only grants use `read:user user:email`
(`packages/gatekeeper-github/src/github.ts:266-300`). The flow uses two bounded
nonce stages, redirects to GitHub with a callback and requested scopes, exchanges
the code, and stores token/scopes in `UserAccount` DO storage
(`packages/gatekeeper-github/src/github.ts:942-999`,
`packages/gatekeeper-github/src/github.ts:1045-1138`). Reconnect repeats the
nonce flow; disconnect best-effort revokes the provider grant and then deletes
local account storage (`packages/gatekeeper-github/src/github.ts:1165-1187`,
`packages/gatekeeper-github/src/github.ts:1304-1315`). Credentials therefore
are not cryptographically attenuated per repo: the **wrapper is the enforcement
boundary**, not the OAuth token.

**Resource minting.** A selected GitHub URL is parsed into immutable facet props
`{userObjectId, owner, repo, resourceKind, issueNumber?}`. Repo, issue, and pull
URLs produce distinct resource classes/types
(`packages/gatekeeper-github/src/github.ts:1190-1268`). `startSession()` then
returns only `GitHubRepo`, one `GitHubIssue`, or one `GitHubPullRequest` based on
those props (`packages/gatekeeper-github/src/github.ts:3181-3213`,
`packages/gatekeeper-github/src/github.ts:3242-3251`). The Cap'n Web surface is
purpose-built rather than a REST passthrough: repo sessions expose metadata,
create/open/list/search for issues and PRs; issue capabilities expose details,
title/body/label/state/comment operations; PR capabilities add diff, review,
thread-reply, and merge methods (`packages/gatekeeper-github/src/types.d.ts:28-78`).
Every actual provider call uses the facet's `owner` and `repo`; the agent cannot
supply another account or repo. Search adds a repo qualifier and validates every
returned URL before it enters the cursor—a defense against provider-side scope failure
(`packages/gatekeeper-github/src/github.ts:2551-2604`). Collaborator admission
also checks repo access with the collaborator's own account capability
(`packages/gatekeeper-github/src/github.ts:3775-3796`).

This is narrower than a raw GitHub MCP server, but note the trust placement: a
bug in any GitHub session method still holds an account-wide `repo` token. The
narrow capability is a software invariant, not an attenuated provider grant.

### End-to-end action and audit path

1. A method such as `setTitle()` first records an observation when it must read
   current state for revert data, prepares a Gatekeeper-local action, and calls
   `submitActionForApproval()` with human text and revert metadata
   (`packages/gatekeeper-github/src/github.ts:3918-3971`).
2. The GitHub resource DO writes the action as `staged`, submits its local ID to
   the Workshop queue, then marks it `pending`; submission failure deletes the
   staged record (`packages/gatekeeper-github/src/github.ts:3224-3240`).
3. The Workshop allocates a separate workspace-wide action-log ID, records
   caller/resource/description/state, and associates it with the chat
   (`packages/workshop-backend/src/overseer.ts:4577-4619`). Observations are
   likewise written immediately as approved audit rows
   (`packages/workshop-backend/src/overseer.ts:4357-4398`).
4. Manual approval calls the Gatekeeper's `applyAction()`, then transitions the
   Workshop row with resolver identity and auto/manual attribution through one
   chokepoint (`packages/workshop-backend/src/overseer.ts:4183-4201`,
   `packages/workshop-backend/src/overseer.ts:9376-9405`).
5. Automatic approval exists only when **both** the action advertises
   `autoApprovable` and the user has enabled that action kind. The drain applies
   in ID order and stops at the first human gate or failure
   (`packages/workshop-backend/src/auto-approval.ts:53-95`). GitHub advertises no
   auto-approvable action kinds at this snapshot, so its actions remain manual
   (`packages/gatekeeper-github/src/github.ts:3220-3222`).

### How optimistic approval actually works

The decisive behavior is not in the model prompt. It is in the Gatekeeper's
stored virtual world:

```text
3229: this.#stageAction(action);
3231: await approvalQueue.submitAction(action.approvalId, description);
3238: this.#markActionPending(action);

3823: async createIssue(options): Promise<GitHubIssue> {
3824:   const action = await this.#gatekeeper.prepareCreateIssue(options);
3825:   await this.#gatekeeper.submitActionForApproval(...);
3830:   return new GitHubIssueImpl(..., action.provisionalId, "issue");
3831: }
```

Source: `packages/gatekeeper-github/src/github.ts:3224-3240` and
`packages/gatekeeper-github/src/github.ts:3823-3831`.

There is no `awaitDecision` flag on those descriptions. The agent's
`executeCode` call returns normally and the loop can keep working. A creation
allocates IDs like `~1`; provisional comments/reviews use names such as
`~comment1` and `~review1`
(`packages/gatekeeper-github/src/github.ts:1405-1426`). `createIssue()` returns a
real Cap'n Web issue capability backed by that logical ID, not merely the word
“success.” Reading it fabricates a complete issue with:

- repo, URL, title, body, labels, assignees, and current viewer as author;
- `state: "open"`, timestamps equal to submission time, and zero comments;
- the provisional ID wherever GitHub would normally return a number
  (`packages/gatekeeper-github/src/github.ts:2331-2346`).

A provisional pull request also compares the real branches to synthesize
base/head SHAs and statistics; if that read fails it keeps empty/zero fields and
logs a warning (`packages/gatekeeper-github/src/github.ts:2349-2408`). Pending
mutations overlay reads in submission order: title/body/state/labels are
rewritten, comment count increments, and a pending merge appears closed and
merged (`packages/gatekeeper-github/src/github.ts:2411-2476`). Pending comments
and reviews are injected into discussion cursors, including provisional URLs and
IDs (`packages/gatekeeper-github/src/github.ts:2818-2866`). This is a coherent
shadow model, not a canned tool result.

**Approval reconciliation.** Approving a creation performs the GitHub API call,
stores `provisional -> real` identity, and retires the local action
(`packages/gatekeeper-github/src/github.ts:3254-3271`). Later dependent actions
resolve the provisional target before applying; body/comment references are
rewritten to known real IDs (`packages/gatekeeper-github/src/github.ts:3326-3351`).
That is why the agent can stack “create issue, edit it, comment on it” before
human approval.

### Rejection: what is and is not reconciled

Rejecting a provisional root marks all Gatekeeper-local dependent actions
rejected, deletes the provisional mapping, and returns `{restart: true}`
(`packages/gatekeeper-github/src/github.ts:3481-3503`). Dependency discovery is
explicit over the target provisional ID; reply chains are walked transitively
(`packages/gatekeeper-github/src/github.ts:1956-2005`). No external effect needs
compensation because none was performed.

But three other layers do **not** reconcile:

1. **The requested restart is dropped.** The shared contract says the Overseer
   will restart a Gadget when `rejectAction()` returns `restart: true`
   (`packages/workshop-shared/src/gatekeeper.ts:820-829`). The current Overseer
   awaits the call but ignores its return value, then only marks the selected
   Workshop action rejected (`packages/workshop-backend/src/overseer.ts:9526-9555`).
2. **Dependent Workshop action rows are not closed.** GitHub rejects dependent
   records only inside its own resource DO. The Overseer updates only the one
   outer row named by the UI (`packages/gatekeeper-github/src/github.ts:1975-2005`;
   `packages/workshop-backend/src/overseer.ts:9540-9551`). The dependent cards can
   therefore remain `pending` in the Workshop log even though a later approval
   reaches a Gatekeeper record already marked rejected and fails.
3. **Downstream agent work is not undone or replanned.** Action chat records are
   deliberately not surfaced during agent replay
   (`packages/workshop-backend/src/agent.ts:1965-1969`). A denial leaves an
   `awaitDecision` turn ended rather than resuming it with a rejection message
   (`packages/workshop-backend/src/overseer.ts:9485-9523`), and GitHub actions do
   not set `awaitDecision` at all. Code edits, created Gadgets, prose conclusions,
   or later queued actions built on the simulated issue remain in the chat's
   durable history. There is no automatic revert or synthetic “the premise was
   rejected” turn.

This is the most important finding in the survey. Optimistic simulation is
safe only if rejection invalidates every derived artifact or wakes the agent to
repair it. At HEAD, it invalidates the GitHub shadow model but not the rest of
the workspace.

### README claim/code ledger

| README claim | Code finding | Verdict |
|---|---|---|
| Gatekeepers provide a clean API, OAuth, narrow resource access, action logging, and human approval (`README.md:65-74`). | GitHub implements all five through resource facets and the Workshop audit queue (`packages/gatekeeper-github/src/github.ts:1190-1268`; `packages/workshop-backend/src/overseer.ts:4357-4398`, `packages/workshop-backend/src/overseer.ts:4577-4619`). | **Confirmed**, with the caveat that narrow GitHub scope is wrapper-enforced over a broad token. |
| “When” an action requires approval, the Gatekeeper simulates it and the agent proceeds (`README.md:75-77`). | GitHub does. MCP explicitly says it simulates nothing, sets `awaitDecision: true`, returns `status: "pending"`, and tells `executeCode` to return (`packages/mcp-shared/src/session.ts:185-238`). | **Disagrees as a universal claim.** Simulation is Gatekeeper-specific. |
| Users may approve/reject actions “in bulk, or one-by-one” (`README.md:77`). | Public RPC has singular `approveAction(id)` / `rejectAction(id)` only; frontend resolution invokes one ID at a time (`packages/workshop-shared/src/api.ts:1765-1781`; `packages/workshop-frontend/src/useResolveAction.ts:8-33`). | **Bulk path not found; contradicted by the shipped surface.** |
| Simulation avoids synchronous blocking (`README.md:75-77`). | GitHub proceeds optimistically, but non-simulating MCP deliberately suspends a turn until approval; denial leaves it ended (`packages/mcp-shared/src/session.ts:212-238`; `packages/workshop-backend/src/overseer.ts:9485-9523`). | **Partially true, not architectural.** |
| Rejecting a simulated action can request a restart. | Interface promises restart handling, GitHub returns it, Overseer ignores it (`packages/workshop-shared/src/gatekeeper.ts:820-829`; `packages/gatekeeper-github/src/github.ts:3481-3494`; `packages/workshop-backend/src/overseer.ts:9540-9551`). | **Code/contract defect.** |
| Gatekeepers are independently maintained services in the future (`README.md:79`). | They are separate Workers today, but independent deployment/versioning details are explicitly future work. | **Separate Workers confirmed; independent lifecycle PLANNED/UNVERIFIED.** |

## 4. Gadget sandbox model

### Server boundary

A Gadget's `server.js` is loaded into a Dynamic Worker with:

- only JavaScript modules from that Gadget's committed or chat-preview tree;
- `mainModule: "server.js"` and the exported `Gadget` Durable Object class;
- an `env` consisting of `GADGET` self plus only that Gadget's named binding
  edges; and
- `globalOutbound: null`, which removes ordinary global network/subrequest
  authority (`packages/workshop-backend/src/overseer.ts:3870-3939`,
  `packages/workshop-backend/src/overseer.ts:2656-2669`).

Binding visibility is itself scoped: permanent edges appear in mainline; a
provisional edge appears only in its owning chat preview and is invisible to
other chats (`packages/workshop-backend/src/overseer.ts:2163-2175`). External
resources arrive as loopback Cap'n Web capabilities, so Gadget code does not
receive provider tokens or a generic backend API. By construction it cannot
reach an unbound Gatekeeper through `env`; creating the capability is a
user/backend operation, not a Gadget operation
(`packages/workshop-shared/src/api.ts:1745-1751`).

This is the enforcement point behind the useful part of the marketing claim:
**server-side Gadget code has no ambient network and only explicitly injected
object capabilities.** Its private DO storage remains reachable because that is
the app's own state (`packages/workshop-backend/src/agent.ts:547-575`).

### Browser boundary

`client.js` runs in a `srcDoc` iframe with an opaque origin. CSP blocks default,
frame, object, form, and `connect-src`; scripts and UI assets must be inline/data
(`packages/workshop-frontend/src/GadgetUI.tsx:105-116`). The iframe gets only a
Cap'n Web channel to its Gadget server. The parent accepts the handshake only
from the exact iframe window and origin `"null"`
(`packages/workshop-frontend/src/GadgetUI.tsx:326-388`). The sandbox flags permit
scripts and user-activated popups, including escape from the sandbox
(`packages/workshop-frontend/src/GadgetUI.tsx:491-505`). Programmatic
`window.open` is monkey-patched off, while `_blank` links remain supported with
`noopener` (`packages/workshop-frontend/src/GadgetUI.tsx:51-84`).

### Actual boundary versus “impossible to leak”

**README CLAIM:** “It's impossible for the slide deck app to have a security bug
that leaks your slides”; the sandbox controls all access
(`README.md:52-60`).

**CODE-READ boundary:** much narrower. Ordinary server `fetch` is removed and
ordinary browser fetch/XHR/WebSocket are blocked. But:

- the repository explicitly acknowledges that CSP/request interception does
  not cover WebRTC/STUN and says the same gap exists in normal Gadget iframes
  (`packages/workshop-backend/src/browser-export.ts:35-43`);
- a malicious client can construct a user-clicked external `_blank` URL whose
  query/path encodes data, because popup escape is intentionally allowed
  (`packages/workshop-frontend/src/GadgetUI.tsx:51-84`,
  `packages/workshop-frontend/src/GadgetUI.tsx:491-505`); and
- an injected Gatekeeper capability can intentionally disclose or mutate its
  scoped external resource—the sandbox mediates that authority, it does not
  make data flow impossible (`packages/workshop-backend/src/overseer.ts:2656-2669`).

Therefore the absolute README statement is **false at the actual browser
boundary**. The defensible claim is: “server-side Gadget code has no ambient
outbound network; browser UI has strong CSP/opaque-origin containment with a
known WebRTC gap and an explicit user-gesture navigation channel; all other
external authority is injected as named Gatekeeper capabilities.”

## 5. `typed-storage` and git-backed durable state

`typed-storage` is a thin typed schema/index facade over synchronous Durable
Object KV, not a database server. It provides typed collections, singletons,
unique/non-unique indexes, ordered list ranges, subscriptions, and a
`transaction()` that delegates to `storage.transactionSync()`
(`packages/typed-storage/src/index.ts:10-120`,
`packages/typed-storage/src/index.ts:149-181`,
`packages/typed-storage/src/index.ts:600-657`). Secondary indexes are maintained
as subscribers in the same synchronous transaction as the primary record
(`packages/typed-storage/src/index.ts:320-369`,
`packages/typed-storage/src/index.ts:373-454`). It is intentionally modest: its
TODO still names schema versions and migrations as future work
(`packages/typed-storage/src/index.ts:1-5`). Migrations therefore live in each
owning DO's application code/version singleton.

The git storage plan's central decisions are now code-confirmed:

- real SHA-1/zlib loose objects through isomorphic-git plumbing;
- objects only, no refs, one object DB per workspace;
- Gadget/Blueprint records and chat pins serve as refs;
- accepted histories deduplicate by content; and
- explicit three-way merge moves conflicts into the chat instead of CRDT-merging
  divergent bases (`plans/git-storage.md:14-53`;
  `packages/workshop-backend/src/git-store.ts:1-36`).

The current implementation maps only `.git/objects/xx/<38 hex>` virtual paths
onto a typed-storage collection and rejects unsupported filesystem operations
(`packages/workshop-backend/src/git-store.ts:56-120`). `GitStore` reads/writes
nested file trees and commits by OID (`packages/workshop-backend/src/git-store.ts:210-262`).
This is an unusually good durable-state choice for agent-authored code: the
accepted state is portable and content-addressed, while the active collaboration
stream remains cheap and explicit.

`plans/git-storage.md` must be read chronologically. Part 1 proposed git plus
chat-local Yjs; Part 2 changed pinning; Part 3 replaced Yjs with CodeMirror OT
before deployment to avoid shipping a third live format
(`plans/git-storage.md:583-599`, `plans/git-storage.md:1101-1117`). Treating the
opening Yjs design as current would be wrong. The shipped schema's `version: 2`
comment confirms committed code is git-backed and the old Yjs `code`/`snapshots`
collections are dead migration data (`packages/workshop-backend/src/overseer.ts:951-970`,
`packages/workshop-backend/src/overseer.ts:1017-1042`).

Relative to CortexKit: Cloudflare's workspace DO is a rich aggregate containing
many domains, while our per-module SQLite stores make ownership and independent
backup/recovery clearer. They lead on code-history semantics; we lead on store
fault-domain separation and explicit backup ownership.

## 6. Scheduling (`gatekeeper-scheduler`)

Cloudflare OS places scheduling in an **ambient Gatekeeper**, not in the
Workshop kernel. This is the same architectural instinct as putting scheduling
in our executive module rather than the local daemon core.

The agent-facing `ScheduleSession` offers elapsed intervals, timezone-aware
calendar recurrences, one-shots, and workspace-local listing. Registration does
not activate execution; it binds a disabled Workshop hook and returns its ID
(`packages/gatekeeper-scheduler/src/scheduler.ts:89-181`). Enabling the hook
passes immutable account/workspace/schedule metadata and a persistent callback
capability into an account-scoped `ScheduleDriver`
(`packages/gatekeeper-scheduler/src/scheduler.ts:185-224`).

One SQLite-backed `ScheduleDriver` Durable Object per account stores metadata
and callback capabilities under separate keys, uses one DO alarm, processes 20
due schedules per pass with four deliveries, and persists/fences state before
crossing RPC boundaries (`packages/gatekeeper-scheduler/src/schedule-driver.ts:29-70`,
`packages/gatekeeper-scheduler/src/schedule-driver.ts:82-145`,
`packages/gatekeeper-scheduler/src/schedule-driver.ts:237-299`). A logical run
has a stable `runId`; eight attempts use exponential delay from one minute to
one hour, after which the schedule becomes `dead`
(`packages/gatekeeper-scheduler/src/driver-state.ts:5-80`,
`packages/gatekeeper-scheduler/src/driver-state.ts:165-214`). Admission is
rechecked before each delivery through the Workshop hook, so disabled/revoked
capabilities skip rather than executing under stale authority
(`packages/gatekeeper-scheduler/README.md:77-95`).

Their placement reasoning is visible and honest: one account driver simplifies
management/revocation but creates a shared failure domain where a hung callback
can delay the account; batching limits ordinary load but does not eliminate the
tradeoff (`packages/gatekeeper-scheduler/README.md:142-160`). That is a useful
confirmation of our boundary. The scheduling mechanism belongs with an
executive/capability owner that understands retries and task intent, not in the
binary envelope router.

## 7. Forward plans and repository conventions

### Plans

- **pi migration — mostly shipped, plan partly stale.** `pi-impl.md` fixed the
  low-level `runAgentLoopContinue`, sequential tools, 30-turn cap, static prompt
  strategy, and provider model mapping (`plans/pi-impl.md:18-41`,
  `plans/pi-impl.md:198-267`). Current code confirms those decisions. The plan's
  PDF bridge and binding-transport follow-ups are already implemented in
  `ai-models.ts`; steering/follow-up UI, immediate per-message cost display, and
  custom synthetic message types remain **PLANNED** in the document
  (`plans/pi-impl.md:444-482`; `packages/workshop-backend/src/ai-models.ts:302-345`,
  `packages/workshop-backend/src/ai-models.ts:426-505`).
- **Multi-Gadget — core shipped, follow-ups remain.** The plan introduced one
  shared workpiece-ID namespace, per-Gadget binding edges, workspace-scoped
  chats, zero-Gadget workspaces, and per-Gadget facets
  (`plans/multi-gadget.md:7-37`). The current storage schema explicitly records
  the multi-Gadget migration and registry as source of truth
  (`packages/workshop-backend/src/overseer.ts:951-969`,
  `packages/workshop-backend/src/overseer.ts:1044-1063`). The plan itself says
  Part 1 was completed before Part 2 (`plans/multi-gadget.md:179-185`). Individual
  Gadget sharing, orphan-Gatekeeper UI, new workpiece types, and general
  per-callback restore targeting remain **PLANNED**
  (`plans/multi-gadget.md:164-177`).
- **Git storage — shipped through Part 3.** Do not report Part 1's Yjs layer as
  current; Part 3 explicitly replaced it with CodeMirror ChangeSet/Jupiter OT
  before deployment (`plans/git-storage.md:1101-1194`).

### Agent-facing repo conventions

This repository gives coding agents unusually concrete rules:

- `workshop-backend` and public `workshop-shared` are a kernel whose every line
  is reviewed; public exports require doc comments, duplicate RPC interfaces
  are rejected, and large kernel changes should be split
  (`AGENTS.md:14-20`; `REVIEW.md:10-23`).
- Capability review starts with ambience, the one resource-minting chokepoint,
  MCP annotation trust, OAuth SSRF preservation, and secret leakage
  (`REVIEW.md:25-41`, `REVIEW.md:45-59`).
- Cap'n Web promise pipelining is intentional and stubs must be disposed; the
  guidelines explicitly protect these non-obvious semantics from generic lint
  advice (`AGENTS.md:93-99`; `REVIEW.md:61-69`).
- Build guidance records silent cache hazards, requires `pnpm`, and names the
  workerd test fence that prevents a Node fallback from looking green
  (`AGENTS.md:61-93`).

The useful pattern is not the length of `AGENTS.md`; it is that security and
review invariants are stated at the exact seams an agent is likely to “simplify.”

## 8. Comparison table

Labels are architectural judgments, not feature counts.

| Axis | Cloudflare OS | CortexKit | Verdict |
|---|---|---|---|
| Process isolation | Gadget server code runs in Dynamic Worker facets with `globalOutbound: null`; UI runs in an opaque sandboxed iframe, but WebRTC and user-click navigation remain holes (`packages/workshop-backend/src/overseer.ts:3924-3938`; `packages/workshop-frontend/src/GadgetUI.tsx:491-505`; `packages/workshop-backend/src/browser-export.ts:35-43`). | Supervised out-of-process Rust modules, user isolation, one module crash/restart independent of peers. | **WE-LEAD** — OS processes give a clearer failure and runtime boundary; their server network denial is excellent, but the full Gadget includes a weaker browser boundary. |
| Capability model | Resource-specific Gatekeeper wrappers attenuate broad provider credentials and expose Cap'n Web objects; policy is executable integration code (`packages/gatekeeper-github/src/github.ts:1190-1268`, `packages/gatekeeper-github/src/github.ts:3242-3251`). | Manifest grammar + vault + deny edges; modules receive explicit capabilities without credentials becoming ambient. | **WE-LEAD**, narrowly — ours is more declarative and reviewable across the fleet; theirs has stronger provider-specific product APIs today. |
| Human in the loop | Durable observation/action log, simulation, revert metadata, two-key auto-approval, and UI cards (`packages/workshop-backend/src/overseer.ts:4357-4398`, `packages/workshop-backend/src/overseer.ts:4577-4619`; `packages/workshop-backend/src/auto-approval.ts:53-95`). Rejection has the reconciliation defects above. | Ask ledger blocks on explicit decisions and does not fabricate a completed world; less polished async batching. | **PARITY** — they lead on action UX/audit and optimistic throughput; we lead on epistemic integrity after denial. |
| Durable state | SQLite-backed DO aggregates, real git code objects, OT chat streams, Gatekeeper-local stores (`packages/workshop-backend/src/overseer.ts:945-1042`, `packages/workshop-backend/src/overseer.ts:1161-1206`). | Per-module SQLite + WAL and a backup module. | **PARITY** — they lead for collaborative code history; we lead on ownership, backup, and fault-domain separation. |
| Extensibility | Agents create full browser/server Gadgets, connect typed bindings, and package Blueprints; each app gets private DO state (`packages/workshop-backend/src/agent.ts:535-575`; `README.md:81-91`). | Modules are supervised binaries with a stable wire contract; broader language/runtime freedom, but higher authoring/deploy cost and no instant end-user UI primitive. | **THEY-LEAD** — Gadgets are a much lower-friction end-user extension unit. |
| Multi-agent | One primary chat-agent model plus optional spawned/callable chat agents inside the same Overseer (`packages/workshop-backend/src/overseer.ts:11121-11195`). | Peer fleet with seats, harnesses, durable work graph, and independent processes. | **WE-LEAD** — their spawner is useful delegation, not a peer fleet. |
| Model/provider routing | Anthropic, OpenAI, Google, Workers AI, and Ollama across direct, platform Gateway, and user Gateway paths (`packages/workshop-backend/src/ai-models.ts:350-375`, `packages/workshop-backend/src/ai-models.ts:507-645`). | Harnesses own model choice; the daemon routes envelopes rather than centralizing provider billing/transport. | **THEY-LEAD** as an integrated product feature; our separation is cleaner infrastructure but not equivalent UX. |
| Scheduling | Ambient Scheduler Gatekeeper with persistent callbacks, hook admission, alarms, retry fencing, and account UI (`packages/gatekeeper-scheduler/src/scheduler.ts:89-224`; `packages/gatekeeper-scheduler/src/schedule-driver.ts:237-299`). | Deliberately in executive module, outside daemon core. | **PARITY** in placement; **THEY-LEAD** in shipped end-user scheduling UX. |
| Audit and sharing taint | Every observation/action enters one workspace log; `prohibitAllSharing` can block later sharing/actions (`packages/workshop-backend/src/overseer.ts:4357-4398`, `packages/workshop-backend/src/overseer.ts:4577-4584`). | Framed envelopes and module logs, but no equally polished cross-connector activity surface in the brief. | **THEY-LEAD** — their unified user-facing activity ledger is a real product advantage. |
| Deployment/control plane | Multi-Worker release manifest, dynamic Gatekeeper bindings, Cloudflare-account deploy; local workerd is easy but production self-host is undocumented (`scripts/release/manifest-lib.ts:290-320`, `scripts/release/manifest-lib.ts:400-450`; `README.md:21-32`, `README.md:114-120`). | Local user-isolated daemon and supervised binaries; cloud control plane is not the core assumption. | **PARITY** — they lead at managed web deployment; we lead at local ownership and provider independence. |

No home-team score: Cloudflare OS is ahead where an end user feels the system—app
creation, connector UX, action history, cloud collaboration, and provider
routing. CortexKit is ahead where an operator debugs or constrains a fleet—fate
domains, declarative capability policy, credential ownership, and peer-agent
supervision.

## 9. Borrow candidates

Each candidate names the mechanism and its fleet landing point.

1. **Action/observation ledger as a first-class product surface.** Borrow the
   split between approved observations and pending/applied/rejected actions,
   including caller, resource, resolver, and auto/manual attribution
   (`packages/workshop-backend/src/overseer.ts:4357-4398`,
   `packages/workshop-backend/src/overseer.ts:4577-4619`,
   `packages/workshop-backend/src/overseer.ts:4183-4201`). **Land in:** MCP
   gateway/policy module as producer; executive/activity module as durable owner
   and UI/API surface. Do not bury it in transcript prose.
2. **Two-key auto-approval with an in-order drain.** Provider/module code must
   classify an action as auto-applicable **and** the user must enable the exact
   kind; stop at the first manual gate or failure
   (`packages/workshop-backend/src/auto-approval.ts:53-95`). **Land in:** tool-
   surface policy module, with the ask ledger as the authority that records the
   user's rule. This is safer than a global “auto approve” switch.
3. **Resource-shaped connector capabilities.** Copy the `account capability ->
   URL grammar -> immutable scoped session` pattern, including response
   post-validation for provider queries
   (`packages/gatekeeper-github/src/github.ts:1190-1268`,
   `packages/gatekeeper-github/src/github.ts:2551-2604`). **Land in:** connector
   SDK owned by the MCP gateway; compile resource URLs into manifest-grammar
   capability instances rather than exposing provider-wide tools.
4. **Optimistic actions only with a complete dependency ledger.** The useful
   mechanism is provisional logical IDs, read overlays, and real-ID rewrite on
   approval (`packages/gatekeeper-github/src/github.ts:2331-2476`,
   `packages/gatekeeper-github/src/github.ts:3254-3271`). **Land in:** executive
   ask/action module, not daemon core. Required acceptance condition: every
   derived action/artifact names its provisional dependencies; rejection
   atomically closes dependent ledger rows and schedules a repair/replan turn.
   Do **not** copy the current dropped-restart/stale-card behavior.
5. **Capability-only sandbox env.** Their strongest enforcement is
   `globalOutbound: null` plus a flat env of explicit loopback capabilities
   (`packages/workshop-backend/src/overseer.ts:3924-3938`,
   `packages/workshop-backend/src/overseer.ts:2656-2669`). **Land in:** subc
   supervisor/module launcher as an optional network-deny profile derived from
   the module manifest. The module gets vault-backed capability handles, not
   ambient sockets.
6. **Compaction checkpoint split.** Persist a semantic handoff separately from
   machine-derived replay state; protect unresolved decisions and reverts from
   the cut (`packages/workshop-backend/src/agent-compaction.ts:200-217`,
   `packages/workshop-backend/src/agent-compaction.ts:346-461`). **Land in:**
   harness Magic Context owner + agent-seat history module. Keep the transform
   lane authoritative for evidence; use model summary only as a replay aid.
7. **Git object database for agent-authored workspaces.** Real loose objects,
   refless application-owned heads, and explicit three-way conflicts are a
   cleaner substrate than opaque snapshots
   (`packages/workshop-backend/src/git-store.ts:1-36`,
   `packages/workshop-backend/src/git-store.ts:210-262`,
   `packages/workshop-backend/src/git-store.ts:443-474`). **Land in:** code/
   artifact workspace module used by agent seats, with the backup module as GC
   and export owner—not in subc's routing core.
8. **Scheduler placement and run fencing.** Stable `runId`, bounded retries,
   admission before every callback, and one explicit shared-failure-domain
   owner (`packages/gatekeeper-scheduler/src/driver-state.ts:37-80`,
   `packages/gatekeeper-scheduler/src/driver-state.ts:165-214`;
   `packages/gatekeeper-scheduler/README.md:142-160`). **Land in:** executive
   module. Treat this as validation of the current daemon-core exclusion, plus a
   concrete execution contract for that module.
9. **Kernel-specific agent/review instructions.** Preserve generic contributor
   guidance, but add seam-specific invariants where automated edits are most
   likely to erase capability or RPC semantics (`REVIEW.md:7-41`,
   `REVIEW.md:61-69`). **Land in:** subc and shared-protocol `AGENTS.md`/
   review checklist, owned by the daemon/protocol maintainers.

### Bottom line

Borrow their product mechanisms, not their fate domain. The best immediate
items are the unified action ledger, two-key auto-approval, resource-shaped
connector capabilities, and checkpoint split. Treat optimistic simulation as a
research candidate until its rejection invariant is stronger than Cloudflare
OS's current implementation. Their own code demonstrates both why the idea is
powerful and why “the agent may proceed as if approved” creates a new class of
durable consistency obligation.
