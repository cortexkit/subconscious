# Cortex App — Design Record

> Rescued from a standalone `cortex/` repository that had no remote and
> existed only on one machine. The app was later renamed from Cortex to
> Alfonso; this record keeps its original wording and predates that rename.


Status: DESIGN (living). Captures the design conversation to date so nothing
is lost. Not yet a build spec; open questions flagged inline. Provenance:
Ufuk design sessions 2026-07-08 and 2026-07-12, plus channels #plugin-hooks,
#visual-context.

---

## 1. What Cortex is

**Cortex** is the native desktop app: the *conscious* layer over the
`subconscious` daemon. It is the single surface through which a human interacts
with Alfonso. Metaphor family: subconscious (daemon) → synapse (local models)
→ cortex (the window the user touches). Org/brand stays **CortexKit**; the app
is **Cortex**; the agent is **Alfonso**.

- **Native per-OS, no web shell.** SwiftUI/AppKit on macOS first, native on
  Windows/Linux too. NOT Electron, NOT Tauri. The one scoped exception is the
  Board's block-rendering region (a sandboxed webview), justified in §5.
- **App and agent are deliberately distinct.** Alfonso must survive engine
  swaps and app rebrands, so "name the app after the agent" is structurally
  wrong. Cortex = app, Alfonso = agent.

### 1.1 Strategic lock (Ufuk, 2026-07-08)
**Alfonso is only reachable through Cortex.** The engine that executes turns
(our own llm-runner/broca, or Claude Code, Codex, OpenCode) becomes a swappable
choice. Everything durable — memory, profile, compaction lineage, Board history,
the working relationship — lives on *our* side of the boundary, never in the
engine. Consequences:
- The **relationship is the product**; the engine is demoted to an execution
  detail. Switching engines preserves Alfonso's continuity because nothing
  durable was ever in the engine.
- The **lanes/Board become the product contract** every engine-adapter must
  drive (see §4, §8). "Engine adapter" is a first-class concept.
- Today's dogfood mode (Alfonso-in-OpenCode, no Cortex app) is transitional;
  there is a migration moment where this primary relationship moves into the
  app with memory intact (cheap: MC store + memories are already module-side).

---

## 2. Onboarding

