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
| 17 | Would a *new entry* in the thing you are asserting over be routine or alarming? | Routine and you asserted a count: healthy growth reads as breakage. Alarming and you asserted a property: silent shrinkage reads as green |

Row 17 is the shape of every entry here worth trusting: **a rule recorded without
its discriminator is half-guidance**, and the half that travels is whichever
sounded cleaner. "Assert the property, not the census" and "a data-driven suite
must assert its item count" are both correct, in the same repository, on the same
afternoon — and neither is derivable from a rule about assertions. What separates
them is a question about the *subject*, not about the check.

So when a rule goes in a document, the thing to write beside it is not more
justification. It is the case where it is wrong.

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

### The fixture shape decides which property gets asserted

THALAMUS predicted their suite was uniformly report-asserting. Five sites in, the
prediction was **falsified** — and the exception explains the rule better than the
rule did.

The one covered site had a **recording sink in its fixture**, so asserting the
effect (`sink.is_empty()`) was the natural thing to write. The four defective
sites express their outcome through a **return value**, so asserting the return
value was the natural thing to write. **Nobody chose the weaker property. The
fixture shape decided it.**

That predicts where to look in any codebase: **guards whose effect is not
observable in existing test scaffolding.** And it names a cheaper structural fix
than exhortation — make effects observable in fixtures, and the natural assertion
becomes the right one.

The prediction then held **prospectively** on the sixth site, which is the only
form of confirmation worth much. Adding a way to read the durable state
(`mids.pins(key)`) made the effect assertion the obvious thing to write — and the
accessor already existed, built for observability. So the fix is stronger than
"make effects observable in fixtures": **an observability accessor is also a
testability affordance**, and the two are usually built for different reasons by
different people. Adding a read accessor for a status surface silently upgrades
the tests someone writes months later.

Final tally over six refusal points: four defective (fence mechanism, outbound
validity gate, tool authorizer, generation admission), two already covered — and
**both of the covered ones were covered because their effect was readable in a
fixture.** All six had tests naming their error variant, which is why all six read
as covered.

### Commit the test before probing it

WERNI ran the suite green before every mutation, per the rule above — and twice a
run came back green when they expected red. Both times **the git checkout that
reverted the mutant also reverted the new test**, which was uncommitted. **A suite
that no longer contains the test passes very convincingly.**

So: commit the test first, and re-verify the mutation actually landed in the file
before believing any result. The clean-baseline rule and this one are the same
rule about different halves — confirm what is actually in the tree, not what you
intended to put there.

### A value only does work when two of something collide

WERNI mutated four surface entry points to hand a shared pipeline a **constant**
installation namespace, mechanism untouched. Slack killed it, Discord killed it,
**Teams and Telegram survived with everything green.**

The reason generalises past namespaces: **a namespace argument is invisible to any
single-installation test.** Under one installation a correct namespace and a
constant are indistinguishable — the value only does work when two installations
collide. Slack and Discord happened to have two-identity tests for unrelated
reasons; the coverage difference was accidental, not designed.

General form: **for any parameter whose job is to keep two things apart, a
single-instance test cannot fence it.** Ask what the value distinguishes, then
build a fixture containing both sides of that distinction. The consequence here
was a silent drop rather than an error — a collapsed namespace made a second
installation's turn read as a redelivery of the first, answered "still working"
forever.

Method rider from the same run: **when the suite does catch your mutant, find
which test and which assertion.** A hit can be a decision-level test that proves
nothing about enforcement — which is exactly what three passing tests were at the
site that turned out to be defective.

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

**The boundary of everything else in this document**, stated precisely by CEREB:
every probe above tests *whether a mechanism can be removed without detection*,
and that presupposes a mechanism. This defect had none. The code does exactly
what it says at the instruction level and lies only about **what a reader will
believe**. No mutation reaches that, because there is nothing to mutate.

The candidate set *is* automatable even though the judgement is not: enumerate
every function whose name asserts a safety property
(`redact|verify|authorize|sanitize|validate|check|assert|ensure|guard`) per
crate, then take the **intersection across crates** — a name living in two crates
is where a reader picks up the wrong contract, especially across a boundary where
one side fails closed by design. On CEREB's workspace: 68 safety-named functions,
exactly two names duplicated, one liar and nine honest clears. That turns "read
everything" into "read these."

Its stated limit: it finds **name collisions across crates**. A lying name with no
honest twin has no contrast to trip the sweep and is still only findable by
reading the body against the name.

And the instinct to defer a zero-caller finding is backwards. **Zero callers is
simultaneously the cheapest moment to fix and the most likely to be dismissed** —
the cost rises with every call site added, and the review that adds the first one
will read correct.

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

## What this rung actually finds: unfenced guards, not broken ones

A correction to the whole document, measured rather than assumed. Across four
seats and eleven fix commits, **almost every fix shipped zero lines outside
`mod tests`.** Verified by hunk position against each file's test-module
boundary, with positive controls on commits known to ship code (a feature commit
showed 2 shipped hunks; a manifest-guard commit showed 2).

So the guards were **present and working in the served binaries the whole time.**
What was missing was any test that would catch their removal. The mutations that
found them — delete the call site, watch the suite stay green — were probes, not
discoveries of live faults.

That distinction matters and is easy to lose in the retelling:

- **Live bug:** "an older projection reaches the provider after a newer one."
- **Latent bug:** "an older projection *would* reach the provider if anyone ever
  deleted that line, and nothing would notice."

The second is what was found. It is still worth the night — an unfenced guard is
one refactor from an unenforced one, and the refactor will pass review because
the tests are green — but it is not an outage narrowly avoided, and describing it
as one inflates every future report from the same method.

The exception proves the rule: one fix in the set (a `unwrap_or(Success)`
fail-open default) **did** ship enforcement code, because that one was genuinely
wrong rather than merely untested.

**Where the inflation enters, and it is not carelessness.** THALAMUS traced it in
their own reports: every commit message was accurate, each describing the mutant
("making the gate fence to...", "passed all 400 tests"). The inflation lived
entirely in the prose summaries — "four guards reported their refusal correctly
while preventing nothing" **describes the mutant and reads as a description of the
shipped code.** Compressing "the mutant does X and nothing catches it" into "the
guard does X" silently changes the tense and the subject.

So, a standing rule for reporting mutation results: **the finding is always about
a hypothetical artifact, and every natural short phrasing describes it as if it
were the real one.** Name the mutant as the subject, or say "no test would catch
this" rather than "the guard does not do this."

And the reason nobody caught it from inside: **the person who wrote the change is
the worst-placed to notice what it does not contain.** They knew what they wrote;
the question of whether it touched shipped code never arose.

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

## A status field whose healthy value and never-happened value render identically

The gauge version of this is well known: a counter at 0 is indistinguishable from
a counter never wired. The same property shows up in workflow state, where it is
harder to see and more expensive.

Two instances in one night:

- A work-graph leaf marked **done with zero files changed**, its delivery note
  saying the prerequisite was unavailable. On the board, *"blocked, did nothing"*
  and *"delivered"* render the same.
- An **optional phase that fails without failing the run**: every discuss-phase
  seat reporting failure across two campaign rounds while fanout and measurement
  proceeded and both rounds settled. Failure, skip, and success-with-nothing-to-say
  all render as a completed round.

The second is worse than a missing phase. Results were banked under the
assumption the phase ran, so they came from a different experiment than the one
designed -- contamination, not absence. And if a fix lands mid-campaign you get a
silent methodology change between rounds, with the leaderboard comparing across a
boundary nobody recorded.

So: **stamp the run with which phases actually executed**, independently of
whether their absence is judged to matter. That is cheap, it stays useful if the
phase turns out to be legitimately optional, and it is the only thing that keeps
rounds comparable across a repair.

### A status field can lie in both directions at once

The campaign phase above turned out to be neither dead nor optional. Its grammar
parser demanded `LANE: class - scope` on an **ASCII hyphen**, while models emit
the em-dash: 74 of 93 posts ever recorded failed on that separator. Of the 19
that parsed, most carry truncated classes, because splitting on the first hyphen
cuts inside hyphenated names (`kv-cache-bandwidth` becomes class `kv`). And a
third defect renders success as failure -- the overlap check does not exclude a
seat's own lane, so a redrive self-collides and a valid recorded post still
reports `lane-taken`.

