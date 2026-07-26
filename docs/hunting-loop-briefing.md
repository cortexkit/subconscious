# Briefing a hunting loop

A hunting loop points a worker at a subsystem and asks for one defect, with no
code changes. This is how to aim it. Everything here is backed by one loop that
ran sixteen rounds and shipped twenty-five defects, so the ordering below is
measured rather than argued: the same lane and the same model produced four
times the consequence per finding once the targeting changed.

Authored by the ai-provider-quota seat; kept here because it is fleet-wide
method, not one module's story. That repo's `docs/provider-invariants.md` is the
worked example of what a loop leaves behind.

## The checklist

Everything below this table is the reasoning. This is the part you run down. A
document that says "check for X" depends on the reader already suspecting X; a
document that lists the X's does not. **If you have to bring the suspicion, it
is a warning, not an instrument.**

Two things about building one of these, learned by getting both wrong here.
**A list forces you to enumerate the population, and enumerating the population
is how you discover a missing member** — building this table is what revealed
that the sweep-the-property rule had no section to point at, which no amount of
re-reading the sections could have surfaced. And **a table transcribed from
memory is prose with borders**: it inherits the errors of the recollection that
produced it while looking more authoritative than the prose did. Two rows here
pointed at nothing on the first pass. A reader seeing a table assumes someone
went and looked, so the format launders unverified recall into apparent
verification — which makes re-deriving every row from source a precondition for
the format being honest, not diligence on top of it.

Why any of this works: **review is good at inspecting what is present and
structurally blind to what is not there.** A reviewer reads a diff, and an
absence has no diff. Every defect that survived review on the day this document
was assembled was an absence — an unasserted guard, an unrendered screen, an
unowned transition, an uncounted member, a contract nobody had written down. So
the instruments that find them enumerate a population rather than examine an
artifact.

Before briefing a round:

| # | Check | Where it fails |
|---|-------|----------------|
| 1 | Are the closed seams named, with their cleared members listed? | A lane with an open seam returns your own last fix |
| 2 | Is the target ordered by how *unobservable* the class is, not by where defects are easy to find? | Parsers produce volume and repeat two classes |
| 3 | Does the brief say an honest null is acceptable? | A lane that must produce will produce speculative hardening |
| 4 | On a sweep, is the negative half (every member, why cleared) a deliverable? | A null becomes unauditable, and your population count goes unchallenged |
| 5 | If the prompt asks a worker to *fence* anything, does it grant an explicit refusal path — with reporting a wrong mechanism counted as success? | The sweep hardens the defects it was sent to find |

Before believing a green result:

| # | Check | Where it fails |
|---|-------|----------------|
| 6 | Can any fixture in the suite *construct* the defective input? | Ten honest assertions, all blind in one direction |
| 7 | Is the condition tested **at this site**, not merely tested? | The named test exists, attached to the other branch |
| 8 | For a guard: does a test assert the prevented effect *did not happen* — not that an error came back? | A mutant that acts and then reports correctly passes |
| 9 | For a fix that removes, reclaims or refuses: would the tests pass if it did that to *everything*? | The suite cannot tell the fix from its unbounded version |
| 10 | When a mutation reddens something, which test died? | Three tests named for other things means defended by accident |
| 10a | When a probe is *expected* to fail, did the failing phase's label appear in the output? | A timeout, a crash and a genuine finding all exit nonzero |
| 10b | Before writing off a measurement as contaminated, do its neighbours agree? | An aggregate caveat silently retires a true finding |
| 10c | For a causal claim: did you run the control arm, with only the one variable differing? | An uncontrolled measurement is an observation, not an attribution |
| 10d | Does your "independent" check differ in *premise*, or only in whose hands ran it? | Re-running someone's query is a transcription check |
| 10e | Does the query you ran answer the question you are asking of it? | One relation's answer quietly supporting a claim about another |
| 11 | Did the new CI step *execute*, or was the run cancelled before reaching it? | A green list showing passes for runs that never ran the new logic |

Before calling a class closed:

| # | Check | Where it fails |
|---|-------|----------------|
| 12 | Did you sweep the *property*, or re-check the instance that revealed it? | The second guard of a pair stays undefended |
| 13 | For each branch pair (simple case / full case): does every condition in one have a counterpart in the other? | The richer path carries the consequences and the thinner test |
| 14 | Does a condition that suppresses output also suppress the thing that would report the suppression? | Alarm and backstop removed by one condition |
| 15 | Every consumer of your wire enumerated — not just the ones who asked? | Settling with whoever asked is guard-the-instance again |
| 16 | Does each counter you publish say what its anomaly *looks like*? | A gauge nobody reads as a detector |

## What this method cannot see

A hunting loop reads **one codebase** carefully, and it is structurally blind to
defects that live **between two**. A seat that runs sixteen rounds and finds
nothing more has not exhausted its defect surface — it has exhausted the part one
reader can see.

