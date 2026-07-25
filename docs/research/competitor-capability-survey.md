# Competitor capability survey — hermes-agent, openclaw, paperclip, osaurus

Surveyed 2026-07-25 from local checkouts under `~/Work/OSS/`. Every claim below is
sourced to a file in those repos; anything the survey could not substantiate is
recorded as "not found" rather than inferred.

The point of this document is not to catalogue features. It is to name the places
where another project has already solved a problem we are currently living with,
and the places where our architecture is genuinely ahead so we stop re-litigating
them.

## What each project is

- **hermes-agent** — a self-improving multi-channel agent runtime. Terminal/TUI plus
  messaging gateways, skills, memory, cron, subagents, and several execution
  backends (local, Docker, SSH, Modal, Daytona).
- **openclaw** — a local-first personal assistant where a Gateway owns sessions,
  channels, tools, and state, and paired device "nodes" supply capabilities.
- **paperclip** — a control plane for operating a *company* of agents. Org charts,
  goals, budgets, governance, approvals, audit. Not an agent runtime; it drives
  other runtimes through adapters.
- **osaurus** — a Mac-native Swift harness keeping agents, models, memory, tools,
  identity, and automation on Apple Silicon.

## Findings that map onto problems we currently have

### 1. OpenClaw's OAuth refresh lock — directly addresses our dual-custody bleed

OpenClaw serialises OAuth refreshes with a filesystem lock keyed by provider and
profile identity, shared across every agent using that profile
(`src/agents/auth-profiles/path-resolve.ts`, `docs/auth-credential-semantics.md`).
The lock exists specifically to stop concurrent refreshes from racing on a
single-use refresh token.

This is the mechanism our dual-custody problem has lacked. Our rotation-chain
competition — vault and another custodian sharing one refresh chain — has been the
dominant source of `needs_reauth` events, and we have been solving it by moving
custody provider-by-provider rather than by making concurrent refresh safe.
A refresh lock does not replace vault-native login, but it makes the intermediate
state survivable instead of lossy.

OpenClaw also deliberately does **not** copy OAuth profiles between agents by
default, which is the same conclusion we reached about per-connection handles.

### 2. Paperclip's tool gateway quarantines new tools

Paperclip's gateway catalogues MCP tools, classifies each as read / write /
destructive, **quarantines newly-appearing tools until reviewed**, requires human
approval for risky calls, signs arguments, and audits credential resolution
(`server/src/services/tool-gateway.ts`).

The quarantine is the interesting half. Our tool-surface refresh policy already
has the shape (a pending-changes queue with a configurable drain class, so a new
tool does not silently enter the surface and bust the cache), but Paperclip pairs
it with a *risk classification* that decides whether a call needs approval at all.
Our `execution_mode` carries similar information and is not currently used to gate
anything at the facade.

### 3. Osaurus separates AppleScript from computer-use

Osaurus exposes GUI driving (`computer_use`, AX trees and SOM captures via
`NativeMacDriver`) and scripting (`applescript`, executed in-process through
`NSAppleScript`/OSAKit) as **two distinct tools** with distinct gates
(`docs/COMPUTER_USE.md`, `Packages/OsaurusCore/AppleScript/Tool/AppleScriptTool.swift`).

This matches the structured-interface-first ladder we settled for cerebellum and
plexus, and it is worth knowing that someone shipped it as a hard tool boundary
rather than an internal preference. Scriptable Mac apps — Finder, Safari, Mail,
Notes, System Events — are reachable without touching pixels at all.

### 4. Osaurus's browser: per-agent profile, user-mediated login

`browser_use` runs against a persistent per-agent WebKit profile. Cookies and
sign-ins survive restarts, are isolated from other agents and from the user's real
browser, and **the user completes login in a visible window — the agent never types
credentials** (`docs/BROWSER.md`).

That last property is a cleaner answer to browser credential custody than anything
we have specified. It removes the credential from the agent's reach entirely rather
than guarding it, which is the structural version of the argument we made about
banning bearer handles from tool arguments.

### 5. Both browser-driving projects hide tool chatter in nested subagents

Osaurus runs browser and computer control inside nested subagents that return
distilled summaries to the parent rather than primitive tool chatter. Hermes does
the same through its `cua-driver` boundary. For us this is a context-economics
finding: GUI driving generates enormous low-value transcript volume, and the
projects that shipped it all chose to keep that volume out of the parent context.

