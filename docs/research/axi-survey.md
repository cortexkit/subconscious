# AXI survey — what the fleet should borrow, and what we already do

Source: https://axi.md/ (10 principles + 490-run browser benchmark + 425-run GitHub benchmark),
its catalog of ~30 domain CLIs, and the two published studies. Surveyed 2026-08-16 on Ufuk's ask.
Verdicts are per-principle against the fleet's actual surfaces, with owners named. The headline
result (a principled CLI beats raw CLI AND every MCP variant on success/cost/turns simultaneously;
Atlassian's MCP-Compressor corroborates CLI-over-MCP independently) is directionally credible —
but read the caveats: one model, read-only tasks, LLM judge, and the margin over the runner-up is
5% on cost, so the value is in the PRINCIPLES, not the protocol religion.

## Scorecard against the fleet

| # | AXI principle | Fleet state | Verdict |
|---|---|---|---|
| 1 | TOON output (~40% token savings) | JSON/tables everywhere | **DECLINE for now** — see below |
| 2 | Minimal default schemas (3-4 fields) | Mixed; `ck` mostly lean, some `--json` dumps | Adopt as audit rule |
| 3 | Truncation with size hints + `--full` | AFT does it; `ck stderr` caps without hints | **BORROW: the hint half** |
| 4 | Pre-computed aggregates (`totalCount`) | Banked doctrine (denominators, absence counts) | Already ours; audit `ck` for gaps |
| 5 | Definitive empty states | Banked doctrine (empty-vs-failed, vacuity family) | **Already ours** — they converged on our rule |
| 6 | Structured errors, exit codes, fail-loud unknown flags | Shipped (`ck` exit 2 on unknown flags; ErrorBody{code,detail}) | Already ours; one deviation noted below |
| 7 | Ambient context (session-start dashboard) | Unified status line (prefrontal-composed + AFT) | **Already ours, independently validated** |
| 8 | Content first (no-args shows live data) | Bare `ck` prints help/domain list | **BORROW — best single item** |
| 9 | Contextual disclosure (`help[]` next-step lines) | Sporadic (`ck health` footer does it) | **BORROW: systematize** |
| 10 | Consistent `--help` | Shipped (help-before-verbs, tail parsing) | Already ours |

## The three real borrows

**(8) Content-first `ck`.** Bare `ck` currently prints the domain list — help text, exactly the
anti-pattern their benchmark punishes. A bare `ck` that printed a compact live dashboard (daemon
version/uptime/connection count, per-module one-line health, alarm count) would make the single
most common agent invocation productive instead of navigational. `ck health` already computes
most of it; this is a composition change, not new data. Their detail worth copying verbatim: the
home view names the executable path (`bin: ~/.local/bin/ck`) — self-identifying output kills the
stale-PATH-copy class we have hit twice.

**(9) `help[]` footers — AMENDED after AFT's production correction.** AFT measured blanket
footers as a real token tax in their June cost arc and trimmed them; what survived their
measurement is ERROR-PATH steering (nearest-miss candidates, unknown-id redirection, zero-result
escalation) — which is where the tool-discovery bleed actually lives in axi's own trajectory data.
The `ck` borrow narrows accordingly: footers on error and empty arms plus the two navigational
surfaces (bare `ck`, `module list`), NOT on routine success outputs — blanket success-path footers
re-inflate a measured cut.

**(3) Truncation hints.** Where we cap (stderr tail, transcript pages), append the size fact:
`(truncated, N lines total — use -n <count>)`. We cap correctly; we do not consistently SAY we
capped and how to get the rest. An uncommunicated cap reads as a complete result — the same
honesty class as our definitive-empty-states rule, applied to the other end.

## Declines, with reasons recorded (and reversal conditions)

**(1) TOON.** Three reasons. (a) Ufuk's standing preference: outputs easy for WEAKER agents to
parse, familiar raw formats over denser abstractions — TOON is a denser abstraction, and their own
caveat concedes format sensitivity is single-model-evaluated. (b) Their runner-up (dev-browser,
plain output) was within 5% on cost — the wins came mostly from combined ops and aggregates, not
the serialization. (c) Our biggest token surfaces (AFT tool outputs, MC transforms) already have
owners optimizing them with production trace evidence rather than benchmark priors — and AFT's
evidence runs the OTHER way: 84k-call telemetry showed weak-model format brittleness at ~100x the
auto rate on a format-steering parameter, which they removed in August.
REVERSAL CONDITION: a multi-model measurement on OUR traces showing >15% end-to-end savings on a
top-cost surface.

**(protocol switch).** We are not converting MCP surfaces to CLIs. Our facade already implements
what their benchmark says matters (surface_mode search, ack_only, narrowing, minimal schemas), and
the finding we take instead is their Finding-3 mechanism: schema overhead and per-action snapshot
round-trips are the MCP tax — both addressable inside a facade.

## Fleet pointers (routed to owners)

- **AFT** owns the largest agent-facing output surface and has the tooling to A/B on real traces:
  principles 2/3/9 as an audit lens; TOON evaluation is theirs to decline or measure.
- **CEREB**: their Case-A/C mechanism (combined operations: act-then-observe in ONE call —
  `click --query` returns the post-action snapshot; query filtering over full snapshots) is the
  strongest external validation yet for capture-attached-to-actuation and addressable-element
  queries over dump-everything. Also quota-axi exists in their catalog — insula's domain, weaker
  than ours (local-first file reads, no daemon), nothing to take.
- **ALF/BROCA**: principle 7 is the unified status line already being built — outside validation,
  no change.
- **PLEX**: gws/slack/notion-axi are prior art for connector ERGONOMICS (multi-account
  write-safety: "drafts mail, never sends" as a structural posture) — worth a read when those
  connectors build, not a dependency.
- **subc (this repo)**: the three borrows above, all in `ck`.

## What they got from us for free

Principles 4/5/6 are our banked doctrine arrived at independently (denominators, empty-vs-failed,
fail-loud unknown flags) — convergent evolution is evidence both sides derived from the same
failure classes. Their principle 6 puts errors on STDOUT (agents read stdout); our convention
keeps stderr for diagnostics. Ours is compatible with harness capture and preserves the
structured-data-only stdout rule they themselves state — no change.