Five steps (Ufuk's flow + amendments):

1. **Animated welcome** — quick "what this is."
2. **CTA → onboarding.**
3. **Install — bundle, don't download.** The app bundle ships `subc-core` +
   the core module set embedded (small Rust binaries; tens of MB even for ~10).
   "Install" = place binaries, write `subc.jsonc`, install a *user-level*
   launchd/service agent (no admin password — say so on screen), start the
   daemon. Works offline, one signed/notarized artifact clears Gatekeeper.
   The progress screen renders REAL state: `supervisor.list` + `supervisor.health`
   let the app show each module registering and flipping to `ok` live — the
   first truthful "system coming alive" moment, doubling as self-diagnosis.
   Heavy/optional modules (e.g. Synapse model weights) download later, on
   demand, supervised by the running daemon.
4. **Profile questions during install dead-time.** Ask name (how Alfonso
   addresses the user), primary goal (sw dev / assistant / office), skill level.
   **Stored in a module store via subc** (alfonso-core store or a small profile
   domain), NOT app-local prefs — so every agent in every harness reads the same
   profile. UI customization (e.g. don't show diffs/PR-review to a non-dev) and
   agent tone both fall out of the shared profile. Cap form questions ruthlessly;
   let the initiator agent probe conversationally in step 5 (a question the agent
   asks builds the relationship; a form question costs abandonment).
5. **Sign-in.** "Use the subscription you already have" (Claude Pro / ChatGPT /
   Gemini) is the FIRST-CLASS path, not a fallback — proven dogfood path, zero
   marginal cost, makes any free-tier negotiation a nice-to-have not a launch
   dependency. Cortex is the vault's "natural signed custodian": it drives the
   OAuth flow with a real local callback (retires the paste-dead-URL flow) and
   writes into the vault via CKCRED login machinery. Then hand off to Alfonso's
   initiator agent.

**Missing screen to design: first-run adoption/failure path.** Port 8757
occupied, an existing daemon (dogfood machines), a half-dead prior install →
detect-and-adopt, not fail-and-confuse. The health/watchdog surfaces already
shipped give the app everything to say precisely what state the machine is in.
Also design the "sign-in failed / no provider" branch so first-run still lands
somewhere useful (functional-but-agentless retry state) — OAuth-in-browser is
the single most fragile step and must not gate the whole first impression.

---

## 3. Process/naming hygiene

- Every executable carries the **`ck-*` prefix** (done: fleet renamed
  2026-07-10/11 — `ck-subc`, `ck-aft`, `ck-mc`, `ck-broca`, `ck-thalamus`,
  `ck-quota`, `ck-credentials`, `ck-alfonso`, etc.). Makes the whole family
  legible in Activity Monitor / Task Manager.
- `module_id` is the load-bearing rename (it is the storage, lease, and vault
  scoping key — store paths, keychain service names, handle scoping all derive
  from it). Module renames landed as a coordinated cut BEFORE public release;
  repo names are cosmetic (GitHub redirects).

---

## 4. The Board (the agentic interface)

### 4.1 The problem being solved
Today one linear text stream carries four kinds of traffic — working status,
conversation, questions-awaiting-you, and artifacts — and every interruption
(mason done, bash task ends, PM arrives) scrolls the other three away. The
`ask` tool failed because it wrote into the same stream it was trying to escape.
The fix is separate lanes with separate lifetimes.

### 4.2 The Board model (Ufuk refinement, 2026-07-12)
The **Board** (formerly "Canvas") is the SINGLE surface through which the
primary agent communicates with the user. **Chat is one channel of the Board.**
Everything user-facing — chat, asks, status, visual explanations, shown
artifacts — goes through the Board. The Board is **per-session** (one board per
conversation, channels within it).

Collapsing to one surface (vs three separate lanes) is stronger because it
turns four fuzzy tool boundaries into one invariant: "the agent communicates
only through the Board." One output path, one lifecycle model (every board
entry is a typed post with a state), asks/show/status/chat become channel-types
on one surface rather than separate concepts.

### 4.3 Substrate: rooms/channels (already shipped)
A Board is, almost exactly, a per-session **room** with typed **channels** —
the rooms/channels primitive ALF already built (chat channel, asks channel,
status board strip, artifact posts) with the lossy push-hint + pull-current-state
transport. Reuse this: the hard parts (durable transcript, push coalescing,
multi-renderer) are proven. Board module = a room the app renders.

### 4.3.1 Board tool — settled design (co-design meeting, 2026-07-15)
Settled in the Board co-design meeting (SUBC chair, ALF, Ufuk; all decisions
Ufuk-blessed). The meeting minutes are the authority; this section is the fold.

**D1 — boundary.** The Board fully replaces the parent→user `ask` tool (verb +
presentation). The ask RECORD layer stays verbatim underneath: durable rows,
CAS single-winner answers, veto-window auto-proceed, campaign-approval
validation, delivery/revival machinery. `board.ask` = one atomic transaction
writing a normal ask row + a Board block carrying the requestID; answering from
ANY surface routes through `persist_answer`. Subagent→parent ask is unchanged
(recipient_kind='parent', no Board involvement). Three guardrails from the ask
owner: block lifecycle is a PROJECTION of the ask row (never a second truth);
answers go only through `persist_answer` (no room-side answer path); silence
policy math stays computed module-side at record time.

**D4 — module home.** Board lives in **alfonso-core** (no ck-board module);
ops served under the `board.*` namespace. Rationale: ask row + board block must
commit atomically in the same single-writer store; a separate module turns that
into distributed consistency for no gain. Storage rides the rooms engine: one
room per session-board, a `lane` field on events (chat/asks/status/artifacts —
no per-lane transcripts until something needs per-lane cursors), typed block
payloads, upserts as post-with-supersede events (blockId + supersedes seq,
newest-wins fold, append-only history), per-blockId ordering by event seq.

**D2 — verb set v1.**
- `board.post {lane, blockType, props, blockId?}` — append or upsert.
- `board.retire {blockId}` — supersede-to-tombstone (renderer hides, history
  intact); finished-work status blocks must not linger.
- `board.ask {raw ask fields}` — atomic ask-row + block; returns
  {requestID, blockId}.
- `board.show {pointer, caption?}` — pointer-only v1 (path | message-tag |
  doc-ref); rendering app-side; cheap-model augmentation later.
- Read side is NOT a verb: the anti-repeat m1 index (§4.5) is reflected
  automatically; renderers use a `board.state` fold read. Deliberately absent:
  delete (user gesture) and layout (app-side; agent gives order/size hints).

**D6 — config gate + enforcement.** `board.enabled` per-session, frozen at
attach (byte-affecting input; mid-session flip requires a new session). OFF =
today's behavior exactly. ON = **swap-not-remove**: `board.ask` replaces the
ask tool in the surface with the same core fields, so trained ask-shaped
behavior lands on the Board verb (worst case a weak model uses it exactly like
old ask, which still works). The asked-in-prose-and-parked failure is caught by
a turn-end detector (regex v0, microllm oneshot as plan of record) writing an
adapter-authored flag as a health status block; enforcement is one idempotent
nudge per turn-end, then a visible "may be waiting without asking" chip;
escalation counters live module-side. `board.health(sessionBoard)` is the read.

**D5 — migration.** Nothing to migrate: the SubcChat Asks tab remains the
interim consumer of `ask.*` ops; Board v1 builds alongside behind the flag;
per-harness `board.enabled` flip; no dual-write, no cutover event.

**Gate to default-ON:** the §9 prose-quality bench (3 arms × model spread).
v1 ships behind the flag; the bench decides the default. Follow-ups owned:
ALF writes the m1 index-field sketch + board.health shape (SUBC reviews
against MC's m1 cache discipline); ALF+Ufuk finalize tool shape so the next
fleet restart carries a single cache bust.

### 4.4 The prose-quality blocker and its resolution
**Concern (Ufuk):** LLMs are trained on assistant *text* as the natural output
stream. Forcing ALL user-facing communication through tool calls (prose inside
JSON args) risks degrading output quality, worst on weaker models — and Alfonso
routes cost-primary across a model spread, so any design must hold on the
*weakest model in the pool*, not the strongest.

**Resolution — hybrid (chat is text, structure is tools):**
- **Chat = normal assistant text.** The model writes prose exactly as today
  (full training distribution, reasoning+prose interleaving intact). The
  harness display lane already streams that text; the Board's chat channel
  *subscribes* to it. Chat needs **zero tool calls**; prose quality is
  untouched by construction. This also resolves the streaming seam (live prose
  streams as text, teed into the chat channel by the adapter; the record is the
  stream itself).
- **Structure = tool calls.** Asks (lifecycle objects), `show(tag)` (pointers),
  status / fork-diagrams / blocks — here the model *invokes a widget*, not
  writes prose, so a tool call fits with no quality cost.
- **Thinking = reasoning**, unchanged.

This satisfies "the user interacts only via the Board" fully. What it gives up
is the literal "raw history shows only thinking + tool calls" — chat prose stays
in history. That trade is correct: "only tools in history" was a *mechanism*,
not the goal, and it is the mechanism that carries the quality cost. The user
never sees raw history either way; they see the Board.

Free evidence for the hybrid: every harness we wrap (Claude Code, Codex,
OpenCode) keeps user-facing content as assistant text even though all are deeply
tool-driven — weak but real signal that prose-as-text is load-bearing.

**Empirical gate (still required):** any surface that carries prose inside tool
args must be validated on the multi-model bench — three arms (pure-text baseline
/ prose-in-tool-call / hybrid-text-routed) across frontier → cheap routing tier,
side-by-side qualitative read. Do not ship prose-in-tool-args on a strong-model
impression alone.

### 4.5 The anti-repeat mechanism (the hard part)
**Why the model repeats today:** it cannot *trust* that what it said is still in
front of the user. In a linear stream its statement gets buried under tool calls
and interruptions, so next turn it defensively re-states. The Board fixes the
*user's* side by construction, but repetition is the *model's* behavior — so the
fix must land in the *model's* context. The model needs persistence to be
**legible to it**, not merely true.

**Mechanism: reflect Board state back into the model's context** as stable,
prominent, tail-region state (see §4.6 read-stack). On turn N+5 the model can
look and see "my last message is still the live tail; ask #3 is still open"
without hunting buried history. This is exactly what the `ask` tool lacked.

Two load-bearing properties:
- **Addressing / narration-vs-message split.** Today models blur *narration*
  ("let me check X, now I'll run Y") with *addressed messages* (substantive
  content the user should retain); the blur is part of the repetition. The Board
  splits them: durable message → chat channel (pinned); narration → working-status
  lane (ephemeral) or dropped. For weak models the pin is **durable-by-default
  routing** — bare assistant text is retained without the model managing
  anything; narration is the *marked* exception. Failure mode degrades to "a bit
  noisy," never "message lost" or "compulsive repeat." Never make retention the
  thing a weak model must remember to do.
- **The guarantee survives compaction.** The deepest reason to stop repeating is
  that the **Board outlives the context window.** A model can have an old chat
  post compacted out of its own context and still trust the user sees it, because
  the Board is the durable user-facing record (same relationship MC memory has to
  the window). The state reflected back asserts exactly this: "the user's view
  includes everything you've posted, including posts no longer in your context."
  The mental-model shift is from "linear stream where I must repeat to stay
  visible" to "persistent surface that holds my words whether or not I still
  remember them."

**Empirical gate:** anti-repeat is the piece most likely to fail on weak models.
"Don't repeat, it's on the board" in a system prompt is exactly what weak models
ignore under tool-call noise. Whether durable-by-default routing + a prominent
tail state-block + "add what's new" framing actually suppresses repetition must
be *measured*: a conversation with heavy interleaved tool noise across turns, on
the cheap routing tier, scored for re-statement. Iterate on the state-block
shape before locking. (Same bench as §4.4, with an added repetition axis.)

### 4.6 The Board read-stack (three tiers, pointers-not-resolution)
The model reads Board state in three tiers, mirroring the pointers-not-full-
resolution pattern recursively:

1. **Text status-line — every turn, tens of tokens, UNIVERSAL (all models incl.
   text-only/weak).** The cheap live truth: "user's view current through post K,
   open asks: #3, 1 new since your last." Rides the m1 volatile tail,
   change-triggered (surfaces when state changes, like the AFT status bar).
   This is the FLOOR: the anti-repeat trust rides this on every model.
2. **Board-as-image — on-demand pull, ~1.2k tokens, VISION models.** The full
   Board rendered as one designed image for a whole-board glance. Pulled as a
   *tool result* when the model wants to orient, never every turn. Epoch-stamped
   ("board as of post K"); the model reads the cheap line (live) to decide
   whether to spend the image pull. Enrichment, never the floor — do not make a
   vision model pull 1.2k every turn just to check state.
3. **CRUD writes — the Board tool's richer params.** create ask / update block /
   post / resolve. Tool calls in the tail. **Write results stay terse** (ack +
   new anchor id); never "return the whole board" (that would reintroduce
   every-turn bloat). Writes ack, reads tier (line → image), state lives in the
   module.

**Authority property (why the line works):** the status-line and the image are
**adapter-authored ground truth the model did not write** — same reason the AFT
status bar is trusted. A self-reported "I already told them X" builds no trust;
an adapter-reported "user's view is current through post K" does. That authority
is what turns off the defensive re-state.

**Trust-loop closure for vision models:** the user glances at the rendered Board
in Cortex; a vision model glances at a rendered-to-image version of the *same
Board state*. They are looking at the same surface. So the model isn't trusting a
text assertion that the user saw X — it can *see the board the user sees*. This
is the strongest form of "already said, still shown," and it is why the image
tier is more than a token-saver.

**Cost model:** ~tens of tokens/turn (text line) + ~1.2k occasionally (image
pull when orienting). Text-only/weak models pay only the line.

---

## 5. Board blocks (visual expressiveness)

Ufuk's synthesis: freeform HTML mashed up with a contract — "we cannot foresee
every visual interaction," but the 95% path should be cheap/cache-stable.

Gradient (all driven by the same JSON placement shape):
- **Bundled core blocks** shipped in the app binary (status, task list, ask-card,
  table, chart, diff, markdown, image, progress). Onboarding cannot depend on a
  network fetch to render its own progress screen.
- **Library blocks** discovered semantically from an npm-like block repo:
  `block_search("comparison table with expandable rows")` → returns block id +
  a **props schema** (the schema is the contract; the agent never sees the
  block's internals) → agent emits `{block: "chart@2", slot: "b3", props: {...}}`.
  Token cost per placement is a search hit + a JSON object, not markup. This is
  the same discovery-on-demand shape as the MCP facade's `tools_search`/
  `tools_invoke`, and Synapse gives local embeddings for the semantic index.
- **Freeform block** = one wildcard template, sandboxed identically; the agent
  must *choose* it over a library block (search first, freeform as fallback), so
  it stays the 5% escape hatch. Encode this in the tool description (wording
  steers behavior).

Pins:
- **After fetch, the agent drives the block with JSON** (small props updates,
  cache-stable, no markup regeneration) — the 95% path gets contract economics
  while the repo solves the "can't foresee every interaction" problem without
  baking anything into the binary.
- **Version-pin placements.** A placement references `block@version`, resolved
  and cached at placement time. Old sessions keep rendering with the version they
  placed; new placements get the latest. (Same frozen-set instinct as everywhere.)
- **Blocks are code → trust surface.** HTML/CSS/JS executing in the user's app:
  in-house-signed-only at launch, sandboxed context, no network by default.
  Design the signing/trust story day one (third-party block publishing is a
  natural future ecosystem/paid lane — a block marketplace), so opening it later
  is a policy change, not a redesign.
- **The block region is a webview** inside the native app — the one scoped
  exception to native-everything (a natively-rendered block library is
  unwritable at reasonable cost). Native chrome, native chat, native sidebars,
  webview *canvas/blocks*.

---

## 6. Cache/wire invariants (subc/broca/MC seat)

These are permanent, not v1 conveniences:

- **Board state reflected to the model rides the m1 volatile tail, never the
  frozen prefix.** The status-line is a tail update; an image or block injected
  as ambient *prefix* context becomes byte-affecting state that grows with the
  session = whole-prompt cache bust every turn. Text line = change-triggered tail.
- **Visual context is always PULLED (tool result), never PUSHED (prefix).** A
  tool result lands in the tail as an immutable correlated unit under the
  tool_call/result pair, covered by the frozen-render-config epoch machinery
  already shipped (compaction/replay/decay treat it like any tool output). Any
  future "auto-orient" that wants the map without an explicit model call still
  routes through a tail tool-result-shaped injection with an epoch, never prefix.
- **Deterministic render** (same state in → byte-identical image out): required
  for BOTH measurement reproducibility AND tool-result cache hits on re-pull
  within a session.
- **Chat prose stays in the wire as structured tool traffic / assistant text
  that the model genuinely sees** — the Board is the RENDER of that traffic, never
  a replacement that strips it from context (which would make the model
  conversationally blind and turn compaction into a divergence problem).
- **Re-pulled map/board images carry a recognizable identity so a re-pull
  SUPERSEDES the prior one** rather than stacking N posters in the tail (MC's
  superseded-edit reclaim, same as repeated file edits).
- **CRUD write results stay terse** (ack + anchor), never full-board echoes.

---

## 7. Visual-context tier (#visual-context, Synapse-chaired)

Ufuk's framing: human memory stores POINTERS to moments that open to full
resolution on demand; give LLM context the same tier — a vague-but-complete
grand view at fixed low cost, with drill-down (search, file reads, compartment
expansion) as the resolution mechanism we already have. Build it VISUALLY for
vision-native models.

Token economics (Anthropic table): 1000×1000 image ≈ 1,296 tokens; 1568px long
edge ≈ 1,568; high-res tier caps ~2576px/4,784. A designed 1MP poster ≈ one
medium file read, replacing tens of thousands of serialized tokens.

VLM constraint: models read **text-in-image and coarse spatial structure**
(clusters, containment, relative size, color) well, but FAIL at fine
edge-following in dense graphs. So: **containment (treemaps) not hairballs**,
few explicit edges, labeled clusters.

Two first artifacts:
- **AFT codebase poster** — treemap by module containment (cell area = code size,
  shade = churn, badge glyphs = dead-code/diagnostics), plus TOP-K (≤~12)
  cross-module dependency arrows only (thick = call volume). Data already in the
  callgraph store + Tier-2 aggregates. Scoring questions: most-inbound-deps,
  what-X-depends-on-that-Y-doesn't, where-is-dead-code, tightest-coupled-pair,
  test-fraction.
- **MC session/memory map** — compartments on a time axis, importance = size,
  recency = brightness, episode_type = color region, each cell labeled with its
  ordinal range (the pointer → `ctx_expand`). Memory map: categories as treemap
  regions sized by count, cells labeled by `#id`.

Settled design (all seats):
- **`ctx_map`-as-tool-result** is the permanent cache-coherent shape (§6).
- **One renderer, N suppliers, one measurement harness.** Suppliers emit a
  **map-graph JSON contract**: `{nodes, containment, weights, labels, top-K
  edges, encodings}` + per-node **`{label, drill_down: {tool, args}}`** —
  pointer-as-structured-field, not parsed-from-label. Renderer is a pure function
  (map-graph JSON → PNG), deterministic. Lives as a **shared crate first**
  (commons-adjacent), NOT a Synapse module op on day 1 (rendering is CPU-trivial
  treemap work; no subc hop until cross-process access buys something); graduates
  into a Synapse surface if it grows model-dependent layout later.
- **drill_down targets resolve against LIVE tools** (AFT): the map carries the
  store generation it rendered from; pointers are path/name-shaped suggestions
  into live tools whose own freshness reporting is the contract — no
  pinned-snapshot machinery. Measurement scores "did the model fire the right
  drill_down" against the rendered generation.
- **Measurement-before-shipping** (Synapse's deliverable): render → question set
  generated from the source graph → score across the model tiers that will
  actually consume it (frontier AND cheap — a map only the expensive tier can
  read fails the weaker-agent rule).
- **Coherence constraint:** the Board's text state-line and this visual grand-view
  are ONE primitive (cheap legible state + drill-down resolution), rendered
  text-index vs image. Keep one mental contract across both.

SUBC owns the seat reviewing the tool-result injection shape when the first
prototype lands. Sequencing: no owner takes work until current lanes clear; the
first freed owner produces ONE hand-designed prototype poster (AFT treemap or MC
session map), quizzed across three model tiers — that single artifact converts
the room from design to evidence.

---

## 8. Architecture: module-backed, engine-agnostic

- All three surfaces (Board/canvas, chat, show) are **module-backed
  harness-agnostic surfaces** (a `ck-board`-style module owns the state; the app
  renders), NOT app-internal features. Then the same Board works from
  OpenCode-me, CC-via-MITM, and the Cortex app identically, and a phone app later
  is just another renderer of the same state.
- Because delivery is Cortex-side, **normalize at the Board adapter, not at the
  wire.** Each engine emits prose its own natural way (llm-runner native, CC/Codex/
  OpenCode via their stream/headless modes) and the adapter maps each into the
  Board. We never impose one output discipline across all engines.
- The Board state is a **durable relationship record co-equal with MC memory**,
  living module-side — which is what makes the §1.1 lock real (switch engines,
  keep the entire visible relationship, not just the memory behind it).

---

## 9. Open questions / next steps

- **Board module home.** Likely `ck-board` (or folded into alfonso-core, which
  already owns rooms). Decide before prototype.
- **Streaming seam confirmation.** Chat prose streams via the display lane teed
  into the chat channel; confirm the concrete wiring on the first engine adapter
  (llm-runner native is the easy case).
- **Empirical benches (two, possibly one combined):** (a) prose-quality across
  three arms × model spread for any prose-in-tool-arg surface; (b) anti-repeat
  under interleaved tool noise on the cheap tier. Neither ships on a
  strong-model impression.
- **Naming confirmation:** "Cortex" is the working name (strong candidate, not
  final).
- **Kickoff timing.** The full team branding/UI/UX meeting (agenda in note #547)
  was gated on "after other lanes finish." Several closed (engram design,
  plugin-hooks classes 1-2); timing is live.
- **First visual-context prototype** (§7) as the design→evidence converter.

---

## Provenance
- Ufuk design session 2026-07-08 (onboarding, three-lane interface, strategic
  lock, block gradient) — session msgs ~13575-13595.
- Ufuk design session 2026-07-12 (Board collapse, prose-quality blocker,
  anti-repeat mechanism, three-tier read-stack).
- #plugin-hooks channel (community plugin surface — separate doc:
  plugin-surface-v1.md, ALF-owned).
- #visual-context channel (Synapse-chaired, 2026-07-12) — visual grand-view tier.
- Note #547 = CK App kickoff meeting agenda (accumulating).