### 6. Scheduled autonomous work is table stakes and we are the outlier

All four have first-class scheduled execution — Hermes cron with a file lock and
thread pool, OpenClaw heartbeats plus cron with retries and isolated sessions,
Paperclip routines that materialise as tracked issues, Osaurus schedules and
watchers plus a one-slot self-scheduler.

We have the dreamer and smart notes, which are narrower. There is no user-facing
"run this instruction on a schedule" primitive. This is the clearest capability
gap in the survey.

Paperclip's variant is the one worth copying: **a routine execution becomes a normal
tracked issue**, so scheduled automation is auditable and reviewable rather than an
opaque background job.

Osaurus's variant is worth copying for safety: one self-scheduled slot per agent,
cleared before dispatch, fresh session per wake, explicit re-scheduling required.
That is a bounded autonomous loop by construction rather than by policy.

### 7. Hermes's self-improving skill loop — and why the gap reverses on inspection

Hermes creates and refines procedural skills from experience (`README.md`,
`agent/memory_provider.py`). Our knowhow skills are curated and human-authored, so
the primitive is genuinely absent for us.

But a skill refined from experience is a *claim about what worked*, and an agent
writing its own procedure from a run it believes went well encodes the belief
rather than the outcome. Hermes has no settle step and no external scorer, so its
loop necessarily learns from self-assessment.

We can do the thing that makes it safe: learn only from **settled work carrying a
reviewed quality score assigned by someone other than the author**. The work graph
already records exactly that. "The agent thinks this went well" and "a reviewer
accepted this at 92" are different propositions, and only the second is worth
encoding into durable guidance.

So the finding inverts: hermes has the primitive, we have the ground truth, and
the primitive without the ground truth is a machine for propagating plausible
mistakes.

## Where we are ahead, and should stop re-litigating

- **Context compaction.** Osaurus has no general conversation compaction; OpenClaw
  auto-compacts with summaries written into transcripts; Hermes compresses with an
  auxiliary model. None of them treat prompt-cache stability as a first-class
  constraint. Our cache-stable fold with a frozen boundary and byte-identical
  replay is meaningfully more advanced than anything in this set.
- **Transport security.** Osaurus's secure channel — pinned identity, ephemeral
  X25519, ChaCha20-Poly1305, forward secrecy, no plaintext fallback — is close to
  what we built with Noise IK, and confirms the shape. Nobody else in the set has a
  relay-with-blind-ciphertext model.
- **Credential custody.** Osaurus uses the macOS Keychain directly; Paperclip uses
  an encrypted local secret provider; Hermes uses profile-scoped environment
  isolation. None has an audited, epoch-fenced vault with crash-safe rotation.
- **Provider abstraction.** None of the four has anything resembling the wire-family
  renderer model with byte-determinism guarantees on resume.
- **Routing.** None has cost-primary routing, quota cooldowns, or adequacy gating.
  Every scheduled run in those systems costs whatever the default model costs.

That last one reframes the whole survey: the gaps above are real, and they are
cheap for us specifically. They have primitives we lack; we have the substrate that
makes those primitives inexpensive to run.

## What I am routing where

| Finding | Seat | Why |
|---|---|---|
| OAuth refresh lock | CKCRED | Direct mechanism for the dual-custody rotation race |
| Tool quarantine + risk classification | SUBC (facade), PLEX | Pairs with the pending-changes queue; `execution_mode` is unused at the facade |
| AppleScript as a separate gated tool | CEREB | Confirms the structured-first ladder as a shipped boundary |
| User-mediated browser login | CEREB, PLEX | Removes browser credentials from agent reach structurally |
| Nested subagent distillation for GUI work | CEREB | Context economics of pixel-driving |
| Scheduled work as tracked issues | ALF | Closest thing to our work graph; auditable automation |
| One-slot self-scheduler | ALF | Bounded autonomous loop by construction |
| Self-improving skills | ALF | Capability gap against hermes |

## Method note

This survey was itself a casualty of the day's harness fault: the first dispatch
produced zero output for ninety minutes while reporting `running`, and was only
found by running ALF's stall detector against my own repo rather than only pointing
it at others. The relaunched run completed in 36 minutes. Worth recording next to
the findings, because the failure was invisible from every surface that claimed to
be watching it.
