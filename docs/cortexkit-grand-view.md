# CortexKit — The Grand View

Purpose: the complete picture of what CortexKit is and what exists today,
written as source material for product/website work. Substance only, no
copywriting. Companion to `fleet-map.md` (which is the internal seat-onboarding
version); this one explains the system to someone meeting it for the first
time. Maintained by the SUBC seat. Last updated: 2026-07-19.

## The thesis

CortexKit is an AI operating system: the infrastructure layer that sits
between AI agents and everything they need — models, tools, credentials,
memory, money, other machines, and the human. It is local-first: it runs as a
daemon on the user's own machine, holds their credentials in their own vault,
keeps their conversation memory in their own store, and touches the cloud only
through zero-knowledge encrypted lanes. It is model-agnostic (30+ providers,
multiple accounts per provider, cost-aware routing) and harness-agnostic (its
capabilities reach agents running in our own runtime, in third-party tools
like Claude Code and Codex, or in any MCP-capable host).

The organizing idea: everything an AI agent needs to be genuinely useful over
months — durable sessions, unbounded context, tool access, spending limits,
credential custody, cross-device reach, team boundaries — is infrastructure,
and infrastructure belongs in an OS layer, not re-implemented inside every
chat app. CortexKit builds that layer once, as supervised modules around a
tiny router, and every surface (native app, terminal, chat platforms,
third-party harnesses) consumes it.

Most modules are named for brain anatomy, honestly mapped to function:
subconscious routes beneath awareness, broca produces speech, wernicke
comprehends it, thalamus relays, callosum connects hemispheres, engram is
memory trace, astrocyte regulates metabolism, synapse connects locally.

## The substrate: subconscious

One daemon per user machine: `ck-subc`. It does exactly two things — routes
frames between clients and modules by reading a fixed 21-byte header (never
parsing payloads), and spawns/supervises the module fleet (health probing,
restart budgets, crash recovery, resource limits). Everything else is a
module. The daemon is single-principal by design: one security domain per
daemon, forever. Orgs get their own daemons rather than multi-tenancy.

Around it: a wire protocol with per-route epochs and HMAC-authenticated
loopback transport; client SDKs in TypeScript, Rust, and Swift; the `ck`
operator CLI (module control, health drill-downs, quota views, git-style
dispatch to module CLIs); and an MCP gateway that exposes the whole module
fleet to any MCP-capable host with per-tool policy control.

Everything below is a supervised module — separate process, own repo, own
store, zero daemon code. The daemon has needed no per-module edits since the
architecture landed; that invariant has held through fourteen modules.

## The organs

**AFT — code perception and action.** The tool engine: indexed code search,
structural outlines, symbol zoom, call graphs, semantic search, editing with
syntax validation, PTY-capable shell, worktree management. ~21 tools, shared
artifact stores so parallel agent worktrees reuse one index. This is what
agent hands and eyes are made of on this platform.

**Broca — the durable LLM loop.** Runs agentic conversations as write-ahead-
logged, exactly-once state machines: a crash mid-tool-call resumes without
re-executing side effects; a session is a lineage of runs that survives
restarts, machine reboots, and provider outages. Native renderers for five
provider wire families (Anthropic, OpenAI Chat + Responses, Gemini, Bedrock,
plus specialized endpoints like ChatGPT-account and Antigravity) with
byte-deterministic request rendering — the property that makes prompt caching
provably stable. Catalog-driven: any of ~130 providers works through
configuration, not code.

**Magic Context — the context lifecycle.** Solves the context window as an
engineering problem: autonomous compaction (a historian model folds old turns
into compartments while the agent works), durable cross-session memory,
retrieval over everything that ever happened, and a cache-stability core that
guarantees compaction never thrashes the provider's prompt cache (folds cost
exactly one cache rebuild, by proof). The practical result: sessions that
run for weeks without hitting a context wall, at flat token cost.

**Thalamus — the provider-wire relay.** A local proxy that sits on the HTTPS
path between third-party harnesses (Claude Code, Codex) and their model
providers. It byte-splices Magic Context's compaction into their traffic
without those tools' cooperation: cache-neutral edits, verified against real
provider billing. This is how unowned harnesses get unbounded sessions.

**Alfonso — the agent runtime.** The layer that makes agents into a workforce:
background workers in isolated git worktrees, model routing (cheapest model
clearing a per-task capability bar, quota-aware, multi-account), asks (typed
questions to the human with urgency and default-on-silence), rooms (multi-
agent meetings and channels with transcripts), the Board (structured
agent-to-human surface: status, asks, artifacts), a work graph (epics/tasks
with dependency gating), and Athena (multi-model adversarial review panels —
the design-gate machinery the fleet itself is built with).

