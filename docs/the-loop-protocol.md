# THE LOOP — continuous verified find-fix-gate cycle

A repeating, budget-heavy hardening loop for a repo Alfonso owns. Two report-only
masons hunt in parallel (one performance, one correctness), Alfonso verifies every
finding at source before authorizing any fix, then merges and gates each round and
starts again. Findings never get fixed on the mason's say-so; the verification gate
is the whole point.

Invoke by pointing Alfonso at this file: "run THE LOOP on <repo>".

This is the shared copy. It lives in tracked `docs/` rather than under
`.cortexkit/`, which is gitignored fleet-wide -- a protocol only one machine can
read is not a protocol. Companion reading: `docs/hunting-loop-briefing.md`, which
is the accumulated method for the verification gate (what makes a finding
confirmable, what makes a green result untrustworthy, and the instrument failures
that produce confident wrong answers).

---

## Roles

- **Perf mason** — finds ONE single highest-confidence performance issue. Report only.
- **Bug mason** — finds ONE single highest-confidence correctness bug. Report only.
- **Alfonso (orchestrator)** — verifies, rejects or authorizes, merges, gates, keeps
  the ledger, escalates design forks to the user. Never delegates the verification.

Both masons: `iq=10`, isolated worktree off current `master`, report-first (NO code
change on the first turn), exact `file:line` citations, and **no subagents** (each
mason does its own investigation and fix directly, never delegating).

Launch mechanics (learned the hard way): write mason prompts to
`.cortexkit/alfonso/prompts/<name>.md` and pass `prompt_file` — inline prompts die
on dirty-parent rejections and the file survives to relaunch. Keep EVERY path in a
prompt relative to the worktree; an absolute path once sent a mason out of its
worktree to commit directly on local master.

---

## Round protocol

1. **Launch** perf mason + bug mason in parallel, report-only, seeded with the
   current ledger (§ Ledger) so neither re-surfaces an already found/fixed/rejected
   item.
2. **Collect** both reports. Each finding must carry: exact `file:line`, the mechanism
   (why it is real, traced not guessed), the proposed fix shape, blast radius, and a
   self-assessed confidence.
3. **Verify at source (gate A).** Alfonso independently traces each finding.
   - Confirmed real → authorize fix.
   - Not confirmable to 100% → **reject** back to that mason with the reasoning; it
     hunts again. Never fix an unconfirmed finding.
   - Real finding but the fix is a **behavior/architecture decision** (semantics
     change, perf/correctness tradeoff, wire/contract change) → **escalate to the
     user**, do not auto-fix.
   - **Residual-doubt Oracle rule (Ufuk-directed):** if Gate A (or Gate B) leaves ANY
     residual doubt — especially concurrency / epoch-fence / cache-stability / unsafe /
     multi-path invariant changes — fire an independent Oracle pass for one more check
     before authorizing. For a delicate fix already merged, fire an Oracle **backstop**
     and **fix-forward** if it finds a hole. Oracle is read-only + durable, so the loop
     keeps moving while it checks; never block a round waiting on a backstop.
4. **Collision check.** If both confirmed findings touch the same file/area, serialize
   the fix+merge (fix A, merge, rebase B, re-gate). Disjoint → fix in parallel.
5. **Fix.** `background_prompt` the SAME mason (keeps worktree + context) with the
   authorized fix instruction: minimal behavior-preserving change + a non-vacuous
   regression test that fails without the fix. A report-turn probe (the script or
   harness the mason built to demonstrate the finding) should become the fix
   turn's committed regression where the shapes align — the report artifact is
   already the finding's executable statement, and reusing it is a quiet economy
   of the same-mason design (observed on external rounds: probe adopted verbatim
   as the regression).
6. **Review diff (gate B).** Alfonso reads the fix diff for correctness AND regression
   before merge. Wrong fix on a right diagnosis gets bounced. For PERF fixes,
   gate B additionally requires a MEASURED before/after on the mechanism's own
   quantity (p50 latency, allocation count, bytes) — a mechanism without a
   number stays a claim, and the number is what makes the ledger line
   trustworthy later.
7. **Merge + gate.** Merge to master. Gate = full workspace tests green + clippy
   native + clippy `x86_64-pc-windows-gnu` + `cargo fmt --check` + `check_comments`
   on the diff, then push (CI confirms ubuntu + windows; there is no macOS runner,
   so a macOS-only fault is not covered). For TS/Swift touched: their suites +
   typecheck too.
8. **Update ledger.** Append found/fixed/rejected/escalated entries with SHAs.
9. **Finalize** both mason tasks (accept with scores, or reject with reasons).
10. **Loop** back to step 1 with the updated ledger.

Report one line per round: `round N: perf[verified→SHA|rejected|escalated] · bug[…]`.
Do not pause to ask between rounds; run continuously.

---

## Hard rules

- **Prod is untouched.** The loop produces gated master commits only. Deploying any
  built artifact to the live fleet is a SEPARATE explicit user-gated window, never
  part of a round.
- **Scope = the owned repo only.** A finding in a peer-owned repo is routed to that
  owner as a report, never fixed here.
- **No masking.** No suppressed type errors, no deleted/weakened tests to pass, no
  timeout bumps as a "perf fix." A perf claim needs a mechanism, not a vibe.
- **Verification is non-delegable.** Alfonso traces every finding personally. The
  masons' confidence is an input, never the authorization.
- **Escalate design forks.** Anything that changes observable behavior or a contract
  stops the loop for that item and goes to the user.
