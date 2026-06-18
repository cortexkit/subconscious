---
stepsCompleted: ["step-01-document-discovery", "step-02-prd-analysis", "step-03-epic-coverage-validation", "step-04-ux-alignment", "step-05-epic-quality-review", "step-06-final-assessment"]
inputDocuments:
  - "_bmad-output/planning-artifacts/prds/prd-subconscious-2026-06-17/prd.md"
  - "docs/subc-core-architecture.md"
  - "_bmad-output/planning-artifacts/epics.md"
  - "_bmad-output/planning-artifacts/prds/prd-subconscious-2026-06-17/.decision-log.md"
scope: "subc v1 — tool plane (modes 1 & 2), concurrency-first"
---

# Implementation Readiness Assessment Report

**Date:** 2026-06-18
**Project:** subconscious (subc)
**Scope:** v1 — tool plane

## Step 1: Document Inventory

| Type | File | Status |
|---|---|---|
| **PRD** | `prds/prd-subconscious-2026-06-17/prd.md` | ✅ found (whole, status: draft) |
| **Architecture** | `docs/subc-core-architecture.md` | ✅ found (in `docs/`, not planning-artifacts — pointed at explicitly) |
| **Epics & Stories** | `epics.md` | ✅ found (whole, workflow complete) |
| **UX Design** | — | ⚪ N/A (agent-facing infrastructure; CK-app/dashboard is mgmt-plane, out of v1) |
| **Decision log** | `prds/.../.decision-log.md` | ✅ found (full audit trail) |

**Duplicates:** none (no whole+sharded conflicts).
**Missing:** none for v1 scope. UX intentionally absent (no UI in v1).
**Note:** architecture lives in `docs/` by project convention (`project_knowledge` path); not a defect.


## Step 2: PRD Analysis

**Note on form:** `prd.md` is written goals-style (Problem → Vision → Goals & Success Metrics → ...), not as a numbered FR list — by design (collaborative/adapted mode). The canonical numbered requirements were **formalized in `epics.md` Step 1**, derived from `prd.md` + `docs/subc-core-architecture.md`. That inventory is the traceability baseline below.

### Functional Requirements (19)
FR1 daemon+UDS · FR2 supervise AFT (spawn/health/restart/drain + hot-swap) · FR3 one AFT/root shared across instances · FR4 HELLO handshake · FR5 channel multiplexing/routing · FR6 17-byte envelope + splice routing · FR7 concurrent in-flight + out-of-order · FR8 CANCEL · FR9 per-channel flow-control window · FR10 liveness/status from cache · FR11 PUSH forwarding · FR12 manifest registration (tool_provider) · FR13 mode-1 MCP facade · FR14 mode-2 thin plugin (hoisting) · FR15 dual-scope identity (session/project) · FR16 honor declared concurrency · FR17 nested child-process supervision · FR18 envelope version negotiation · FR19 graceful standalone fallback.

### Non-Functional Requirements (10)
NFR1 concurrency (headline) · NFR2 mutation correctness · NFR3 zero regression · NFR4 thin-core · NFR5 latency bound · NFR6 resource/dedup (secondary) · NFR7 hot-update 0-drop · NFR8 standalone (discovered upgrade) · NFR9 ≥1 harness beyond OC/Pi · NFR10 embedding flip/flop eliminated.

### Additional Requirements / Constraints
Shared `subc-protocol` Rust types crate · 17-byte little-endian envelope with frozen-prefix versioning · store-and-route-without-interpreting (config provenance) · path-identity unification (canonical ProjectRootId, worktree first-class) · per-user tenancy (per-user socket, race-free singleton) · AFT two-leg concurrency migration (Leg 1 transport+router, Leg 2 within-root executor — cross-team, Leg 2 = felt-headline critical path).