The evidence is direct: after the loop below was called closed, a contract
conversation across three seats found three more live defects in an afternoon.
None were reachable by the loop's method, because each side was locally correct
— one seat's poll-time stamping could not be falsified from that seat without
knowing the other's backoff semantics. Four messages, not sixteen rounds.

So **the next move after a loop is not another loop.** It is writing the contract
with whoever consumes you, and checking it from both ends.

The tally from doing that across four seams in one evening: two live defects in
one consumer, one in another, one error in the producer's own description of its
wire, and **zero found by anyone reading their own code**. Enumerate *every*
consumer, not the ones already in the conversation — settling with whoever asked
is the same guard-the-instance mistake in a different costume.

A design property worth building in rather than discovering: **put the
provenance stamp in the thing being measured, never in the thing doing the
measuring.** When a build commit comes from the running module's own health
report, a stale reader can fail to *ask*, but it cannot report a stale answer as
current.

## The targeting principle

**The value of a hunting round is inversely proportional to how observable the
defect class is from production.**

The sharpest form of unobservable is a system that says **nothing** when it has
the most to say. One quota view printed a bare header when the producing module
was cold or broken — so the only case where something was genuinely wrong
rendered as the calmest possible screen. Pair this with the yes-path rule below
— same root, opposite sign, and the quiet path has no natural author either way.

The mechanism there is worth its own check, because it is a shape rather than an
instance: **two independent suppressions keyed on the same condition, where the
second silences the signal that would have contradicted the first.** A bare
header at zero rows is reasonable. Suppressing a count line at zero is
reasonable. The pair removes the alarm and its backstop simultaneously, because
one condition drives both — and unlike a cross-codebase seam, *both branches
were in one file* and still nobody owned the pair.

So: **when a condition suppresses output, check whether it also suppresses the
thing that would have reported the suppression.** Empty-state handling is where
this lives, because empty is exactly when every "only show if non-zero" branch
fires at once.

If you want one cheap sweep and nothing else: **list every place your code grants
something** — a permission, a capacity claim, a freshness assertion, a success
record — and check each has a test proving it is *withheld* when unearned. Test
suites have no natural author for the yes-path. Nobody forgets to test that a bad
credential is refused; everybody forgets to test that a good-looking claim is
withheld. That grep found the highest-stakes gap in the module this document
came from, twice.

Parsers, arithmetic and mapping have a second line of defence — eventually a
human notices a number that looks wrong. A state transition that overwrites its
own evidence has none, so reading the transition is not the best available
method, it is the only one.

The measurement behind that: rounds 1-10 went at parsers and transport and
produced volume, but four of six findings were repeats of two classes. Rounds
11-16 went at identity, scheduling, dedup, fencing and routing — fewer findings,
each one invisible from the consumer boundary.

When you commission a red-proof fixture, prefer one drawn from **a failure that
actually happened**. Synthetic fixtures drift with the imagination of whoever
wrote them; a fixture anchored to a real incident cannot drift away from what it
was built to catch.

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

   Expect the identical expression to be **correct one call site away**. A
   daemon read a missing config file as an empty module list, which is right at
   boot (nothing configured, nothing to supervise) and catastrophic in a rescan
   that diffs against running state, where an empty list means retire
   everything. Same expression, three places, two correct. Only the caller's
   semantics separate them, which is worse than an unusual construct: the
   familiarity is what licenses it. It also means this cannot be audited by
   grepping — every hit needs its caller read, which is the work review skips.

   A near neighbour: **an observable that is adjacent to the real property
   rather than equal to it.** A test polled for a pid file to *exist*, but
   `echo $! > file` creates the file before the shell writes into it, so a read
   landing in that window parses an empty string and panics. Existence was never
   the property it cared about — readable content was. Adjacency is invisible
   until the window opens, and the fix is to poll for the property you actually
   need. Not every such poll is wrong: one waiting on a marker whose *presence*
   is genuinely the signal is correct, so adjudicate each rather than sweeping
   them into one verdict.

   The sub-family worth its own sweep is **an absent input converted into a
   positive assertion**, because unlike the rest of the rung it has literal
   signatures to search: `unwrap_or`, `unwrap_or_default`,
   `map(...).unwrap_or(...)`. The test is not "is there a default" but **is the
   substituted value a claim, or the representation of absence?** A default is
   safe when the type has a value that *means* absent — null, empty set, an
   `unknown` variant — and the code picks that one. It is a defect when every
   available value is a positive assertion and the code picks the flattering
   one.

   Two found the same evening: a missing config read as "no modules configured"
   when it meant "I could not read the file", and an event with no recorded
   outcome delivered as a successful one. A third that the discriminator flags
   even though a test already pins it: an unrecognised session kind falling back
   to the *most privileged* variant. The fence there pins a compatibility shim,
   not a safety property, and it conflates two inputs that deserve different
   answers — an **absent** stamp (an old peer, from before the field existed)
   and a **present-but-unparseable** one (a newer or malformed sender). Only the
   first is what the compatibility argument justifies. **A fence proves intent,
   not correctness.** Two that cleared honestly under the
   same test: an absent filter defaulting to include-all (the representation of
   "no filter", not a claim that everything passed one), and an absent detail
   block defaulting to JSON null (which asserts nothing). The last two sat *one
   line apart in the same struct* and got opposite verdicts — the
   correct-one-call-site-away problem at its shortest possible range.
   **Check the fixtures before reading a single assertion.** A fixture that
   never constructs the absent case makes every test over it vacuous for that
   branch — you can write ten sharp assertions over a helper that structurally
   cannot produce the input, and every one is honest, specific, and blind in the
   same direction. That is worse than an untested branch, because the coverage
   is real; it just cannot vary in the dimension that matters. Asking whether
   any fixture can even construct the input is cheaper than reading the tests
   and strictly more decisive.

   Where the type system allows, prefer removing the ambiguous state to handling
   it. Mapping absence to a safe value is correct and still lets the next caller
   construct the ambiguity; making the field non-optional means nobody can.

   The same move works on APIs, not just types. One release-evidence builder
   split advertisement from exercise into two entry points — the plain one and a
   `_with_exercised_rows` variant — so **the honest zero is the default and the
   claim requires an argument**. That is the structural form of this whole rung:
   rather than defaulting the two together and testing that the default is
   honest, make the flattering value impossible to obtain by accident.

   One check before making a field non-optional: **can the value legitimately be
   unknown at construction time?** If a record is admitted before its outcome
   exists, non-optional forces every caller to invent one — the fail-open
   default moved upstream and made mandatory. Where admission and outcome share
   a call site the answer is no and the change is free; establish which it is
   rather than assuming.