- **Fence instructions carry an escape hatch.** Any instruction that fences behavior
  ("do not change X", "tests only") MUST state: reporting something as WRONG counts
  as SUCCESS, and is the default whenever the mason cannot positively justify what it
  found. Without that clause, a tests-only mason reaching a fail-open defect writes a
  PASSING test certifying the bug — it happened live, and harm scales with the
  mason's thoroughness.
- **Regression tests assert the EFFECT, not the verdict.** Proving a guard says no
  proves nothing about whether anyone listens; the test must assert the guarded
  effect did not happen (no attach event, zero rows written, byte-identical output).
  Three positions exist — guard unasserted, verdict asserted, effect asserted — and
  only the third fences.

---

## Stop conditions

- Both masons return no high-confidence finding two rounds running (diminishing
  returns) → report and hold for the user.
- An escalation fires → surface it, keep the other lane moving if independent.
- The user calls it.

---

## Ledger

Running record re-fed into every launch so masons never repeat. Kept in
`.cortexkit/alfonso/loop-ledger.md`. Entry shape:

```
[R<round>] <perf|bug> <FIXED sha|REJECTED|ESCALATED> — <file:line> — <one-line what>
```

---

## Mason prompt template — PERF (report-only turn)

```
Repo: <repo path>. You have an isolated worktree off master.

TASK: Find exactly ONE — the single highest-confidence — PERFORMANCE issue in this
codebase. REPORT ONLY. Make NO code changes this turn; the worktree stays clean.
Do NOT spawn or use any subagents — do all investigation yourself with direct tools.

What counts: a real, source-traceable inefficiency on a path that matters — hot-path
allocation/clone, needless O(n) or O(store) work under a lock, lock held across
expensive/async work, redundant serialization, per-call work that should be cached,
syscall/IO amplification, unbounded buffering. NOT micro-nits with no measurable
effect, NOT style, NOT speculative "might be slow."

Rigor bar: trace the mechanism. Show the call path that reaches the hot code and why
it is hot (frequency × cost). If you cannot show it is actually on a frequent/hot
path, it does not qualify — find a better one.

OUTPUT (exactly this shape):
- TITLE: one line
- LOCATION: file:line (exact)
- MECHANISM: how it is reached and why it is expensive (traced, not guessed)
- COST: what scales it (n, store size, frequency, contention)
- FIX SHAPE: the minimal change you would make (do not make it)
- BLAST RADIUS: what else touches this code
- CONFIDENCE: 0-100 with why

ALREADY COVERED (do not re-report any of these):
<ledger contents>

Pick the one you are most sure is real and impactful. One finding. Report and stop.
```

## Mason prompt template — BUG (report-only turn)

```
Repo: <repo path>. You have an isolated worktree off master.

TASK: Find exactly ONE — the single highest-confidence — CORRECTNESS BUG in this
codebase. REPORT ONLY. Make NO code changes this turn; the worktree stays clean.
Do NOT spawn or use any subagents — do all investigation yourself with direct tools.

What counts: a real defect — wrong result, race/TOCTOU, missed error path, resource
leak, incorrect state transition, panic/unwrap on reachable input, off-by-one,
lost/duplicated work, a broken invariant. Must be reachable by real execution, not a
theoretical "if someone called this wrong." NOT style, NOT missing-feature, NOT a
test-only issue.

Rigor bar: trace the exact execution that produces the wrong behavior. Name the
input/interleaving that triggers it and the observable wrong outcome. If you cannot
show a reachable trigger, it does not qualify — find a better one.

OUTPUT (exactly this shape):
- TITLE: one line
- LOCATION: file:line (exact)
- TRIGGER: the input/interleaving/sequence that reaches the bug
- WRONG BEHAVIOR: what actually happens vs what should
- MECHANISM: the source-level why (traced)
- FIX SHAPE: the minimal change you would make (do not make it)
- BLAST RADIUS: what else touches this code
- CONFIDENCE: 0-100 with why

ALREADY COVERED (do not re-report any of these):
<ledger contents>

Pick the one you are most sure is real. One finding. Report and stop.
```

## Fix-reprompt template (after Alfonso verifies)

```
Your finding is VERIFIED at source. Authorized to fix now, in this worktree.

Constraints:
- Minimal, behavior-preserving change that addresses the ROOT mechanism you found.
- Add a non-vacuous regression test that FAILS without your fix and passes with it
  (prove it fails first). No vacuous asserts.
- No unrelated churn, no reformat-the-file, no scope creep beyond the finding.
- Do NOT spawn or use any subagents — do the fix and verification yourself.
- Green bar before you hand back: the package's tests + clippy (native) clean, AND
  run the repo formatter so master's gate does not bounce on style — for Rust run
  `cargo fmt --all` (master's rustfmt is authoritative; hand-formatting has skewed
  from it before), for the TS client the package's format/lint script.
- Rust + `#[cfg]`-gated code: if your change touches or adds any `#[cfg(unix)]` /
  `#[cfg(windows)]`-gated item, flag it prominently in your handback ("touches
  cfg-gated code") so the merge gate's windows-gnu clippy pass gets extra
  attention. The recurring CI-killer (3 occurrences across repos): a unix-gated
  USE with an unconditional IMPORT passes every unix-local gate and fails Windows
  CI on `-D warnings` (unused import). Do NOT run the windows-gnu cross-compile
  yourself in the worktree — per-worktree target dirs make it a cold ~20min
  build; the orchestrator's merge gate runs it on a warm cache in seconds.
- Commit with a message naming the mechanism, not the symptom. Then report the SHA
  and the before/after of your regression test.
```