So one `failed` covered three different states: *parser rejected the input*,
*genuinely produced nothing*, and *succeeded then collided with itself*. The
same-glyph problem is usually about a healthy value indistinguishable from a
never-happened one; here the field was also reporting failure for work that
succeeded.

Worth keeping for its own sake: **the grammar fought the models' strongest prior
and lost 74 times out of 93.** When a parser and a generator disagree about
punctuation, the parser loses -- so build the tolerance in rather than
discovering the preference in a corpus of failures.

### But check whether the defect is a constant before calling it contamination

I escalated the case above as contamination -- results banked under a broken
phase, a mid-campaign fix splitting the dataset. The seat checked the *previous*
campaign instead of accepting the framing, and found the identical failure
signature there. The phase had never run, in any campaign. There was no boundary
to stamp.

**A defect present in every run is not contamination -- it is a constant, and
constants preserve comparability.** The finding inverts: nothing in the
historical record needs marking, but the whole record turns out to describe a
different experiment than anyone thought (here: every promoted winner was
selected without the deliberation phase). The damage is bounded to the moment
someone fixes it, which is why **the stamp belongs on the fix, not on the
breakage.**

Checking one prior run was cheaper than the escalation I sent.

## Phrase a status report so that silence would be suspicious

The best deploy summary anyone sent me tonight had this property: every claim in
it was one command from being falsified. An **unfiltered** `--stat` over the
commit range, the two source commits named and characterised, the running
process verified by pid and inode number.

Compare *"nothing important changed"* -- unfalsifiable by construction, and it
reads exactly the same whether it is true, whether the author checked, or whether
the author's filter was broken.

The same author flagged an assertion of theirs as structurally correct but **not
yet mutation-proved**, and volunteered it above their passing results. That is
the right instinct: correct-shaped assertions are precisely the ones that pass
while unreachable, because nothing about reading them reveals whether control
arrives.

### A snapshot gauge needs its own age rendered beside it

A health line read `1 critical unsettled intents`. The store held **zero** at
that instant, verified three times. The counted items were wakes that legitimately
live 29-59 seconds in flight, and the snapshot refreshes every 30 seconds -- so
under normal churn the gauge has a fair chance of freezing a genuinely in-flight
row on *every* refresh. Not an occasional false positive: a structural one.

The second defect was worse and only surfaced because someone checked. **The
reading was not merely wrong, it was 35 minutes old.** The refresh shares a tick
with other work, and slow passes park it. A `computedAt` field existed; nothing
rendered its *age*, so a stale number and a fresh one looked identical.

That is presence-without-liveness again, aimed at the surface's own timestamp:
recording when a value was computed does nothing unless a reader is shown how
long ago that was. **Render the age, not the timestamp** -- and if a gauge is a
snapshot, its staleness is part of its value.

A rider that follows from the same case: a freshness floor must compute against
the *item's* creation time, never against snapshot time, or it inherits exactly
the staleness it was added to filter.

### A headline number that is a sum cannot be thresholded on one addend

The agreed escalation rule was: treat the degraded line as the known case unless
one named counter exceeds ten. Applied to a live reading, the surface said **11**
and the named counter said **7**.

Enumerating resolved it: the displayed number is a sum of two counters with
different semantics -- seven in-flight, four whose outcome nobody established --
and the threshold covered only the first. So the second could climb without
limit, the surface would show a large growing number, and the rule would
correctly instruct a reader to ignore it. **The trigger would be doing exactly
what it was designed to prevent.**

The two addends also have different healthy ranges. In-flight is transient by
nature; outcome-unknown is not, and a growing count there is the genuinely bad
case. Summing them produces a number with no meaningful threshold at all.

So: if a headline aggregates counters, either threshold the aggregate or display
the addends separately -- but never threshold one addend and display the sum.

**And my semantic split, while correct in general, was operationally wrong.**
I argued the two addends needed different treatment because in-flight is
transient by nature and outcome-unknown is not. Checking the rows showed
outcome-unknown is *also* a routine transit state on that surface: four intents
passed through it after one ambiguous attempt and settled completed on the
reconcile probe, all within 59 seconds.

So the discriminator is not the class, it is **per-item age**. A 59-second
outcome-unknown is healthy reconcile machinery; a ten-minute one is an incident.
A distinction that is real in the type system can be absent in the operational
data, and only the rows tell you which.

**Persistence beats magnitude.** The strongest of the corrected triggers is not a
threshold at all: re-read ten minutes later and check whether the *same* items
are still elevated. Churn freezes a different item on every snapshot; a real
backlog freezes the same ones. That costs one extra read and is far harder to
fool than any number.

### And chasing why the refresh parked found a four-day-old zombie

A consult terminal since four days earlier still carried two attempts stuck in
`canceling`, each re-probed every tick, each accumulating **9,600 probe attempts**
-- harvesting state and then failing to settle against a fence that no longer
exists. A canceling attempt on a terminal parent has nothing to settle against
*by construction*, so it must be reaped at terminalization rather than retried
forever.

Nobody would have looked. It produced no alert, only churn -- and that churn was
part of why the snapshot above went stale. **A retry loop with no terminal
condition is invisible until you ask why something unrelated is slow.**

## Five empty tables, one fault

Chasing whether a backup generation was stalled, I sampled five store tables for
progress. Every one read unchanged. All five had **zero rows** -- they belong to
a second subsystem that is built, gate-passed, and has never had a live session
on this machine, so its entire schema is permanently empty here.

I was sampling tables that *structurally cannot change*, and "unchanged" from
each was a null result wearing a finding's clothes. One `SELECT COUNT(*)` would
have caught all five at once -- which is the same rule as preferring a probe
whose expected result is non-empty, so a blank means the instrument failed rather
than the world being quiet.

The fd count was the same fault in another costume: 1388 for one process against
1387 for an unrelated one is a system-wide number, not a per-process one.

### The one crude measurement that worked, and why it was only half right

CPU time over a 40-second window moved 20ms. I read that as "waiting, not
working" -- correct -- and inferred it might be stalled, which was wrong. The
process was waiting **on the network, per object, three seconds at a time.**
**CPU is a good liveness signal for CPU-bound work and a useless one for
round-trip-bound work**, and 20ms in 40s is exactly what a healthy uploader looks
like.

The real progress record was not in the database at all. It was a file: one
object id appended and fsynced per confirmed upload, which doubles as the resume
record. Measured: 21 objects/min, 1220 of 1464 -- textbook.

Two riders from its owner, both worth keeping. **A rate read from an
append-as-you-go log is sound; a ratio read from it mid-flight is not** -- the
denominator is not final until the run is. And knowing recovery is cheap is not a
reason to restart: a restart would have cost one in-flight object and resumed,
but it would also have destroyed the evidence distinguishing waiting from wedged,
and nobody knew the cost was low until the question was answered.

## A transcribed allowlist agrees with its source only at the moment it was typed

A scope verifier was built to check that a module's public exports match a
normative ABI file. Its first delivery carried **two hardcoded sets of ten
names** and never read the file.

The names were correct. That is what makes it worse than an obvious bug: it
passed, it would keep passing, and it would keep passing after the normative file
changed. **A transcribed allowlist decays with nothing to detect the drift** --
one step better than deriving the allowlist from the tree you are checking, and
failing in the same direction.

So the verifier committed the defect it exists to catch, in the rule immediately
next to one that got it right: the same file's ownership rule parsed its
authority correctly.

Two details worth copying from how it was corrected:

**The derivation was proven exact before it was asked for.** Parsing the five
relevant keys yielded precisely the ten hardcoded names, zero missing and zero
extra in both directions -- so the request was mechanical rather than
speculative, which is the difference between a correction and a redesign.

**Two string literals were left in place, and reported rather than hidden.** They
are call-graph roots, not allowlists, and renaming an entrypoint produces "could
not resolve all three parity functions" plus further violations. **A literal that
fails loudly cannot hide drift**; the instruction was "no hardcoded names" and
the honest answer was to explain why two remain rather than to satisfy the
letter of it.

## Recomputed is not reproduced