3. **The state machine and its fences.** Incarnation/ABA, attempt ordering,
   admission fairness, what a timeout does to a prior observation. Demand the
   exact interleaving — "these two race" is not a finding.
4. **Whatever decides which data headlines.** There are two failure directions,
   and the obvious fix for one produces the other.
5. **Transport and resource bounds.** Cheap to check, real when found.
6. **Parsers last**, justified by being cheap rather than productive.

## Five brief rules that each changed the output measurably

- **A negative is conditional on the configuration it was measured under.** When
  a change reshapes the landscape a lane was searching — a new dispatch
  geometry, a different data shape, a lifted bound — the *rejected* candidates
  in the ledger need re-reading, not just the accepted ones. A mechanism ruled
  out under the old geometry may be live under the new one, and nothing in the
  ledger says so, because a null is recorded as a fact rather than as a fact
  about conditions. This is also an argument for re-firing a lane promptly after
  a shift, before the assumptions built on the old landscape fossilize.
- **State the closed seams explicitly.** Once a class is swept, name it and its
  cleared members. A lane with an open seam keeps returning instances of your
  own last fix; the first round with nothing left to mine found a real identity
  defect immediately.
- **Demand measurement before magnitude.** Two rounds inflated tens of
  microseconds into a latency problem. After the brief said so with real
  numbers, the next round measured a hundred runs, reported the aggregate as
  small, and argued from a contiguous stall instead. Same lane, scored 74 then 93.
- **Watch the first run of a changed pipeline to completion.** With
  `cancel-in-progress`, a green run list does not support a per-commit green
  claim — only a tip-is-green one. That is harmless when history is cumulative
  and the tip is what ships, and *not* harmless when the run itself changed: two
  pushes carrying a new CI step were cancelled here, so the list showed passes
  for runs that had never executed the new logic. The hazard is proportional to
  how much of the check is new. Verify the step, not the job. (A reading trap
  for later: on a bisect, a cancelled entry looks like a failure and an absent
  one looks like it was never pushed, and neither is true.)
- **Publish a counter with its anomaly signature.** Twice in one evening a gauge
  that already existed, unwatched, turned out to have been naming the defect all
  along: three blocker counters holding *exactly equal* meant a transition that
  never fires (a race would have drifted them), and a conservation identity
  nobody was asserting would have shown an unaccounted bucket. Say what the
  anomaly looks like when you add the counter — drift means a race, exact
  equality means a dead transition, imbalance means something unaccounted. **A
  gauge without its anomaly signature is data; with it, it is a detector.**
- **Build an enumeration, not a warning.** Documenting a bias does not immunise
  you against it. One seat had written the exact warning — "you will be tempted
  to guard it once and call the class closed" — in their own words, in a file
  they had edited that hour, about a gap they had just found, and it still did
  not fire: the feeling of completion arrives before the memory of the warning,
  and prose has to be *remembered and consulted*. A list has items on it, and an
  unchecked item is visible without introspection. A warning asks you to feel
  differently; a list asks you to check something.
- **Say an honest null is acceptable, and mean it.** Two nulls scored 90 and 92;
  one benchmarked the change it was tempted by and declined to report a 15.6µs
  saving. A lane that believes it must produce will produce speculative hardening.