### PRD Completeness Assessment
Complete and internally consistent for v1 scope. Requirements are concurrency-first (NFR1 headline), with the secondary/deferred items (dedup, mgmt plane, proxy plane, dreamer) explicitly out of v1. Honest tiering of the concurrency deliverable (contract + cross-session in v1; within-root Leg-2-gated) is recorded. No UX requirements (agent-facing infrastructure). The only structural note: numbered requirements live in `epics.md`, not `prd.md` — acceptable given the adapted authoring mode.


## Step 3: Epic Coverage Validation

### Coverage Matrix

| FR | Requirement | Epic / Story | Status |
|---|---|---|---|
| FR1 | daemon + UDS | Epic 1 / 1.0 | ✓ |
| FR2 | supervise AFT (spawn/health/restart/drain + hot-swap) | 1.5 + 4.1 | ✓ |
| FR3 | one AFT/root shared across instances | 1.0 + 1.10 | ✓ |
| FR4 | HELLO handshake | 1.6 | ✓ |
| FR5 | channel multiplexing/routing | 1.4 | ✓ |
| FR6 | 17-byte envelope + splice routing | 1.3 + 1.4 | ✓ |
| FR7 | concurrent in-flight + out-of-order | 2.1 | ✓ |
| FR8 | CANCEL | 2.3 | ✓ |
| FR9 | per-channel flow-control window | 2.4 | ✓ |
| FR10 | liveness/status from cache | 2.5 | ✓ |
| FR11 | PUSH forwarding | 1.8 | ✓ |
| FR12 | manifest registration (tool_provider) | 1.6 | ✓ |
| FR13 | mode-1 MCP facade | 3.1 | ✓ |
| FR14 | mode-2 thin plugin (hoisting) | 1.8b + 1.9 | ✓ |
| FR15 | dual-scope identity (session/project) | 1.7 | ✓ |
| FR16 | honor declared concurrency | 2.1 | ✓ |
| FR17 | nested child-process supervision | 1.5 + 4.2 | ✓ |
| FR18 | envelope version negotiation | 1.6 | ✓ |
| FR19 | graceful standalone fallback | 1.2 + 4.3 | ✓ |

### Missing Requirements
None. All 19 FRs have a traceable story home. The two coverage holes the Athena council found (daemon bootstrap → FR1; harness-facing TS plugin client → FR14) are now closed by Stories 1.0 and 1.8b.

### Coverage Statistics
- Total PRD/inventory FRs: **19**
- FRs covered in epics: **19**
- Coverage: **100%**
- NFRs: all 10 mapped (NFR1→2.2/2.6, NFR2→2.x, NFR3→1.9, NFR4→cross-cutting, NFR5→1.11, NFR6→1.10/3.x, NFR7→4.1, NFR8→1.2/4.3, NFR9→3.2, NFR10→1.10).
- Reverse check (epics→PRD): no orphan stories implementing un-specified FRs; the addendum ACs all trace to an inventory FR/NFR.


## Step 4: UX Alignment Assessment

### UX Document Status
**Not Found — and correctly so (not a warning).**

### Is UX implied?
No, for v1. subc v1 is **agent/harness-facing infrastructure**: its "users" are the AFT module and the harnesses that connect over a socket. There is no human-facing UI surface in scope — no web, mobile, or desktop component. The PRD explicitly scopes the human-facing surfaces (the CortexKit app, the MC dashboard) to the **mgmt plane**, which is near-term but **out of v1**.

### Alignment Issues
None. The architecture (`docs/subc-core-architecture.md`) does account for the future human-facing surface (the mgmt plane: management-surface providers + the CK app as a mgmt-plane consumer, §3 + §4) — so when UX *does* arrive (the CK app), the architecture already has the seam. No architectural gap for deferred UX.

### Warnings
None. Missing UX is intentional and scope-correct, not an oversight.


## Step 5: Epic Quality Review