An artifact that records command output can be checked two ways, and only one of
them proves anything about the commands.

RECOMPUTING checks the document against itself: embedded bytes hash to recorded
digests, the file list matches the range, vocabularies are closed. All necessary,
and all satisfiable by a document nobody ever ran a command to produce. Internal
consistency is a property of the author's arithmetic.

REPRODUCING checks the document against the world: check out the candidate
commit, run one of the recorded commands, compare the bytes. That is the only
check separating CAPTURED evidence from TRANSCRIBED evidence.

The distinction generalises past evidence documents. Every field is either a
claim about something recomputable -- recompute it -- or a claim that AN ACTION
OCCURRED, which only re-performing the action can test. Ask which fields are of
the second kind. There are usually few, and they carry all the weight.

This is the transcribed-allowlist rule one layer up: an allowlist agrees with its
source only at the moment it was typed, and a recorded output agrees with the
command only at the moment it was run.

## A substring match produces a confident, specific, wrong answer

A capability scan reported that a parser module performed randomness. The
offending line:

```ts
readonly #brand = PARSED_ARTIFACT_TOKEN;
```

`rand` matched inside `#brand`. The scan found a randomness import in a private
class field name.

What makes substring matching worse than a vague instrument is that **it fails
with specificity**. It does not return "maybe"; it returns a file, a line number,
and a match -- and a wrong answer carrying a line number is far more persuasive
than a wrong answer carrying none. The reviewer was seconds from reporting a rule
violation attached to nothing.

Redone with word boundaries and a positive control -- the same pattern against a
script known to do filesystem I/O, which detected correctly -- both modules came
back clean. The zero became a measured zero rather than an untested one.

The rule being verified said checks must be AST and call-graph based rather than
name grep. The verification of that rule was done by name grep. **Your own
tooling is not exempt from the rules you write for workers**, and a check run in
the act of enforcing a standard is exactly where that exemption feels most
natural.

## "Pre-existing and unrelated" is a claim, not an observation

A worker reported that the full test runner hit *"a pre-existing unrelated
failure"* and ran only the focused suite instead. The reviewer ran the full
runner themselves: 30 files, no failure.

Either transient or mis-attributed -- but the shape is what matters. **A gate
reported as failing-but-unrelated, without isolating it, is exactly how a real
regression passes through wearing someone else's name.** The characterisation
does all the work and none of it is evidence: nobody bisected, nobody reproduced
on the base, nobody named the pre-existing cause.

Prefer a blocked delivery to a characterised one. "I could not get a clean run"
is a fact; "the failure is unrelated" is a diagnosis, and a diagnosis offered to
excuse skipping a gate is the least trustworthy kind.

The reviewer's other check on the same delivery is the companion habit: the fix
added a test-count assertion, and they verified **the assertion itself could
fail** by bumping the expected number -- because a count derived from its own
passing run proves nothing. That is the self-oracle shape arriving inside the fix
for a self-oracle.

## Preserving a variant creates an obligation to handle it everywhere

A parser was taught to round-trip a deprecated import spelling verbatim rather
than normalise it, on the principle that rewriting someone's syntax is not our
call. That was right. But once **two spellings are both legitimate, every
predicate that classifies one must classify the other** -- and a guard added a
day later, by a different author, covered only the modern spelling. The exact
corruption the guard existed to prevent still shipped through the other door.

Nothing in the type system or the tests connects the preservation change to the
validation change. The obligation is created at one site and comes due at every
other, with no mechanism carrying it across.

The fix that matters is structural: make the classification **spelling-agnostic
at one site**, rather than adding a parallel path -- a second path is exactly
where the third divergence lands.

Note the sequence, because it is the fence class twice over: the first fix
fenced a defect as correct, and the second fix fenced *half* a defect as fixed.
Both were caught by an oracle from outside the layer -- the runtime, not the
syntax tree.

### An instrument can also produce a false positive

Every instrument failure above returned a false all-clear. This one went the
other way: a probe reported a case as *refused*, which would have read as an
over-broad guard and sent someone chasing a bug that did not exist. The refusal
was real; the reason was `file_not_found`, because the fixture had been written
to the wrong directory.

A false positive is cheaper than a false all-clear -- it wastes work rather than
hiding a defect -- but it costs the same investigative reflex: **when a result
surprises you in either direction, check the instrument before theorising about
the subject.**

## Presence is not liveness: the config is testimony, the probe is evidence

My survey for unprotected repos read `git remote -v` and classified anything with
a remote as safe. It found five repos with none.

It missed the largest. **1,465 commits, in a repo whose remote is configured and
does not exist** -- `ls-remote` returns "Repository not found." Every push someone
believed was happening had been failing, and the folder held every line of that
night's work.

`git remote -v` renders identically for a working remote and a tombstone. The
config records an *intention*; only the probe establishes the *fact*. Re-running
the survey with a liveness probe against all 30 folders found it immediately, and
the probe costs about twenty seconds for the whole tree.

It also corrected a misreading from earlier the same night: I had hit that same
"Repository not found" while checking a peer's push state and attributed it to my
own SSH access. **An error blamed on your own environment stops being
investigated** -- and it was the actual finding, sitting in plain sight for hours.

The three states get progressively quieter, and the quietest one is invisible to
both checks above it:

| state | presence check | liveness check | ahead-count |
| --- | --- | --- | --- |
| no remote | catches | catches | catches |
| dead remote | **misses** | catches | catches |
| unpushed | **misses** | **misses** | catches |

A repo with a working remote and 83 unpushed commits passes both checks, because
they answer *"is there a remote and does it respond"* and never *"does it have
the work."* **Only the ahead-count measures the property anyone actually cares
about, which is durability rather than configuration.**

General form: a survey for *unprotected* things must probe the protection, not
read its declaration. Backups configured to a dead endpoint, replicas pointing at
a decommissioned host, alerts routed to a closed channel -- all present, all
inert, all indistinguishable from working until something asks them to perform.

### A line that is always there stops being read

My fleet pulse printed `CKTUI MISSING` on every 30-minute wake for eight days. I
read past it every time, because a line that never changes reads as furniture.

Investigating it once found a repo with 34 commits and **no git remote at all**.
That repo led to a survey: five CortexKit repos have no remote, 447 commits
living on exactly one disk, including the only copy of a module currently serving
production traffic.

This is the corollary to the designed-zero rule, aimed at the reader instead of
the gauge: **a signal that is permanently present conveys nothing, whatever it
says.** If a condition is expected to persist, it needs either a resolution path
or a different presentation -- because an alert nobody can act on becomes an
alert nobody reads, and it takes the surrounding lines down with it.

### And check your own ledger before asking

I asked a seat about two blockers. Both were closed -- one of them by *me*, hours
earlier. I had carried a resolved item forward as an open one and offered to
solve a problem that no longer existed, holding a deploy for a defect already
fixed in production.

**A stale entry and a live one render identically on a list.** Nothing about
"open item, waiting seat" distinguishes still-blocked from resolved-and-never-
struck-off -- the same-glyph problem, in my own tracking. A hold that rests on a
stale entry is not caution; it is staleness wearing caution's clothes.

## Is the substituted answer a claim, or the representation of not knowing?

When a fallible step is made infallible, ask what the substitute *asserts*. The
dangerous substitutions are the ones that read as a positive finding.

```rust
pub fn is_equal_to(&self, other: &Self) -> bool {
    self.try_fingerprint().ok() == other.try_fingerprint().ok()
}
```

Fingerprinting fails when a record exceeds a size bound. `.ok()` maps that to
`None` **on both sides**, and `None == None` is `true` -- so two records that both
failed to fingerprint compare *equal*. A revalidation asking "is this still the
same target" is answered **yes** at precisely the moment neither side could be
verified.

That is the swallowed-error class one level up. `catch { return null }` converts
an error into an absent *value*; this converts it into an absent *comparison*,
and comparison of two absences manufactures a claim of sameness out of two
failures. Same family as `unwrap_or(Success)`: both pick the flattering reading
of "I could not tell."

The discriminator generalises past defaults. A substitution that represents *not
knowing* (`None`, a typed error, a refusal) is safe to propagate. A substitution
that represents *an answer* -- success, equality, authorized, unchanged -- is a
fabricated finding, and every caller downstream will treat it as evidence.