- **On a sweep, make the negative half the deliverable.** "Name every member and
  say why each is cleared" turns a null into an auditable artifact. It also
  catches operator error: a brief said 34 providers and the sweep said 35, and
  listed them. Take the population count from the artifact, never from the brief
  — a sweep that adopts your count cannot find the member you missed.
- **A tests-only fence needs an explicit stop-and-report clause.** Fencing is the
  right instrument for an under-tested *correct* mechanism and the wrong one for
  an *incorrect* one, and the worker cannot tell those apart from the prompt. A
  sweep sent to find fail-open defaults reached one, wrote a passing test named
  for it, and thereby converted a latent bug into a defended one — the test now
  reads as a deliberate specification and its name supplies the justification.
  Fully compliant: the instruction said tests-only and it wrote a test. Without
  the clause, a sweep hardens exactly what it was sent to find.

  **Worker quality is irrelevant here** — a better model writes a more convincing
  test asserting the same defect. And **the harm scales with the sweep's
  thoroughness**: an unswept defect stays discoverable by the next person who
  reads the code, while a fenced one is discoverable by nobody, because a green
  test now certifies it. The clause is therefore not a refinement of the
  instruction. Without it the instrument *inverts* above a certain diligence.

  The trigger is not "tests-only" as such but **a fence instruction with no
  refusal path**. Any "write a test for X" carries an implicit premise that X is
  correct, and a worker has no standing to reject that premise unless the prompt
  grants it — so the clause belongs in *any* fencing prompt, sweep or not. Grant
  it explicitly, and **make reporting a wrong mechanism a success outcome rather
  than an incomplete one**. Otherwise the only compliant move is to write the
  test, and compliance is what hardens the defect.

  Make the report the **default for anything the worker cannot positively
  justify**, not an escape hatch it has to recognise the need for. Recognising
  the need requires the judgement "is this mechanism correct or merely untested"
  — which is precisely the judgement a fence instruction removes.

## `--version` is not an identity check unless the deploy crosses a version bump

A deploy the same evening made this concrete without anything going wrong. Nine
commits ahead of the last tag, no version bumped, so both the old and new
binaries reported `0.3.17`. The warm-exec probe was **true and useless**: it
cannot distinguish the new build from the release it replaced, which is exactly
the case an untagged batch deploy always is.

The three layers answer three different questions and none substitutes for
another:

- **inode** — the process is running the file at the deploy path
- **a live turn** — the process is serving
- **a symbol differential** — *which code* is inside

The differential needs controls on both ends, or it proves nothing: a string the
change introduces (present in new, absent in old) **plus** strings present in
both, which establish that the tool genuinely read the old binary rather than
failing silently against it. Without the controls, "absent from old" and "never
looked at old" are the same output — the null-versus-negative confusion again,
this time in a deploy check.

## Two functions, one name, opposite safety semantics

A shape none of the probes above can reach, because **there is no mechanism to
mutate.** No branch to delete, no verdict to ignore, no effect to suppress — just
correct code under a lying name.

CEREB found `redact_then_truncate(value, max_bytes)` in the browser crate whose
entire body was `utf8_safe_truncate(value, max_bytes)`. No redaction. Meanwhile
`cerebellum-core::journal::redaction` exports a function of **the same name**
that redacts through a bound policy and returns `Err(RedactionUnavailable)` when
none is bound — its own doc explaining that copying or truncating input would
"make sensitive material durable." The core module explicitly refuses the exact
behaviour its same-named neighbour performs. **A prefix of a secret is still a
secret.**

Zero callers today, so not a live leak — and still worth fixing, for the reason
that generalises: **a function that looks like redaction and is not will answer a
future search.** Someone wiring an adapter greps for redaction, finds it, uses
it, and the review reads correct because the call site says `redact_then_truncate`.
**The name is the claim, and it is the only thing most readers will check.**

The only instrument that finds this is reading the call site **against the
contract the name implies**. Same family as a drifted fixture: the artifact
answers a question it should not be authoritative for.

## The classes are a lens, not a partition

Tested against tonight's confirmed defects rather than against the taxonomy's own
examples, and it does not partition:

- The authorizer that armed a destructive tee on a refusal sits in **two** classes
  at once. The classifier returned Unauthorized correctly and arming ignored it
  (fenced decision, unfenced enforcement); every test asserted the Unauthorized
  label and none asserted nothing was queued (report asserted, effect unasserted).
- The secret-input guard whose call site was neuterable sits in **one**. There was
  no report to assert — the unit tests asserted a predicate's return value, not
  an emitted outcome.
- The `unwrap_or(Success)` fail-open default sits in **neither**. No decision
  function exists; the defect *is* the absent decision, and nothing asserted
  anything.

So do not route a candidate by picking its class. **Run every probe against every
candidate** — delete the call site, assert the effect did not happen, mutate the
ordering, vary the input — because a defect answering "no" to one framing can
still answer "yes" to another, and the third shape answers no to both while being
live in production.

The classes are useful for *explaining* a defect once found and for generating
probe ideas. They are not a decision procedure for which probe to skip.

