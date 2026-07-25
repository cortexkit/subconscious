# Briefing a hunting loop

A hunting loop points a worker at a subsystem and asks for one defect, with no
code changes. This is how to aim it. Everything here is backed by one loop that
ran sixteen rounds and shipped twenty-five defects, so the ordering below is
measured rather than argued: the same lane and the same model produced four
times the consequence per finding once the targeting changed.

Authored by the ai-provider-quota seat; kept here because it is fleet-wide
method, not one module's story. That repo's `docs/provider-invariants.md` is the
worked example of what a loop leaves behind.

## The targeting principle

**The value of a hunting round is inversely proportional to how observable the
defect class is from production.**

Parsers, arithmetic and mapping have a second line of defence — eventually a
human notices a number that looks wrong. A state transition that overwrites its
own evidence has none, so reading the transition is not the best available
method, it is the only one.

The measurement behind that: rounds 1-10 went at parsers and transport and
produced volume, but four of six findings were repeats of two classes. Rounds
11-16 went at identity, scheduling, dedup, fencing and routing — fewer findings,
each one invisible from the consumer boundary.

## Order to brief them in

1. **Identity, and whatever gates multi-tenancy.** The worst available defect in
   most modules is serving one tenant's data under another's credential, and it
   is undetectable downstream. Ask specifically what happens when the compared
   identifier is *absent on both sides*: missing compares equal to missing, so
   the fence fails toward "same", which is the permissive answer.
2. **Anything that grants a claim rather than refusing one.** A refusal is an
   outcome someone writes a test for. A *forfeiture* — the action proceeds, just
   at a weaker claim — reads as a happy path, so nobody writes a rejection
   vector for it. Measured instance: deleting a refusal reddened two named tests
   across two binaries, while mutating two forfeiture paths in the same file
   left everything green. Where the claim is a safety claim, an unearned one
   writes a record asserting something the system did not achieve, which is a
   quiet lie rather than a crash.

   Two follow-ups, both learned by nearly missing them. A grant applied at
   several sites needs a guard at **each**, and a test on one site says nothing
   about the other — the moment you find one, you will be tempted to guard it
   once and call the class closed. And check *what* the nearest existing guard
   asserts: one that checks an eligibility flag is not set never establishes
   that an unset flag is not honoured, which is the upstream condition rather
   than the gate. In the measured case, deleting the real gate reddened only
   three tests named for something else entirely — defended by accident, and one
   refactor of those unrelated tests away from being undefended.
3. **The state machine and its fences.** Incarnation/ABA, attempt ordering,
   admission fairness, what a timeout does to a prior observation. Demand the
   exact interleaving — "these two race" is not a finding.
4. **Whatever decides which data headlines.** There are two failure directions,
   and the obvious fix for one produces the other.
5. **Transport and resource bounds.** Cheap to check, real when found.
6. **Parsers last**, justified by being cheap rather than productive.

## Four brief rules that each changed the output measurably

- **State the closed seams explicitly.** Once a class is swept, name it and its
  cleared members. A lane with an open seam keeps returning instances of your
  own last fix; the first round with nothing left to mine found a real identity
  defect immediately.
- **Demand measurement before magnitude.** Two rounds inflated tens of
  microseconds into a latency problem. After the brief said so with real
  numbers, the next round measured a hundred runs, reported the aggregate as
  small, and argued from a contiguous stall instead. Same lane, scored 74 then 93.
- **Say an honest null is acceptable, and mean it.** Two nulls scored 90 and 92;
  one benchmarked the change it was tempted by and declined to report a 15.6µs
  saving. A lane that believes it must produce will produce speculative hardening.
- **On a sweep, make the negative half the deliverable.** "Name every member and
  say why each is cleared" turns a null into an auditable artifact. It also
  catches operator error: a brief said 34 providers and the sweep said 35, and
  listed them. Take the population count from the artifact, never from the brief
  — a sweep that adopts your count cannot find the member you missed.

## The cost-asymmetry gate

This killed two proposed guards and justified one. Wrongly rejecting a good
response turns a working provider into a broken one; wrongly accepting a
questionable one costs at most one stale read.

**When both sides are bounded, decline the guard. When one side is unbounded and
silent, take it.** A guard whose correctness depends on a value unmeasurable
from the running machine must not ship.

## Verification is non-delegable

**Five of eighteen proposed fix shapes were wrong as written.** Finding quality
ran far ahead of fix-shape quality for the entire loop. One report's finding was
real while its stated *mechanism* was wrong — caught only because the file
contradicted itself and the report had quoted the half that supported its case.
An auto-applying version of this loop would have shipped a worse bug in round 2.

After three rounds where fix authorization could not reach a dying worker, the
lane became report-only by design and the operator wrote every fix. That removed
a failure mode and cost nothing, because verification was already theirs.

## Pre-commit the stop condition

Write it down before the round runs. A stop rule authored after the result is a
rationalisation with a timestamp.

State it as a property of the *result*, not a round count: "if this returns a
speculative hardening proposal rather than a triggerable defect, stop." A good
one binds in both directions — that rule would have ended the loop on a weak
round, and it also refused to allow stopping on a real defect when stopping
would have been the tidier ending.