**But the signature is not the defect, so you cannot grep this class.** A sweep
of every `is_none_or` / `map_or(true, ..)` / `unwrap_or_default()` site across
three crates found nine candidates, and *seven were the same idiom doing the
opposite work*:

```rust
measured.is_none_or(|value| value > maximum)   // absent measurement => bound exceeded
```

Here "I could not measure the interval" forfeits the containment claim exactly as
"the interval was too long" does. Identical expression shape to the defect,
inverted meaning, and correct -- because the substituted answer is the
conservative one. Whether `is_none_or` fails open or closed depends entirely on
which side of the comparison the guard sits.

So **the sweep produces candidates and the reading produces verdicts.** Two of
the nine meant absence-passes; one of those was clear by construction (the field
is always populated where it matters, and absence means *not applicable* rather
than *unknown*, with a fail-closed sibling one line away), and one was genuinely
ambiguous and got flagged rather than changed.

That last distinction is worth copying: an *unambiguously* wrong helper can be
deleted without a contract decision, because no reading justifies it. One where
"nothing configured, nothing to enforce" is a legitimate reading is a **contract
call, not a cleanup** -- and the tell there was that the status it produces is
named `Accepted`, which reads downstream as *verified*. File it with its trigger:
settle the contract before the first caller, since after that it is a migration.

Worth noting how this one was resolved: **zero callers, so it was deleted rather
than fixed.** Teaching it to refuse would have left two ways to compare the same
records, one of which someone must remember to prefer -- and the correct version
already existed one file over. A helper with legitimate uses gets fixed; a helper
with none gets deleted, and zero callers is the moment deletion is free.

## An expiry check is only as live as the clock it reads

A pairing ceremony's window check consulted a cached `now`, advanced by a
once-per-second countdown task. iOS suspends that task while the app is
backgrounded. So: open the ceremony, background the app, let the window lapse in
real time, foreground -- the countdown never ran, the cached time is stale, **the
window reads live**, and the trust-establishing confirm proceeds.

A fail-open on the step that establishes mutual trust, in a guard whose own doc
comment read *"authoritative (fail-closed)."*

Any deadline judged against a cached clock inherits the liveness of whatever
advances it -- a timer task, a poll loop, an event tick -- and every one of those
can be suspended, starved, or descheduled. Judge against `max(cachedClock,
realClock)`, or read the real clock at the decision point.

## Guards that shadow each other are each individually unfenced

The same probe found two guards that could each be deleted with the full suite
green -- not because they were untested, but because **each was standing behind
the other.** Delete the window check and the state guard refuses; delete the
state guard and the window check refuses. The single test that covered both drove
the clock through a tick that also moved the state, so the two were never
exercised apart.

Coverage of a *conjunction* is not coverage of its terms. To fence N guards on
one path you need N tests, each of which reaches the guard under test with every
other guard satisfied -- and the mutation is what proves you got there.

A corollary that caught a bad test on this same path: the first isolation tests
passed *and* passed against their own mutants, because a third guard refused
before control ever reached the one under test. **If the mutant does not kill
your new test, the test is the suspect.**

## A suite that shrinks and stays green

The sharpest instance of the whole family, and I reproduced it myself rather than
taking it on report:

```
baseline            102 pass, 0 fail
move one schema     100 pass, 0 fail    <- two tests ceased to exist
restored            102 pass, 0 fail
```

Moving a single hash-pinned normative schema out of the tree did not fail two
tests. **It deleted them.** The run still reported zero failures, so a reader
watching CI sees green both times. Only the count moved, and nothing asserts the
count.

So the gate that exists to catch a missing pinned schema reports success when one
goes missing -- and it is reachable by an ordinary mistake: a rename, a vendored
crate, a moved manifest.

This is the designed-zero problem in its purest form: **a check whose absence is
indistinguishable from a check that passed.** Nothing in a normal test framework
prevents it, because a data-driven suite generating cases from files on disk
generates zero cases from a file that isn't there.

**Assert the expected test count.** A suite that can silently shrink is a suite
whose coverage is unfalsifiable, and the count is the only thing that makes
deletion visible. Pair it with loud failure at the load site -- a missing or
malformed input must abort, never return null -- because embedding the data
without fixing the swallow just relocates the same silence.

## The instruments are the least-tested layer

One night's measured ratio: FIVE INSTRUMENT DEFECTS AGAINST ROUGHLY ONE CODE
DEFECT. Not one of the five was found by reading the command; each surfaced only
when a result contradicted something already known.

Treat that as a measurement rather than an anecdote. It says the checking layer
is now the least-tested layer in the system -- exactly where application code sat
before anyone wrote tests for it. And it has a consequence for ordering work: an
instrument defect is UPSTREAM of every finding the instrument produces. A large
coverage enumeration run by an untested enumerator yields a large, confident,
wrong map with nothing to signal which it is.

So when a backlog contains both findings and instruments, THE INSTRUMENTS COME
FIRST, however small they look next to the findings.

The rule that follows: EVERY CHECK SHIPS WITH A PROOF IT CAN FAIL. For a
data-driven check, assert the item count. For a digest check, tamper one blob.
For a coverage enumeration, plant one item known to be uncovered and one known to
be covered, and require both to come back correctly classified. One extra case per
instrument, and it converts "the output looks right" into "this demonstrably
reports the failure it exists to report".

Apply it to throwaway scripts too. A one-off verification script carries the same
weight as a committed test at the moment someone acts on its output, and it has
none of the review that a committed test gets.

### A control must be established by a different method than the thing it guards

The planted case only works if its expected answer comes from somewhere the
instrument cannot also be wrong about.

Picking an item you already believe is uncovered is contaminated WHENEVER THAT
BELIEF CAME FROM A PRIOR RUN OF THE SAME KIND OF ANALYSIS. If a reachability
analysis established that item X is unreachable, and the new enumeration is also
a reachability analysis, then a defect that under-reports marks X uncovered (the
control passes) and under-reports the rest (the finding is wrong) -- and the
control cannot separate those, because it shares the defect. That is the parity
test aligned to the buggy side, one layer up.

So the question is not whether the expected answer is TRUE. It is where the
answer CAME FROM. A dormancy recorded as a design decision at the time is an
independent source and makes a fine control; the same fact arrived at as the
output of an earlier analysis is still true and useless here. Put plainly: A TRUE
FACT ESTABLISHED BY THE METHOD UNDER TEST IS NOT EVIDENCE ABOUT THAT METHOD. A
control's job is not to be right -- it is to be ABLE TO BE WRONG in the specific
way the instrument might be.

INDEPENDENCE HAS TWO LEGS, AND THE SECOND IS THE ONE THAT BITES. Independent
METHOD is visible in how the check is written, so it gets checked. Independent
INPUTS is invisible at the check and has to be traced upstream. Worked case: a
mutation over a materialized corpus looks fully independent of a route analysis --
different method entirely, execution rather than static tracing -- but if THE
CORPUS WAS DERIVED FROM THE ROUTES, an analysis that misses a route yields a
corpus missing the same frames, and the mutation inherits the gap. Different
method, shared input provenance, contaminated anyway.

The same case is worth reading a second way, as a claim rather than a control:
"no materialized frame reaches it" is not "no route can reach it". That is the
narrow-but-true class landing on a piece of EVIDENCE instead of on a check -- the
fact is correct and the proposition it supports is smaller than the use it was
put to.

STRONGEST SHAPE: make the control true BY CONSTRUCTION rather than by prior
finding. Plant a synthetic item with no enforcement site anywhere, verifiable by
a direct read, require it reported uncovered, then remove it. For the covered
direction, pick something a test already exercises end to end, so reachability is
demonstrated by dispatch rather than asserted by analysis.

Same move as tampering a blob rather than picking a blob you believe is already
wrong.

When the synthetic plant becomes load-bearing, control its own premise too. You
establish "no enforcement site anywhere" by searching, and A SEARCH RETURNING
ZERO BECAUSE THE PATTERN IS WRONG LOOKS IDENTICAL TO ONE RETURNING ZERO BECAUSE
THE THING IS ABSENT. Pair it with a positive control at the same scope: search
the same way for an item you know HAS an enforcement site, and confirm you find
it. Otherwise the control rests on an unchecked empty result.