## A fenced decision is not a fenced enforcement

A policy function returning the right verdict and a call site acting on that
verdict are **independently mutable**. Tests over the decision function leave the
enforcement separately deletable — delete it and everything stays green — while
grepping for the error variant finds the decision test and scores the guard
covered. Same false positive as auditing by error name.

That does not retire the grep; it demotes it. **Grep the variant to build the
candidate list, then mutate each production site separately to build the
coverage map.** The enumeration is cheap and still finds the unasserted
refusals; only the mutation is evidence. Two distinct shapes score covered under
the grep alone: a fenced decision whose enforcement is unfenced, and one fenced
site out of two that return the same variant.

Once a defect is fixed, **sweep the property, not the instance that revealed
it.** List every site with the same property — every place the wire asserts
something a consumer acts on, every call site that applies a grant — rather than
re-reading the one just repaired. One seat nearly stopped after fixing a
relaxation gap and found a second, completely undefended guard only because the
enumeration was mechanical rather than a decision about whether they felt done.
Adjudicate each candidate separately, though: the sweep finds them, it does not
settle them, and two fields one line apart in one struct can deserve opposite
verdicts.

That second shape is not incidental. **Where a module has two emission paths for
the same data, every guard exists twice and will be tested once** — and the
tested one is predictably the *simpler* path, because it is easier to write a
test for, while the richer path is where the consequences live. Two separate
guards in one module landed this way on the same evening: one condition named
and tested on the plain path, the same condition entirely undefended on the path
that actually carries the extra field. Deleting it left 424 tests green.

The cheap check: if there is a branch for the simple case and a branch for the
full case, **diff the two branches against each other** rather than reading each
on its own. Every condition in one wants a counterpart in the other.

**Status: untested outside its author's repo.** Both instances behind this rule
came from one module, so it may be a description of that codebase wearing a
rule's clothes. The prediction, stated so it can fail: the next seat with a
dual-emission or dual-rendering surface who runs this will find a guard tested
on exactly one branch. **If several seats run it and none find that shape,
demote it** — and report the null, because a rule nobody contradicts is not the
same as a rule that holds.

This also predicts how the naming proxy fails, which is not by being absent. If
every guard exists twice and gets tested once, the named test almost always
*exists* — attached to the other branch. So the proxy fails by being **present
and pointing at the wrong site**: searching "is there a test for this condition"
returns yes, correctly, and the answer is useless. Ask instead: **is this
condition tested at this site?** The first question has a misleading true answer
whenever a twin exists.

Note the ordering this produces on one guard: nobody asserted it → the verdict
was asserted → **the enforcement was still separately deletable**. Three
positions on one axis, not a repeat. Constant-function mutation on the unit
cannot see the third, because the unit is genuinely correct — a predicate can be
perfectly tested and never invoked, and the missing coverage lives one layer out
at the call site. The check is one line: neuter the call site (`if false && …`)
and see whether anything reddens.

Where the guard's job is to *prevent* something, assert on the thing not
happening. A rework asserted the consent hook receives **zero calls** on the
refusing path, which made a pure ordering mutation visible: move the consent
request ahead of the safety check, leave the refusal itself intact, and both
tests fail. **That fences ordering rather than outcome**, which no outcome
assertion can do — asking a human to approve an action already known unsafe is
its own defect and is invisible to every test that checks only the returned
error.

The practical cost is that this needs a **recording double** rather than a
return-value assertion: a spy that logs its calls, then assert the log is empty.
Cheap to build, and the only way an ordering invariant becomes visible at all.

