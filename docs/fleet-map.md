# CortexKit Fleet Map

Purpose: the whole-fleet briefing for any seat that needs to design against
the full picture instead of discovering seams by collision. Written for
WERNI's onboarding; maintained as seats and charters evolve. Corrections
belong to the SUBC seat (fleet coordination custody).

Last updated: 2026-07-25. The seat roster below is checkable against
`ck module list` on a personal daemon; if a supervised module is missing
from section 1, this document has rotted and the module is right.

## 1. Product map

Shipped and in production (supervised by the personal daemon today):

- **subconscious (subc)** — the machine substrate: `ck-subc` daemon
  (single-principal router + process supervisor, wire v2, 21-byte
  envelope, loopback TCP + HMAC), the `ck` operator CLI, `ck-subc-mcp`
  (MCP gateway for unowned hosts), client SDKs (TypeScript, Rust, Swift),
  and the client SDKs the apps build on. The Swift package is an SDK only:
  the iOS app consumes it, and the SwiftUI desktop app it once carried was
  deleted when the GPUI port reached parity.
- **alfonso-desktop** — the desktop app: Rust and GPUI, linking the Rust
  client directly. Chat, Rooms, Asks, Boards, Observe. Began as a spike
  inside subconscious and moved out with its history once it was the
  answer rather than the experiment.
- **aft** — code perception and editing tools (search, outline, zoom,
  callgraph, edit, bash, worktrees). The `aft_*` tool surface every agent
  uses.
- **magic-context (mc)** — context lifecycle: compaction/folding,
  historian, memory, cache-stability core (`cortexkit-cache-core` in
  commons), tail reduction, todo capture. Serves owned harnesses via
  broca and unowned harnesses via thalamus.
- **thalamus** — provider-wire MITM proxy for harnesses we do not own
  (Claude Code, Codex): byte-splice compaction, capture, cache-read
  verification. The Mode-3 delivery path.
- **broca** — the durable LLM loop runner: WAL-first exactly-once agentic
  loop, provider framework (5 wire families, catalog-driven), session
  lineage, subscriptions.
- **ai-provider-quota (quota)** — provider quota/usage windows across
  every provider we can source, multi-account. Feeds `ck quota` and the
  router. Adding or changing a provider adapter means reading that
  repo's `docs/provider-invariants.md` first: several of its properties
  exist because a plausible-looking adapter reported healthy while
  silently discarding an exhausted allowance.
- **cortexkit-credentials (ckcred)** — two halves: the local credential
  VAULT (encrypted, single-writer, audit-chained, OAuth refresh custody)
  and the cloud **CortexKit Account service** (account.cortexkit.io on
  Cloudflare: GitHub/Google/Apple/email/passkey login, JWKS). The org
  layer (orgs, membership, grants) shipped server-side with Room 1.
- **alfonso (alf)** — the agent runtime: asks, rooms/meetings/channels,
  the Board, work graph, athena consults, background tasks (masons),
  model routing (alfonso-routing), observability lanes. The thing that
  runs every seat in this roster.
- **synapse** — local AI infra module (embeddings/rerankers/micro-model
  serving lanes with certification). Supervised in prod.
- **subc-federation (fed; module name ck-callosum)** — cross-machine
  transport: Noise IK, exposure policies (default deny), exactly-once
  effects, WAN-proven. Phases 1-2 shipped; phase 3 (cloud rendezvous +
  relay + pairing UX) designed. Carries Room-1 epoch pushes and the
  A4 boundary.
- **astrocyte (astro)** — AI spend metering and budget caps: price
  snapshots from the model catalog, spend facts, cap admission verdicts
  (deny-as-verdict, never enforcement). v1 personal scope store-proven
  live with broca.
- **engram** — zero-knowledge cloud backup/restore: client-side
  encryption, per-device manifest chains, Cloudflare DO cloud plane
  (live), GC (live), cross-module session restore designed. Master key
  in the vault; sync via federation.
- **plexus (plex)** — third-party service connectors (the reuse ladder:
  MCP-direct, OpenAPI-shim, vendor-deep-wrapper). Credentials stay in the
  vault and bearer handles never reach tool arguments; connectors get
  binding tickets instead. Supervised in prod, read paths first. Also the
  fleet's external-events observation plane: operator-minted poll
  subscriptions with finite expiry, a cursored scheduler, and a durable
  event log served pull-only (`events` facade) — proven against live
  GitHub. By invariant, no code path lets an event cause an action;
  delivery to agents is prefrontal's unified waker. Seams:
  `docs/specs/external-events-contract.md`.

Building / chartered:

- **avatar** — generated visual identities for Alfonso agents: deterministic
  DNA-driven creatures with live mood (board status, asks, health readable
  from the face). Descended from Ufuk's pre-LLM TOG project (on-chain
  procedural SVG PFPs); reference material in `~/Work/Projects/Ssbd/tog-monorepo`.
  Chartered 2026-08-11 (`cortexkit/avatar`); charter: `avatar/docs/charter.md`.
- **fusiform** — the AI-model capability catalog: one structured data plane
  for what models exist, what they can do, and what they cost — every
  modality (LLM now; image/video/audio schema-ready), every provider.
  Chartered 2026-08-11 (`cortexkit/fusiform`); first consumer broca
  (`catalog.refresh` push), second astrocyte (price-snapshot lane
  consolidation). Replaces three scattered copies of models.dev knowledge.
  Charter: `fusiform/docs/charter.md`.