That is three layers of one rule stacked: the enumeration needs a control, the
control needs a source read, the source read needs its own positive control. Each
layer is cheap. The stack is what makes the output mean anything.

### Show someone what you have already concluded

The corrections that matter cluster on settled items, and the person who settled
one is structurally worst placed to re-open it. Not through carelessness: an
answer you produced yourself does not PRESENT as an open question, so "how do we
know that" never surfaces as a thing to ask. Someone who never ran the analysis
has that question available for free, because it is the only one they have.

So the habit is not "get a second opinion on hard problems". Hard problems
already attract scrutiny. It is DELIBERATELY SHOW A SECOND READER THE THINGS YOU
HAVE MARKED DONE -- the closed gate, the settled list, the control you already
chose -- precisely because you have no motivation to look there again.

Worked case: a control choice recorded as settled was contaminated, and the catch
came from the seat that had never performed the analysis establishing it. Read
that as structural rather than as a fact about either reader, or the habit stops
reproducing.

Settled is where nobody looks.

The same property explains why workers catch bad specifications. A worker has no
memory of writing the spec, so an unsatisfiable instruction reads to it as a
contradiction rather than as a decision already made. Three of four unsatisfiable
specs in one campaign died that way. Calling that "workers treat the spec as
falsifiable" credits a DISPOSITION, which makes it a property of the model and
therefore not something you can arrange; it is really THE ABSENCE OF AUTHORSHIP,
which is a property of the READING POSITION and can be arranged deliberately.
Dispositions you hope for. Positions you construct.

TARGETING RULE THAT FOLLOWS: SPEND THE NEXT CHECK ON WHATEVER NOBODY BUT YOU HAS
READ. Not the hardest artifact and not the newest -- the one with exactly one
reader. That is a queryable property rather than a judgement call, which is what
makes it survive a busy week.

AND IT HAS A PREDICTION, which is what makes it a rule rather than an
observation: THE THING THAT DEFINES SCOPE IS ALWAYS SINGLE-READER, BECAUSE
NOBODY REVIEWS A LIST. Two seats applied the rule independently within an hour
and both landed on the artifact deciding what gets checked -- one a hardcoded
module set in a fleet monitor, one a checked-in test inventory behind a gate.
Both reported clean over an incomplete set. Start there.

A count assertion over such a list is not the defence it appears to be. If the
expected number is transcribed FROM the list, it proves no duplicates and no
shrinkage and is structurally incapable of proving COMPLETENESS -- completeness
is a claim about the world, and the number is a claim about the list. Add a
qualifying item, forget the list, and both stay consistent forever. The
denominator has to come from the thing being enumerated, not from the enumeration.

### A fixture whose point is the literal form must carry its bytes verbatim

Parsing a golden vector and re-encoding it before feeding the subject erases the
distinction the vector exists to test, and the assertion then passes forever over
an input the subject cannot receive.

Worked case: a reject-vector carrying `7.0` was fed through a parse-and-re-encode
step whose JSON writer renders an integral double as `7`. The decoder received an
integer, the "this must be refused" assertion passed, and nothing ever presented
a float. The fix is to slice the raw bytes out of the fixture line and use them
unchanged.

The hazard is library-specific and that is what makes it dangerous. One JSON
library preserved the float discriminant through the same round trip; two others
collapsed it. So the same test shape is sound in one language and vacuous in
another, and reading the code cannot tell you which.

WHERE A ROUND TRIP IS KEPT DELIBERATELY, ENFORCE THE LIBRARY PROPERTY RATHER THAN
RECORDING IT. A comment saying "this works because the encoder preserves floats"
does not fail when the encoder stops preserving floats; a test asserting that
`7.0` survives re-encoding as `7.0` fails on exactly that day, with a message
naming why. A dependency that is recorded is not guarded.

A test that pins a library's behaviour is a DIFFERENT SPECIES from the rest of a
suite, and worth recognising as a category. Everything else tests your code; this
tests a DEPENDENCY'S PROMISE your code silently relies on. It therefore fails on a
dependency bump rather than on a change of yours, and whoever sees it red will not
have touched anything near it -- so its failure message must name the REMEDY, not
the symptom.

The pattern generalises past JSON. Anywhere behaviour depends on a library
preserving something it never promised -- ordering, precision, encoding form,
iteration stability -- there are two options: REMOVE THE DEPENDENCE, or PIN IT
WITH A TEST. Commenting it is the third option that looks like the second. The
argument for pinning over commenting, in one line: THE COMMENT IS READ BY SOMEONE
WHO ALREADY SUSPECTS, THE ASSERTION IS READ BY SOMEONE WHO DOES NOT.

Prefer removal wherever it is cheap; pinning is for where you WANT the dependence
and need it to fail loudly if it dies. A sweep that comes back "mostly removed,
the unavoidable one pinned" is the healthy shape.

The same hazard appears TEST-SIDE, where it is easy to wave off. A test that
indexes position `[0]` on a query with no ORDER BY passes because the engine
happens to scan in insertion order -- an unstated promise, relied on by an
assertion. The fix is NOT to add an ORDER BY to a genuinely unordered query,
which changes production to satisfy a test; it is to assert over the SET rather
than by position, so the assertion matches the contract that actually exists.
Worth doing even when the risk looks small, because the failure mode is the
expensive kind: AN INTERMITTENT FAILURE GETS RE-RUN AND DISMISSED, burning
attention repeatedly and teaching the reader to distrust the suite, where a test
that fails always gets fixed once.

RELATED, from the same exchange: A LIMITATION ASSERTED AS A DIVERGENCE IS SAFE;
THE SAME LIMITATION ALLOWED TO PASS AS AGREEMENT IS THE DANGEROUS ONE. Where one
implementation genuinely cannot enforce a rule, a parity suite should require the
difference and say why, rather than letting the two sides quietly agree. Most
parity suites default the other way, because agreement is what they are built to
find.

### Re-run a new class on whatever you did not consider the subject

A newly named class gets aimed at whatever you already think of as the code.
Tests, tooling, fixtures, and scripts are reflexively filed as INSTRUMENTS rather
than as subjects, which is exactly where the class hides best -- nobody reviews
them with the same eye.

Three instances in one night, two repos: a rule about transcribed lists was
written, and the transcribed list turned up in the monitoring tool whose job was
catching that class. A rule about unstated library promises was applied across
production and stopped at the test directory, where an assertion indexing
position `[0]` relied on the engine scanning in insertion order. In both cases
the author had the category fully in mind and did not point it at themselves.

So: NAME THE CLASS, THEN IMMEDIATELY RE-RUN IT ON EVERYTHING YOU EXCLUDED FROM
THE SUBJECT. It is mechanical and takes minutes.

A NEGATIVE RESULT FROM AN AUDIT THAT HIDES ITS BOUNDARIES IS INDISTINGUISHABLE
FROM ONE THAT NEVER LOOKED. "No hazards found" is unfalsifiable unless the sweep
states its population, its coverage, and WHERE IT STOPPED. A clean result reading
"133 of 133 shipped sites, stopped at this dependency's API boundary" can be
checked; the same conclusion without those numbers cannot be distinguished from a
sweep that quietly ended early.

Two practices that make a null trustworthy, both from the same audit: GIVE THE
SWEEP ITS CENSUS AS FACTS rather than having it re-derive them, which removes the
failure mode where the sweep's own counting error silently narrows the
population; and VALIDATE THE COUNTING INSTRUMENT AT BOTH ENDS before trusting it
-- confirm it returns the known count on a file you have already counted, and
zero on a file you know has none.

AND SCOPE A SEVERITY TO A CONFIGURATION. Eight flagged sites were real for a
deployment we do not build, labelled "unusual but real" -- not wrong, UNSCOPED,
which is what makes a reader act on someone else's system. Check linkage before
accepting reachability.

A related discriminator, because the obvious version of the rule over-reports:
for lock poisoning THE QUESTION IS LIFETIME, NOT SHARING. Poisoning requires the
lock to OUTLIVE the panic, so a mutex owned by one call frame and destroyed
during unwind yields a FAILED OPERATION, not a poisoned system -- even where the
code reads alarmingly, such as a caller-supplied callback invoked while holding
the guard. Ask whether any other thread can ever take this lock again.