**A query over one relation does not support a claim about another.** Three
instances in one evening, all confident and all wrong: a normative row count
("what the design requires") read as a provable-here count ("what this ABI can
measure"); a vocabulary drawn from the dominant cases in a population and closed
without checking it was *total* over that population; and a capability treated
as reachable because its identifier appeared somewhere in a related file rather
than because it was reachable by the mechanism that matters. Nothing in any of
the files signals the difference — each query returns a clean number.

So: **before closing a vocabulary, assign every member of the population and
confirm the remainder is empty.** And beware a decomposition that changes with
the analyst: three attempts at splitting one set produced three different splits
while the total never moved, which proves the split was a property of the
projection rather than of the data. Collapsing a many-to-many join into a
partition has no canonical answer, so every implementation invents one — and a
number that lands in a file gets cited.

**Two derivations agreeing is evidence only when they are independent in the
premise, not merely in the hands.** Re-running someone's query and getting their
answer is a transcription check wearing a verification's clothes: it catches
typos and nothing else. Confirming a number by a *different relation* is the
check; confirming it by the same one is applause.

**An uncontrolled measurement is an observation, not an attribution.** A striking
spread is not a mechanism. One investigation produced self-overwrite timings of
2174, 4941, 14015 and 20003ms and wrote them up as a root cause with a fix
attached — but the slow arm created a fresh file per iteration while the fast
baseline reused one file three hundred times, so the comparison carried two
variables and isolated neither. Rerun with one variable flipped, both arms
landed at ~100ms and the hypothesis died in four minutes. The numbers were real;
the attribution was never earned.

Two failure directions here, and they produce identically confident writeups:
claiming a control you never ran, and running an arm and never claiming the
control. When an intermittent resists explanation, **eliminating a category is
real progress even when it leaves you with nothing** — and an intermittent you
cannot trigger is one whose fix you cannot verify, so shipping a repair produces
a green suite that proves nothing and retires the investigation, because nobody
re-opens a bug marked fixed.

**A contamination claim is a hypothesis about a population, and it is falsifiable
per-datum.** Environmental noise — memory pressure, a loaded box, a bad window —
slows a *window*; it does not produce an isolated spike in an otherwise fast
period. So before discarding a measurement on those grounds, read its
neighbours: one 82-second transform pass sat between neighbours of 30ms, 17ms,
18ms and 5ms against a day median of 6ms, which kills the environmental
explanation for that point no matter how true it is on average. Getting this
wrong is worse than the contamination it guards against, because **nobody goes
looking for what a caveat deleted.**

A verification that never reaches its assertion looks exactly like one that
passed. A gate run with a deliberately-broken input timed out before reaching
the phase under test — no failure line, nonzero exit — and reading that as "no
problem reported" would have recorded a false clear. **When a probe is expected
to fail, confirm the failing phase's label appears in the output**, not merely
that the exit code is nonzero: a timeout, a crash and a genuine finding are
indistinguishable by exit code. This is the third way to get a signal with no
discriminating power, alongside the unconstructible fixture and the
always-satisfied condition — and the sneakiest, because the evidence is an
absence of output rather than a wrong output.

Stated generally: **where a check exists to prevent an effect, the only assertion
that fences it is that the effect did not happen.** Asserting the returned error
fences the *report*, and a mutant that performs the effect and then reports
correctly passes every such test — report and effect are as independently
deletable as decision and enforcement. So the axis has three positions, not two,
and the instinctive assertion lands on the wrong one: an authority check whose
caller's early return was deleted still returned its error, just after
dispatching.

When a mutation does kill a test, **check that the test that died is the one you
meant to prove.** One deleted condition reddened three tests all named for
something else entirely, which reads as "covered" and is actually "defended by
accident" — the same habit as reading a failure *message* rather than a failure
*colour*, one level up. Read which test died, not that one did.

A mutation result has **two baselines, and everyone runs only one.** We check
that the mutant turns a test red. We forget to check the test was green before
the mutation. THALAMUS ran a mutant against a fence and two integration tests
went red; they nearly reported "integration catches it." Those two fail
identically on clean code — opt-in corpus probes that panic without env vars
pointing at capture directories. Without the clean-baseline run they would have
credited coverage that does not exist and stopped looking. **Green before, red
after. One without the other is a guard with no negative vector, arriving
through the instrument instead of the code.**

### A filter that returns empty must be proven capable of returning non-empty

The same defect in a query instead of a test. `git show <sha> --stat -- 'crates/*/src/'`
returns empty for **every** commit — that pathspec form matches nothing — and an
empty result reads as a negative finding when it is a null one. Nothing on screen
distinguishes "no source files in this commit" from "this query matches nothing,
ever."

It was caught only because the same command was run against a second repo where
the answer was independently known to be different: four commits reported
test-only, a differently-shaped query on the same range showed 349 source
insertions. **The derivation that agreed with expectation was the broken one.**

So before believing an absence, run a **positive control**: the same filter
against input you know it should match. This is the mutation rule wearing
different clothes — confirm the instrument can produce the other answer.

**One control is not enough. The control set has to span the shapes the filter
will meet.** THALAMUS wrote a checker, ran a control on a linear commit, got a
correct non-zero, and the instrument was still broken — `git show` and `git log
--name-only` **print nothing at all for a merge commit** without `-m
--first-parent`. A second control on a merge returned 0 against a known 80
insertions, which then exposed a *second* independent bug (an early return inside
a per-file loop). Two bugs, both returning 0, both looking exactly like a clean
answer.

That one is not hypothetical here: this document's own fleet deploy-gap check had
it. Positive control on plexus `44ab703` — bare `--name-only` listed **zero**
files against a 719-insertion `--stat`. alfonso carries 117 merges a week and
plexus 13, so the blindness covered most real change on every repo that merges
rather than rebases.

**A control that fails must itself be verified before you blame the instrument.**
This is the least intuitive of the three, because a failing control has the
emotional shape of catching something. Two instances in one hour: my first linear
control returned 0 because the commit was genuinely docs-only; THALAMUS planted a
control file to prove a scanner worked, still got zero, and was one keystroke from
reporting the scanner broken — the planted line was
`subprocess.run(["git","log",...])`, which contains `git","log`, not the literal
`git log` the pattern matched. **The scanner had been correct the whole time.**

A malformed control produces the exact signature of a broken instrument, and the
natural response — distrust the instrument — costs you the true answer.

**The constructive form beats the warning.** ALF's repo carries the most merges in
the fleet — 117 a week — and has zero exposure, for a structural reason rather
than luck: every file-set question there is a **range diff anchored at a recorded
base** (`git diff base..HEAD`), never a per-commit log walk. A range diff cannot
be merge-blind by construction, so the bug is unrepresentable rather than
avoided. The anchor came free from a worktree contract that already records a
base SHA for provenance.

So prefer *derive changed-file sets from range diffs anchored at a recorded base*
over *remember to pass `-m --first-parent`*. The first cannot be forgotten; the
second is a defensive flag a future tidy-up deletes. The precondition is worth
stating though: the immunity belongs to the **anchor**, so any query without one
— an ad-hoc "what changed lately", a periodic sweep — steps back outside it and
gets no warning.

ALF's generalisation from that is worth more than the bug: **protections that are
side effects of something the system needs anyway survive refactors that dedicated
guards do not.** A dedicated guard's cost is visible and its benefit is invisible,
so it loses every tidy-up argument on its own merits. A protection riding on
load-bearing machinery cannot be removed without breaking something whose value is
obvious — load-bearing for a reason unrelated to the hazard it prevents.

Which makes it a design instruction, not just an observation: **when choosing
between two ways to prevent a hazard, prefer the one that piggybacks on a
mechanism the system already depends on**, even where the dedicated guard is more
direct. Directness is worth less than durability for anything meant to hold for
years. And it is worth hunting for these deliberately — they are the protections
nobody has to maintain.

### A park carrying an unsized recommendation is a trap for its own author

E2E parked an ungated failing typecheck with "either gate it or delete it," as
though the two were comparable. They were only comparable while the cost was
unknown — one measurement gave one file and two errors on a single line, and
gating became obviously right.

The harm is downstream: **a parked item carrying an unsized recommendation will be
picked up later by someone who trusts the recommendation more than they should,
including its author.** A park is a message to a reader with less context than
you had, and a lean reads to them as a considered judgement. So the honest park
is **the option set plus the unknown**, not a lean. Same defect as publishing a
number with a caveat: the qualifier evaporates and the confident part travels.

The same mechanism reaches design items. CALLO parked "build a recovery path for
a mistaken self-tombstone" with a plausible story, then killed it on
re-examination: the daemon cannot distinguish a mistaken compromise push from a
real one, so **any clear-path is equally available to whoever caused the push** —
a recovery mechanism that cannot authenticate the recoverer is a bypass with
better manners. The real recovery already existed (mint a new key; a fresh key
never matches an old tombstone).

So: **an open item that should stay unbuilt has to say so explicitly, or its mere
presence is an instruction.** A to-do list is a queue by default, and silence
about whether something *should* be done reads as assent. The note has to say DO
NOT BUILD THIS with the reasoning, or the next session re-derives the plausible
story and builds it.

Related pair, from two ungated scripts found the same night: **a failing ungated
script is a dormant alarm; a passing one is an active false assurance.** The
failing one turns red the moment anyone runs it by hand. The passing one is
confirmed by every hand-run while nothing establishes it will stay green.

And for any check at all: **existence and execution are not the question, timing
relative to the decision is.** A release-time check reports once someone is
already publishing — attached to the action you least want to abort, with the
whole merge-to-release window invisible behind it.

The three interlock, in this order:

1. A filter returning empty must be proven capable of returning non-empty.
2. The control set must span the **shapes** the filter will meet.
3. A control that fails must itself be verified before you blame the instrument.

Each caught a different error the night they were written.

**Phrase the check so silence is suspicious.** Better than remembering the
control, because it removes the need to remember. When confirming someone else's
claim, prefer a command whose *expected* output is non-empty: "show me every file
this commit touched" and read it, rather than "show me the source files,
expecting none." An empty result then means the command failed rather than the
claim held — the ambiguity points at the instrument instead of hiding inside the
answer.

The reason this class survives in shell commands and not in suites: **a test's
output is consumed by a machine that can only say pass or fail, so ambiguity in
it eventually surfaces as a contradiction. A verification command's output is
consumed by a person who is already expecting an answer,** so ambiguity resolves
silently toward the expectation. A blank line is what "test-only" looks like, so
looking for test-only found it.

Which puts the exposure exactly where we are most useful to each other:
cross-seat verification of a claim someone has already stated. The asker has an
expectation, the command is typed once against that expectation, and its output
becomes the sentence the other seat acts on.

**The unexamined surface is the ad-hoc verification command, not the test suite.**
Everything else in this document lives in committed test code, where a culture of
mutating already exists. The one-off commands we run to check each other's claims
have no such culture: typed once, trusted on sight, and their output becomes the
record a peer acts on. So the standing form is **any command whose output will be
quoted as evidence gets one positive control first.**

And do not over-correct into a narrower filter. `*/src/*` fixes the broken
pathspec but answers only "did any Rust source change" — a Cargo.toml dependency
bump, a build.rs, or a catalog manifest all reach the served binary and match
none of it. For "does this reach production," the honest instrument is an
unfiltered `--stat` read by eye.

Related, from the same investigation: **do not read git's hunk-header text to
decide where a change landed.** That header is a heuristic guess at enclosing
context, and on a file carrying raw fixture strings it picks the fixture. A hunk
1200 lines inside `mod tests {` was labelled `data: {"type":"message_stop"}`.
Compare line positions against the file's own structure instead — the artifact's
shape, not a label describing it.

### When the mutant does not kill your new test, the test is the suspect

THALAMUS wrote a test for an authorizer that armed a destructive tee. Three
versions, all green, and **two of them were worthless**:

- v1 failed — but on the observability assertion, not either security one. No
  transform was installed, so the declaration was never examined.
- v2 passed, **and passed against the mutant too**. Arming was blocked earlier by
  inactive surface state and absent guidance, so control never reached the
  authorizer. The test would have passed whatever the authorizer did.
- v3 became evidence only after installing an active surface and usable guidance,
  so the declaration was the only thing between the call and the queue.

The instinct on v2 is that the guard has some other protection making the mutant
harmless. It did — and that is exactly the problem: **the test was measuring the
other protection, not the guard it names.** A test that passes for a reason other
than the one in its name is worse than no test at that site, because it marks the
site as covered.

This is the hunted class appearing *inside the hunt*, twice, in ten minutes,
while explicitly looking for it. **So the defence cannot be vigilance** —
vigilance was already at its ceiling and lost. It has to be a mechanical step that
runs whether or not you suspect anything: mutate, confirm red, and confirm it was
*your new test* that went red *for the named reason*. Cheap enough to do always,
which is the only property that matters when the failure mode is invisible from
inside.

**The trap is worst for non-occurrence properties**, which is exactly where this
rung sends you. A test asserting "nothing was enqueued" **passes trivially if the
code path never ran**, and nothing in the output distinguishes that from a working
guard. Absence-shaped evidence and the v2 trap are the worst possible pairing —
and the third-shift version of that pairing is worse still, so stop before doing
these tired.

### A fix to the mechanism does not cover the callers of the mechanism

THALAMUS fixed one fence by asserting it replaces bytes correctly. Twenty minutes
later the next candidate had the identical defect, and **the fresh fix did not
catch it** — the new mutant left the mechanism correct and broke the *caller*,
handing it the wrong bytes to fence to. The gate reported its refusal accurately
and sent the rejected body anyway. All 400 tests passed, including the fence test
written twenty minutes earlier.

So the decision/enforcement split **recurses**. Fixing an enforcement site proves
nothing about a second site that uses the same mechanism, and a suite that just
grew a correct new test is exactly where you feel most covered. **Each call site
needs its own mutant at its own call site. Adjacent fixes do not generalise.**

## Assert both directions, not only the one you are hunting

THALAMUS's fix asserts the forwarded bytes **equal** the client's request and
**do not equal** the transformed one. That kills two mutants: the one they hunted
(report the fence, forward the untrusted bytes) and its opposite (withhold
correctly, mislabel the record as a rewrite).

A single-direction assertion fences the effect. A bidirectional one fences the
**pairing between the label and the effect**, which is the whole job of a
decision log. The aggravating detail in both this case and the attestation one is
that the record was not silent — it was confidently wrong. An operator reading
`raw_only_fence` in the log would conclude the request was withheld when it was
sent.

## The cost-asymmetry gate

This killed two proposed guards and justified one. Wrongly rejecting a good
response turns a working provider into a broken one; wrongly accepting a
questionable one costs at most one stale read.

**When both sides are bounded, decline the guard. When one side is unbounded and
silent, take it.** A guard whose correctness depends on a value unmeasurable
from the running machine must not ship.

## Ask the mutation question of fixes, not just guards

The constant-accept lesson has a twin on the delivery side. A reclamation fix
arrived with every delivered test passing **against an implementation that reaps
unconditionally** — the positive tests prove the change happens and say nothing
about whether it is bounded. The author caught it by adding a live-root control
himself.

So before merging anything that removes, reclaims, or refuses: **ask what the
tests would say if it did that to everything.** If the answer is "they would
still pass", the suite cannot distinguish the fix from its unbounded version,
and shipping it is worse than the leak it repairs. The load-bearing test for a
narrowing change is always the negative one.

The reason this recurs is in how the two get written. The positive test is what
you write **while thinking about the fix** — it comes free with the work. The
negative test requires imagining the fix wrong in the direction you were not
worried about, and only arrives if you deliberately ask: *what would a broken
version of my own change still pass?*

A related trap on the input side: when an observation authorises destruction,
**the observation needs as much confidence as the destruction is irreversible**
— and that asymmetry is invisible at the call site. `if !path.exists() {
reclaim() }` shows a check and an action and hides that the left side is a cheap
sample of a racy world while the right side is unrecoverable. Confirm across two
sweeps before acting on absence.

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