**Quota — provider usage truth.** Live usage windows for 30+ AI providers
(OAuth quotas, cookie-based, local probes), multi-account, feeding both the
human view (`ck quota`) and the router's avoid-exhausted-accounts decisions.

**Credentials — custody.** A local encrypted vault (single-writer,
audit-chained, crash-safe OAuth refresh custody) that is the login root for
provider credentials — plus CortexKit Account, the cloud identity service
(five login methods including passkeys) that anchors device pairing, org
membership, and the coming team plane. Org credentials never route to member
devices; personal credentials never leave the machine unencrypted.

**Synapse — local AI infrastructure.** Embeddings, rerankers, and small-model
serving lanes running on the user's own hardware, with certification gates so
consumers know what a lane actually provides.

**Callosum — federation.** Cross-machine transport: Noise-encrypted
device-to-device channels, capability exposure that defaults to deny,
exactly-once effect semantics over unreliable networks, WAN-proven. A user's
laptop can call a tool on their VPS as if local; an org daemon federates with
member devices. Cloud rendezvous/relay (for NAT traversal) is designed, with
the cloud seeing ciphertext only.

**Astrocyte — metabolism.** Meters every AI dollar across the fleet against
real published prices, maintains spend facts with provable arithmetic, and
enforces budget caps at admission time (a run that would exceed its cap is
refused before it starts, with typed refusal reasons). Verdicts-not-actions:
it never kills work, it prevents overcommitment.

**Engram — memory trace.** Zero-knowledge cloud backup: client-side encrypted
increments of every module's stores, per-device manifest chains, garbage
collection, and (designed) cross-machine session restore. The cloud provider
cannot read anything. The master key lives in the local vault and moves
between devices only over federation.

**Wernicke — the chat gateway (building).** Slack, Teams, Telegram, Discord.
Org-installed bots where only the linked account owner's direct mention
carries authority — every other message is untrusted context, structurally
segregated against prompt injection. The bridge between org chat and org
agents, built on the team-mode authority contract.

**ck-projects — topology (designing).** The just-settled workspace/project
registry: workspace → projects → each project owning N directories. Built for
non-developers too (scattered folders grouped into projects with no git
required), with federation-ready remote references. Every module consumes it
for grouping; none for identity.

## The surfaces

**Cortex** (design stage) — the native app, every platform native (no
Electron). Board-first: the agent communicates through structured lanes
(chat, asks, status, artifacts) rather than a wall of text. Bundles the
daemon and modules; will be the config editor and OAuth custodian. The
long-term human home for the platform.

**subc-chat** — the Swift proving ground for Cortex: live sessions, rooms,
asks with notifications, the Board tab, observability into agent lanes
(consults, background tasks, token metrics).

**brocatui** (building) — the terminal harness: full-transcript virtualized
scrollback, driving broca sessions over the wire.

**ck CLI** — the operator's view: fleet health, module control, quota,
per-module drill-downs.

**Third-party harnesses** — Claude Code, Codex, and any MCP host consume the
fleet through the gateway and thalamus without knowing CortexKit exists.

## Team mode (implemented core, surfaces building)

The same stack, org-shaped: an org runs its own daemon (own vault, own module
fleet, own memory pool, org-level agents) joined to member devices via
federation. The authority layer shipped this week across four modules: org
grants (account-bound, epoch-fenced revocation), acting-for identity
(`org-agent acting-for alice@org`, stamped by infrastructure and
unforgeable-by-construction), an intent ledger making every org-authorized
action exactly-once across crashes, and an org-plane ask machine where
revocation between answer and execution kills the action. Coming next:
per-user reversibility ceilings (junior agents can only take undoable
actions), approval quorums, and team memory distribution — each designed on
the same gated-contract method.

## What is live today

Production, on real workloads, verified from live state: the daemon and
thirteen supervised modules; wire v2 fleet-wide; AFT serving all tool traffic;
Magic Context compacting both owned sessions (via broca) and Claude Code
sessions (via thalamus) with proven cache neutrality; the full metering path
(broca facts → astrocyte pricing → live cap refusals); the credential vault
as OAuth login root; federation across real WANs; engram's cloud plane and
GC; the Room-1 org authority layer conformance-tested across four repos; the
Board surfaces. Building: wernicke, the chaos suite, brocatui, Cortex app,
Room-2 (ceilings/quorum), engram restore, federation phase 3, ck-projects.

## How it is built (the part that does not show but explains the quality)

The fleet is built by the fleet: fifteen AI agent seats, each owning its
repo, coordinating through the same rooms/asks/board machinery the product
ships. Every cross-module contract goes through adversarial multi-model
design gates before code; every seam is proven by live cross-module drives
before production (thirteen real integration bugs were caught at seams in a
single day this week — none reached users). Conformance corpora are vendored
across repos and regenerated from real serialization paths, never
hand-authored. The method is the moat as much as the code is.