AND THE ORDER OF THE FILTERS IS ITSELF THE FINDING. That sweep had two
mechanism-derived scope reductions: lock FLAVOR (only `std::sync` poisons, which
drops whole layers before reading a line) and lock LIFETIME (stack-local cannot
poison). Both are free. Applied in the wrong order they cost a hundred
critical-section reads to discover most could never have mattered.

Generalised: WHEN A SWEEP HAS SEVERAL MECHANISM-DERIVED SCOPE REDUCTIONS, APPLY
THE ONE THAT REQUIRES THE LEAST READING FIRST. Flavour is a type check, lifetime
is a declaration-site check, the critical section is prose. Sorting filters by how
little understanding they demand is the cheap ordering, and it gets skipped
because the interesting filter feels like the one to start with.

AND A DECISION NOT TO FIX BELONGS AT THE SITE TOO. An accepted cost with no
comment DOES NOT READ AS NEUTRAL -- it reads as an oversight, so the next reader
either undoes a deliberate decision or re-derives the whole analysis. Same class
as a deliberate refusal decaying into an apparent bug, arriving through a
different door. Notes serve the auditor; comments serve the maintainer, and only
one of them is present when the code changes.

What makes such a comment sufficient rather than merely present: THE COST, ITS
MEASURED BOUND, AND WHAT A FIX WOULD HAVE TO PRESERVE. The first two explain; the
third CONSTRAINS. "This is deliberate" is a claim, and the next change cannot be
tested against it.

AND THE DOCUMENT UNIT MUST MATCH THE DECISION UNIT: A TRADEOFF DOCUMENTED IN
HALVES IS AN INVITATION TO OPTIMISE ONE HALF. Two costs that trade against each
other read as two free wins when written separately, and the reader who finds one
has no reason to look for the other. They are one comment even when they live in
two places.

MEASURE AN AUDIT IN CONSTRAINTS LANDED AT SITES, NOT FINDINGS REPORTED. A report
is a claim about a moment; a comment at the site is a constraint that travels
with the code. The census, the flags and the negative result are all invalidated
by the next edit to those functions, and none of them are visible to the person
making that edit. In the sweep above, three comments stating why particular
guards cannot poison were the entire durable yield -- they make a future fallible
addition legible AS a change to an invariant, to someone who never read the
audit. Eleven flags yielded nothing.

AN ARTIFACT CAN BE FRESH AND STILL BE BROKEN BY ITS ENVIRONMENT. Every identity
rung -- mtime, inode, symbol presence -- answers "is this the artifact I built",
and all of them PASS on a binary whose problem is external. Worked case: a shim
execs a helper through a path baked in at compile time; the build directory it
pointed at was reclaimed, so the shim was current, correctly signed, correctly
placed, and could not run. Identity checks cannot see this because identity is
exactly what is intact.

For any binary that execs another, verify THE TARGET, and verify it as EXECUTABLE
rather than merely present, since executability is the actual requirement.

THE MISREPORTED SYMPTOM IS THE EXPENSIVE HALF: the failure surfaced as "handshake
timed out", sending the reader after concurrency and startup ordering. A fast
exec failure and a real timeout are separated by ONE MEASUREMENT -- how long did
it take to fail -- and that question is worth asking before any theory.

AND THE PROBE FOR IT NEEDS ITS OWN GUARD. Extracting a baked path from a Rust
binary with `strings` will MANUFACTURE PLAUSIBLE PATHS, because literals are
packed contiguously and a greedy match glues adjacent ones together. The
fabricated path does not exist, which is exactly the finding you were hoping to
make. Test the prefixes of a GONE result before believing it.

DEPLOY IS A WIRING GAP, NOT AN OPERATIONAL CHORE. Merge is a producer and deploy
is its consumer, so a merged-but-never-shipped change is the producer-no-consumer
case one layer out from the codebase. It hides for the same reason the in-process
version does: EVERY PARTICIPANT IS INDIVIDUALLY ALIVE AND CORRECT. The branch
merged, CI passed, the binary runs, the module reports healthy -- nothing is at
fault at any single point, so nothing errors, and the only detector is something
that looks at the whole path and asks whether the END moved.

The four verdicts apply unchanged, including the fourth: a binary deliberately not
deployed is a real deliberate-refusal case, which is why the finding is a question
to the owner rather than a defect report.

A CONFLATION IS INVISIBLE WHILE THE TWO THINGS HAPPEN TO COINCIDE. A deploy check
asked "which changed files reach this binary" and answered it repo-wide. Thirteen
of fourteen fleet repos ship exactly one binary, so repo-wide and binary-wide
agreed everywhere and the distinction could not be seen -- until a repo carried
code outside its deployed dependency graph, and unrelated commits were attributed
to the artifact.

The general shape: WHERE A CHECK USES ONE SCOPE AS A PROXY FOR ANOTHER, IT IS
CORRECT EXACTLY UNTIL THE FIRST CASE WHERE THEY DIFFER, AND THAT CASE ARRIVES
WITHOUT ANNOUNCEMENT. Ask what the check is really scoped to (repo vs artifact,
file vs run, process vs machine) and whether anything guarantees the two stay
aligned. Usually nothing does; they were merely equal when it was written.

Derive the narrower scope from a source that cannot drift -- here cargo's own
path-dependency closure rather than a maintained list -- and choose the error
direction deliberately: crate granularity over-reports where target granularity
would under-report, and for a deploy gap over-reporting is the safe side.

SIZE AN OBSERVATION WINDOW AGAINST THE SLOWEST RATE THAT STILL COUNTS AS
PROGRESS, NOT THE RATE YOU HAPPENED TO MEASURE. A stall detector sampling 30
seconds was correct against the ~20/min rate it was written for; the first
generation that ran at ~3/min expects fewer than two events in that window, so a
zero is ordinary and the detector reported a stall on a system working fine. The
fix is that A ZERO WINDOW EXTENDS RATHER THAN CONCLUDES, and a slow subject
reports as slow rather than as stopped. Same failure as a poll-count deadline
that shrinks under the load which makes the operation slower: THE THRESHOLD WAS
CALIBRATED ON ONE OPERATING POINT AND TREATED AS ABSOLUTE.

STATE THE RULE AT THE GRANULARITY THAT MAKES IT TRUE. A corpus of refusals needs
an accept arm in THE SAME HARNESS AND THE SAME RUN -- not in the same file. A
refusal-only file paired with an accept-only sibling, both loaded by both
consumers, satisfies it; a split RUN would not. Had the rule been written at file
granularity it would have flagged a sound design, and A RULE THAT CRIES WOLF GETS
DISABLED BY THE PERSON IT WAS WRITTEN FOR.

TWO COPIES ARE FINE IF NEITHER CAN CHANGE SILENTLY. The problem was never the
duplication, it was the silence -- so a digest pinned on BOTH sides converts a
synchronisation problem into a build failure, which is the only kind of
synchronisation that survives nobody watching. A shared definition buys the same
property by removing the copies, which is more machinery for an identical
guarantee; prefer it for wire types, prefer the pin-pair for test fixtures.

PAIR THE DIGEST WITH ASSERTIONS THAT NAME THE LOAD-BEARING CONTENT, because A
DIGEST SAYS CHANGED AND NOT WHAT. Re-syncing a fixture that had grown by three
entries failed three tests asserting an exact block count and an exact lane list,
and FAILING ON GROWTH IS INDISTINGUISHABLE FROM FAILING ON CORRUPTION at the
moment you read the output. Asserting which kinds decode typed and which decode
opaque -- with both sets required non-empty, so the check cannot pass by being
constant in one direction -- survives the next addition and is what the client
actually depends on.

BUT A COUNT IS THE RIGHT ASSERTION WHEN THE COUNT IS THE CONTRACT, and the two
cases sit side by side in one repo. A data-driven suite over a published corpus
MUST assert its item count and file set, because silent shrinkage is exactly the
failure that takes a suite from 102 passing to 100 with nothing red -- there the
census IS the property, namely that every published vector is exercised.