### User-value focus (per epic)
- **Epic 1** "AFT through subc + cross-instance dedup" — ✓ closes on a user sentence ("two harnesses stop duplicating my index"), not a technical milestone.
- **Epic 2** "Concurrent tool execution" — ✓ user-felt ("quick reads don't stall behind a slow call").
- **Epic 3** "Any MCP harness" — ✓ ("a harness we never wrote a plugin for now has full AFT").
- **Epic 4** "Live updates & standalone resilience" — ✓ (updates without restart; never breaks when daemon absent).
No "Setup DB / API layer / Infrastructure" technical-milestone epics.

### Epic independence
- Epic 2 builds on Epic 1; Epic 3 on 1+2; Epic 4 on the rest. No epic requires a *later* epic. ✓

### Story quality & dependencies
- Stories are single-dev-sized with Given/When/Then ACs and FR references; error/edge conditions present (the hardening pass added the negative ACs: version floor, cancel race, no-starvation, two-writer, etc.).
- **No forward dependencies** within any epic. The only external gates are explicit cross-track refs (1.9→AFT Leg 1; 2.6→AFT Leg 2) — *backward/external*, not forward, and isolated by the fake-AFT stub.
- Greenfield Rust daemon: Story Zero (1.1) is the contract-freeze, not a scaffold; no upfront "create all tables" — state is introduced per-story (identity 1.7). ✓

### Best-practices checklist (all epics)
- [x] User value · [x] Independent · [x] Stories sized · [x] No forward deps · [x] State created when needed · [x] Clear ACs · [x] FR traceability.

### Findings by severity
**🔴 Critical:** none.
**🟠 Major:** none. (One *process* item, not a defect: the Hardening + Council ACs currently live in addendum sections and must be merged into their parent stories at the story-file stage — already tracked.)
**🟡 Minor:**
- Story numbering has insertions (1.8b inserted before 1.9; 1.0 prepended) from the council additions — renumber to a clean monotonic sequence at the story-file stage.
- Numbered requirements live in `epics.md` rather than `prd.md` (adapted authoring mode) — acceptable, noted.

### Verdict
Epic/story structure passes the quality bar with **zero critical or major defects**. Remaining items are cosmetic/process (addendum merge + renumber), deferred to the story-file stage.


## Summary and Recommendations

### Overall Readiness Status
**READY** — for v1 (tool plane), with two cosmetic/process follow-ups and two tracked cross-track coordination items, none blocking the start of implementation.

### Critical Issues Requiring Immediate Action
None. 100% FR coverage, zero critical/major epic-quality defects, no missing docs for scope, no UX gap (none implied). The two coverage holes a prior council review found (daemon bootstrap, harness-facing TS plugin) are already closed (Stories 1.0, 1.8b).

### Recommended Next Steps
1. **At the story-file stage (before/with Sprint Planning):** merge the Hardening + Council addendum ACs into their parent stories, and renumber to a clean monotonic sequence (absorb 1.0 and 1.8b).
2. **Story Zero is the true first build unit** — it freezes the subc⟷AFT contract (shared `subc-protocol` crate) and folds in Spike A (recorded real-AFT trace from today's NDJSON). Decide the crate's repo home and the per-user tenancy details here.
3. **Coordinate AFT Leg 1 / Leg 2 timing** — Leg 2 is the felt-headline critical path; gate Story 2.6 (within-root metric) on its sizing, but ship subc's Epic 2 (contract + cross-session) independently.

### Cross-track coordination (tracked, non-blocking)
- AFT Leg 1 (transport→socket + per-root router) — gates only Story 1.9.
- AFT Leg 2 (within-root reader-writer executor) — gates only Story 2.6; the felt-headline critical path. Substrate-conversion inventory is the sizing input (deferred to AFT v0.39.4).

### Final Note
This assessment found **0 critical and 0 major issues** across document discovery, PRD analysis, FR coverage (19/19), UX alignment, and epic quality. Remaining items are cosmetic (addendum merge + renumber) or external coordination (AFT legs). The plan is implementation-ready; first build unit = Story Zero → Spike A → Epic 1. The depth of prior review (roundtable + inversion/assumption + 7-model council) is the reason this gate is clean.

**Assessor:** Alfonso · **Date:** 2026-06-18