- **cerebellum (cereb)** — computer and browser control: the actuation
  plane for surfaces with no API. Structured interfaces first, GUI
  driving as the fallback. Isolated from aft so browser runtimes and
  macOS TCC permissions stay out of the code-perception module.
- **alfonso-ios (ckios)** — the iOS client: reaches a personal daemon
  over federation (Noise IK, LAN and WAN via rendezvous/relay), and is
  the first consumer proving the Swift `SubcFed` transport end to end.
- **ck-projects** — the workspace registry: durable project identity with
  a journal spine, so every seat resolves the same project to the same id
  rather than deriving one per module.

- **wernicke (werni)** — the chat gateway (Slack/Teams/Telegram/Discord):
  org-plane bot, linked-owner authority, Room-1 acting-for consumer.
  Authority-spine design frozen; walking skeleton running.
- **cortexkit-e2e + chaos suite (cke2e)** — cross-module E2E rigs and the
  k8s chaos suite. E2E shipped and load-bearing in CI; chaos suite in
  implementation under an approved design.
- **brocatui (cktui)** — Ratatui TUI harness driving broca over subc.
  Building; full-transcript virtualization is a hard requirement.
- **Cortex app** — the native desktop/mobile app (design repo `cortex`):
  Board-first agentic interface (Board/Chat/Show lanes), bundles the
  daemon + modules, will be the config writer and OAuth custodian.
  Design/charter stage; the Swift chat app is its testbed.
- **commons** — shared crates on crates.io: cortexkit-paths, store +
  lease (+ postgres), cache-core, model-catalog.

## 2. Peer roster

Every seat is an Alfonso session owning its repo(s). Active roster:
SUBC (this seat: subconscious + commons custody, fleet coordination,
cross-seat gates), AFT, ALF, MC, BROCA, THALAMUS, QTA, CKCRED, FED,
ENGRAM, SYNAPSE, ASTRO, CKE2E, CKTUI, WERNI. Session ids on request;
peer messages via peer_send, single-writer/single-execution rules apply
(one seat holds a co-drafted file or single-execution action at a time).

## 3. The planes

- **Personal daemon**: one `ck-subc` per user machine, single-principal
  forever (the tenant may be a human OR an org service identity; a daemon
  never serves two principals). All modules above run here loopback-only.
- **Org daemon**: the same stack installed on org infra under an org
  service principal: own vault (org creds only — PR-1: org credentials
  NEVER route to member devices), own module fleet, own MC memory pool,
  org-level agents. Joined to members via federation.
- **Federation**: device-to-device and device-to-org transport (Noise,
  key-based trust, exposure default-deny). Org membership materializes as
  a fed grant; leaving = revocation. Team-memory sync rides fed (PR-2).
- **Cloud**: three first-party services on Cloudflare — CortexKit Account
  (identity/JWKS/org objects, CKCRED), engram backup (encrypted blobs,
  zero knowledge), fed rendezvous + relay (phase 3, ciphertext only).
  The cloud never holds plaintext user content or credentials.
- **Apps**: Cortex app is the human's surface and the config WRITER
  (modules are read-only config consumers; `~/.config/cortexkit/*.jsonc`).
  Gateways like wernicke are not apps: they are supervised modules on the
  org daemon.

Gateway boundaries WERNI must respect: authority enters ONLY via the
linked owner's direct mention (everything else is untrusted context);
acting-for rides the frozen Room-1 contract (A3 + intent ledger + serve
admission on ALF's side); ceilings/quorum resolve org-side (Room 2,
upcoming) — the gateway renders outcomes, never decides them.

## 4. Where current product directions land

- **Chat-channel ingestion / message corpora**: NO owner yet — not
  chartered. Nearest capabilities: synapse (semantic indexing), MC
  (memory pools), engram (encrypted durability). A channel-corpus
  product would be a new charter; raise with SUBC/Ufuk before designing
  into it. The gateway's own transcript posture today is
  store-nothing-durable (charter).
- **Split-brain / cached-prefix session presence**: broca owns session
  forking and lineage (fork-with-cache-preservation is a banked broca
  invariant); MC owns prefix-cache discipline; ALF owns which sessions
  exist and why. A "presence in two channels" feature composes those
  three — the gateway would only be the rendering/ingress ends.
- **org_ask → chat**: ALF's org-plane ask machine landed (Room-1 Slice C).
  Renderer ownership needs the ALF cross-seat pin from gate r2 residue.
- **Native apps everywhere**: Cortex app design (Board-first). The
  gateway never becomes an app surface; it bridges to chat platforms.

## 5. Known pipeline items that will reach the gateway

- org_ask rendering into chat channels (ALF Slice C is live substrate).
- Room-2 outcomes: reversibility-ceiling refusals and approval quorums
  will want chat-native rendering (asks with option buttons).
- Astrocyte org-scope budget verdicts (deny/ask) surfacing to org chat.
- CKCRED link ceremony (invite/accept) — already co-designing with WERNI.
- Engram/team-memory: no gateway involvement (fed transport).