THE DISCRIMINATOR IS WHETHER GROWTH IS EXPECTED. A fixture the producer extends
over time must not assert its size; a corpus where every member must be consumed
must assert exactly that. Same assertion, opposite verdicts, and the separating
question is whether a new entry here would be ROUTINE or ALARMING.

AND EXPECT THE FIRST PROBE TO BE WRONG. Sweeping eleven vector files for accept
arms by matching one key returned zero on a file whose schema marks acceptance by
the ABSENCE of an error key -- a crude probe returning zero is indistinguishable
from a real zero, and here it would have produced a CONFIDENT FALSE FINDING
rather than a null. A CORPUS-WIDE PROBE ASSUMES A UNIFORM SCHEMA THAT MULTI-FILE
CORPORA RARELY HAVE; read each schema before trusting any count. Note also that
the clean result was only worth something because the probe was wrong once and
someone noticed: A SWEEP THAT NEVER PRODUCED A SUSPICIOUS INTERMEDIATE IS USUALLY
A SWEEP THAT WAS NOT MEASURING.

A TELL WORTH KNOWING, from the same case: WHEN TWO CALL SITES DISAGREE WITH A
THIRD, THE ODD ONE OUT IS USUALLY THE ONE THAT DRIFTED. It is cheap because it
needs no judgement about which form is correct -- only that a majority exists.
The trap is that DRIFT PRODUCES LOCAL AGREEMENT: two wrong call sites look
consistent with each other and read as a convention, while the correct one reads
as the outlier.

AND KEEP THE TWO ARGUMENTS SEPARATE: rejecting a bad remedy is not a reason to
leave a case alone. "Adding an ORDER BY to a genuinely unordered query would be
cargo-culting" was correct, and it got used to justify recording the case rather
than fixing the assertion, which was the actual remedy. A GOOD REASON TO REJECT
ONE FIX IS NOT A REASON TO REJECT ALL OF THEM.

### Two constructions over the same ground diff for free

When two mechanisms cover overlapping scope by DIFFERENT construction -- one
scanning a directory, one reading a checked-in list -- their disagreement is a
control you did not have to build, and it is independent on both legs: different
method, different inputs.

This is worth more than re-deriving the list once. Deriving fixes today's drift;
the diff keeps working after someone reintroduces a list for a good reason. Where
the pair already exists, diffing it costs almost nothing and is usually the only
fully independent control available.

Before reporting the disagreement as a hole, establish the blast radius: an item
missing from one gate but covered by the other is INCOMPLETE BY OMISSION, not
UNRUN. Different fix, different urgency. Size it before naming its severity.

It is also why the layered control stack works at all: each layer is read by
something that did not produce the layer beneath it.

## Agreement suppresses the audit that disagreement provokes

The familiar rule is that a measurement contradicting your theory is the finding
until you can explain the instrument's failure mechanism. It has a mirror that is
harder to act on: WHEN A MEASUREMENT AGREES WITH YOUR THEORY, IT IS OWED THE SAME
AUDIT.

The asymmetry is structural rather than a matter of discipline. A disagreeing
measurement ANNOUNCES ITSELF -- it stops you, it demands an explanation, and the
friction is the prompt. An agreeing one produces no friction, so the audit is not
skipped after consideration; it is never considered. That is why one person can
dismiss a correct measurement and accept an incorrect one in the same session
without any inconsistency: both times the question was whether the number agreed,
not what the number measured.

Worked case: a memory investigation sampled RSS fifty times and ran a
first-half/second-half rate comparison to rule out cache warm-up -- the right
control, and one most people skip. RSS on that platform counts shared framework
pages, reading 322 MB against an actual physical footprint of 51.6 MB. NO AMOUNT
OF ANALYTICAL CARE ON TOP OF A MISMEASURED QUANTITY RECOVERS THE ANSWER, and the
care makes the wrong answer more convincing rather than less.

A PEAK COUNTER READ AS A LIVE ONE IS THE SAME CLASS AND IS WORTH KNOWING BY
SIGHT. Tools print peak and current on adjacent lines; a careless match takes the
peak, which never decreases by construction, and manufactures a leak out of a
process reclaiming perfectly. The monotone series is indistinguishable from a real
leak, so it cannot be diagnosed from the series alone. The discriminator is A
FORCED DROP: trigger a reclaim and see whether the number CAN fall.

## Control the denominator, not only the result

A check reporting zero failures over zero items reads exactly like a clean pass.

My first digest pass over an evidence document found zero JSON blocks and
reported zero mismatches, because the document was raw JSON and the pattern
looked for markdown code fences that did not exist. The result line was true and
the check was empty.

Any check that iterates must assert its ITEM COUNT before its failure count means
anything. Same shape as a data-driven suite that silently shrinks when its input
files move: the pass count drops, nothing fails, CI stays green.

## Before specifying a check, run each of its rules by hand against its subject

Two unsatisfiable specifications in one night, from the same author, from the
same gap. The first: a scope list that had gone stale. The second: a purity rule
forbidding filesystem access in code that performs it at two known call sites.
Both rules were written into a task prompt and **never once evaluated against the
module they govern.**

The existing habit -- checking that the artifacts a task references actually
exist -- catches missing subjects. It does not catch *a rule the existing code
already violates*. That is a different check and it costs about five minutes per
rule.

### And when the exception looks cheap, read the code it excuses

The proposed narrow exception was justified on the grounds that the reads were
hash-pinned and therefore not arbitrary capability. Reading the call sites
killed that: **both swallow their errors** (`catch { return null }`,
`read_to_string(...).ok()?`), so a missing or moved file does not fail -- the
parser proceeds as though the schema did not exist. The hash is verified in the
repository at gate time; the runtime path verifies nothing.

So the pinning that made the exception sound was not enforced where the exception
needed it. The real choice was never strict-versus-pragmatic; it was between
removing the dependency and keeping a **silent-degradation path inside the
artifact whose entire purpose is independence from its environment.**

Approval of a gate is not a promise the file is frozen. A gate that passed on
code containing a rule violation did not prove the violation absent -- it proved
the gate did not look.

## Check coverage in both directions, or neither check is tight

An authority document listing which paths a campaign was permitted to touch needs
two independent checks, and each catches what the other cannot:

- **Forward**: every path in the range is authorized by some row. Catches
  *omissions* -- work nobody accounted for.
- **Reverse**: every row corresponds to a path actually in the range. Catches
  *inventions* -- rows added to make the check pass.

Forward-only accepts a document full of phantom rows. Reverse-only accepts one
missing real work. Neither direction is slack.

Running the reverse check found exactly one phantom: **the amendment's own path.**
It authorized every file in the campaign except itself. Only the direction that
looks for unmatched rows could see it -- and an authority document that cannot
account for itself is the same self-exclusion problem the normative index solves,
arriving one layer up.

The general form: any artifact asserting completeness over a set should be
checked in both directions, and should account for itself.

## A number that reconciles is not a set that matches

I recomputed a peer's count of 77 unauthorized paths, got 84, and resolved the
gap cleanly: 84 minus the 7 baseline rows my filter had omitted. The arithmetic
closed exactly, so I stopped -- and described the group as "the 7 baseline files
plus the README." The README **is** one of the seven. The eighth file was a
normative appendix added mid-campaign, and it was the most load-bearing file in
the whole effort: the one authorized specifically to stop a generator inferring
schemas from field names and validating against itself.

So the file that exists to prevent the self-oracle defect was the one missing
from the authority list, and a clean reconciliation is what stopped me looking.

**When a count resolves neatly, that is the moment to enumerate the members, not
the moment to stop.** A plausible grouping that fixes the arithmetic sounds like
knowledge and can be entirely invented.

## Mechanism-real is not cost-real

A hunter reported redundant work on a hot path: a manifest fingerprint computed
before the fast path that would skip it, and a warm-key call made twice per
configure. Both true, both verified at source, and the report was scrupulous
about claiming no timing number.

Measured: one directory enumeration plus n+1 metadata probes, 0.03-0.07ms warm,
0.66ms cold -- against a **twelve second** deadline. Every syscall it counted is
genuinely there and the total is noise.

The alternative was a targeted fix carrying an invalidation contract whose
failure mode is a stale fingerprint preserving cross-package edges after a
manifest change: **unmeasurable latency traded for a correctness hazard.**
Declining was both the cheaper and the safer call, which is not the usual shape
of that tradeoff.

One command settled it. Require a cost number before accepting a performance
finding, and treat "the redundant work is real" as the beginning of the argument
rather than the end of it.

## A scope list predicted in advance goes stale the moment you authorize an exception

A campaign draft enumerated every path each slice was permitted to touch. Months
of gated decisions later, 77 changed paths sat outside that list -- a frame
materializer authorized as a prerequisite, five normative amendments each ruled
on individually, a test-runner consolidation, a coverage generator. **Every one
was individually approved; nothing updated the enumeration.**

So the verifier whose job was to enforce the scope became unsatisfiable: written
faithfully it must reject the very tree it was built to certify.

The trap it sets is worse than the staleness. The obvious repair is to write the
verifier with an allowlist matching what the tree actually contains -- and that
is a **verifier calibrated to its subject**, in the one artifact whose entire job
is independence from what it checks. It would exit 0 forever and mean nothing.

Two durable fixes: update the scope list *at authorization time*, not at
verification time; and mark it as **derived from gate decisions** rather than
predicted in advance, so the next reader knows which direction the drift runs.

## When a tool refuses, it may be right and you may be wrong

I ruled a campaign closeable on prose and an incomplete search. The work graph
was refusing to close it -- three verify leaves still open -- and I never opened
the system that tracks completion before ruling on completion. The refusing tool
was correct and the adjudicator was not.

A tool that blocks you is easy to route around precisely when you are confident,
and confidence is what a careful-looking search produces. Before overriding a
refusal, find out what it knows that you don't.

The same investigation surfaced the work-graph version of the designed-zero
problem: a leaf marked **done with zero files changed**, its delivery note saying
the prerequisite was unavailable. The prerequisite landed later by another route
and nothing revisited it. On the board, *"blocked, did nothing"* and *"delivered"*
render identically -- so a done leaf with an empty delivery and a stated blocker
should not read as complete.

## Four ways to get a trustworthy-looking empty result

All four were hit in a single night across the fleet, and they share one defence:
**a positive control at the same scope as the query.**

1. A query over one **relation** answers only that relation.
2. A lookup in one **store** answers only that store.
3. A query resolving by the wrong **key** returns a clean, confident zero.
4. A **control scoped more narrowly than the query it guards** proves nothing.

The fourth is the subtlest, because the control existed and was run. The first
three fail loudly the moment you look at them; a mis-scoped control *passes* and
reports success. In the live case it was rescued by an inconsistency noticed by
accident -- which is luck, not method.

## A control scoped more narrowly than the query it guards proves nothing

A control is supposed to prove the instrument can return the other answer. But if
the control searches a *smaller region* than the real query, it can fail for a
reason that has nothing to do with the instrument -- and a failed control reads
as "the search is broken" when it actually means "you looked in the wrong place."

The case: checking whether a slice identifier existed, with a control grepping a
known term in `docs/`. The control came back empty, which by the rule nulls
everything after it. It was rescued only because a later grep in the same
directory hit a real line -- the *contradiction* proved the control wrong rather
than the finding absent. The known term lived in `prompts/`, not `docs/`.

So the control must cover at least the region the query covers. And note what
saved it: not the control, but an inconsistency noticed by accident. **A control
can fail silently in exactly the way it exists to prevent.**

## A control cannot catch an error in what counts as the boundary

The controls in this document check that a search can return both answers. They
do not check that the search is looking at the right *region* -- and when the
defect is in the boundary definition, a known-present instance and a known-absent
region both sit inside the blind spot.

The case: censusing production `.lock().unwrap()` sites required excluding test
code. One pass excluded whole files whose lock sites sat inside `#[cfg(test)]
mod` blocks. But `#[cfg(test)]` also attaches to *individual functions at module
scope*, outside any test module -- invisible to a span-based filter, because the
attribute is on the item rather than an enclosing block. A file reported clean
while shipping three real locks. Two seats got this wrong in opposite directions:
one counted shipped code as tests, the other excluded test code as shipped.

So when a filter defines a region, enumerate the ways membership in that region
can be expressed before trusting either direction of the result. Both-ends
controls will pass regardless.

**And prefer the claim that is load-bearing to the claim that is true.** "Zero
lock sites" was a fact about a grep. "These three critical sections cannot panic
while holding the guard" is a fact about the code. The severity assessment should
have rested on the second all along -- when the count turned out wrong, the
conclusion survived only because someone went and read the sections. Prefer
evidence about the mechanism over evidence about its absence.

## Your own cleanup can manufacture the evidence you are chasing

The two most expensive investigations of the night were aimed at symptoms the
harness produced itself. A test's `rmSync` teardown deleted a directory, and the
resulting error read as "downloaded binary identity changed" -- a plausible
supply-chain signal. A `SIGTERM` in cleanup produced "settled right after the
deadline" -- a plausible race. Both hypotheses were aimed at exactly what the
error said, and in both cases the error was written by the cleanup path.

This is worse than a broken instrument, because a broken instrument gives you a
wrong *answer* while this gives you a wrong *question* -- and every subsequent
measurement, however careful, is aimed at a phantom.

When an error appears near the end of a test, check what teardown does before
theorising about what the code does. Specifically: does cleanup delete, signal,
unmount, or revoke anything the error mentions? If so, that is the first
hypothesis, not the last.

Same family, quieter: **a command that runs, prints something, and exits 0
without doing the work.** `sample` without privileges emits no data and succeeds.
`bash` on a `.ts` file resolves to ImageMagick's `import`, prints usage, exits 0.
The shadowing set worth knowing: `import`, `test`, `time`, `sample`, `link`.

## A check that cannot pass is worse than one that cannot fail

We spend most of our attention on checks that can't fail -- the guard with no
negative vector, the assertion that would hold on any input. But a check that
cannot *pass* is the more dangerous defect, because it produces **confident
refusals**.

The live case: verifying a candidate binary contained a fix by grepping its
strings anchored as `^literal$`. Rust packs string literals contiguously, so no
literal sits alone on a line and the check returned 0 for *every* binary --
including the control string the running module was printing at that moment. Left
unchecked it would have concluded the candidate lacked the fix, and refusing to
deploy would have looked like diligence.

A false pass gets caught eventually by the thing it failed to prevent. A false
refusal is self-justifying: it blocks the change, nothing breaks, and the
refusal looks vindicated. Both directions need the control.

## A pipeline reports the last command's exit code, not the work's

Twice in one deploy: `cargo build ... | tail -5` returned **exit 0** while cargo
had failed on a wrong package name, and `bash script.ts || bun script.ts` ran
ImageMagick's `import` binary, exited 0, and never reached the fallback. Both
printed an error and both reported success.

Any build, test, or check piped into a formatter reports the *formatter's*
success. Use `set -o pipefail`, or check the real command's status before
formatting its output. The failure is silent by construction: the error text is
right there on screen while the exit code says fine, and automation reads the
exit code.

**A fix can rebuild the very class it closes, one level up.** Twice in one night:
a reclaim marker introduced to end signal-dependence was specified as
deletable-by-consumers, which made it shared mutable state between two daemons --
and the unlink race's loser sees the signal vanish mid-read, silently, which is
the original missing-signal leak wearing a new hat. Both instances were caught by
someone tracing the *composed* behaviour rather than reviewing the change.

A corollary worth keeping: **the module that writes a fact owns its lifecycle.**
A consumer deleting inside another module's tree converts one unlink today into
whose-prune-ran-last archaeology in six months.

**A fix does not inherit the confidence of what it repaired.** The path anchor in
that check genuinely fixed the multi-row problem *and* genuinely created a false
all-clear — and neither was findable by reasoning about the other. It took running
a control that had no reason to fail. The anchor is still right; it moved the
failure from a direction that wastes a redeploy to a direction that blesses a dead
module. So **every fix deserves its own negative controls**, derived from what the
fixed check now asks rather than from what the old one got wrong.

And a test worth applying to any banked procedure: **could someone copy this out
of the note and have it work?** That is a different question from whether the
entry is accurate — a doctrine entry listing four guards in prose passes a casual
read and still produces a broken command. Only the first question matters for
something whose whole purpose is to travel.

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
