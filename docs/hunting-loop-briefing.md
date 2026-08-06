# Briefing a hunting loop

A hunting loop points a worker at a subsystem and asks for one defect, with no
code changes. This is how to aim it. Everything here is backed by one loop that
ran sixteen rounds and shipped twenty-five defects, so the ordering below is
measured rather than argued: the same lane and the same model produced four
times the consequence per finding once the targeting changed.

Authored by the ai-provider-quota seat; kept here because it is fleet-wide
method, not one module's story. That repo's `docs/provider-invariants.md` is the
worked example of what a loop leaves behind.

## How to read this, and what it has become

This file is now ~4,000 lines across ~90 sections, and it is honest to say that
only the first part is an INSTRUMENT. The checklist below is meant to be run:
numbered questions, grouped by when they apply, each naming where it fails. If you
read nothing else, read that.

Everything after it is a CASE BOOK. Each section is one defect that actually
happened, with the reasoning that found it and, where it applies, the correction
that followed. It is not organised for lookup and it will not surface the relevant
rule to someone who does not already suspect it -- which is the standing test this
document fails by design, because a case book's job is to be read once and change
what you notice, not to be consulted.

SO THE TWO HALVES HAVE DIFFERENT FAILURE MODES. The checklist rots if a rule lands
in the prose without a row (checked periodically; the row must trace to a
section). The case book rots if it grows past reading -- and it is already past it.
A future pass should cut the cases that only restate a rule the checklist already
carries, and keep the ones whose VALUE IS THE CONCRETE FAILURE: the measurement
that refuted an assumption, the instrument that returned a plausible wrong answer,
the fix that rebuilt the class one level up. Those cannot be compressed into a
rule without losing the thing that makes the rule believable.

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
| 9a | Asserting an *upper* bound on work done — at most one call, no more than N writes? Assert a lower bound too | The cheapest way to satisfy "at most one" is to do none, and doing nothing is what a broken path looks like |
| 9b | Reading a status surface: does its failure path emit the same field names as its success path? | Zero-because-healthy and zero-because-the-read-failed are indistinguishable by value; only key presence separates them |
| 10 | When a mutation reddens something, which test died? | Three tests named for other things means defended by accident |
| 10f | Is anything in the suite keyed on the *file* rather than the *behaviour* — a self-digest, a source-text assertion, a snapshot over source? | It reddens on any edit, so a failure count reads as a clean catch while the clause stays unfenced |
| 10g | Did the mutation actually *apply*? | A substitution that matched nothing looks exactly like a guard that was never reached |
| 10a | When a probe is *expected* to fail, did the failing phase's label appear in the output? | A timeout, a crash and a genuine finding all exit nonzero |
| 10b | Before writing off a measurement as contaminated, do its neighbours agree? | An aggregate caveat silently retires a true finding |
| 10c | For a causal claim: did you run the control arm, with only the one variable differing? | An uncontrolled measurement is an observation, not an attribution |
| 10d | Does your "independent" check differ in *premise*, or only in whose hands ran it? | Re-running someone's query is a transcription check |
| 10e | Does the query you ran answer the question you are asking of it? | One relation's answer quietly supporting a claim about another |
| 11 | Did the new CI step *execute*, or was the run cancelled before reaching it? | A green list showing passes for runs that never ran the new logic |
| 11a | Would this check have failed *before* the fix? | If it was already green on the broken box it is measuring something else — the easy check sits one rung below the one that matters |
| 11b | For a deploy marker: what candidate did you *reject*? | A topical, specific, new-sounding string that reads identically in both binaries certifies a swap that never happened |

Before calling a class closed:

| # | Check | Where it fails |
|---|-------|----------------|
| 12 | Did you sweep the *property*, or re-check the instance that revealed it? | The second guard of a pair stays undefended |
| 13 | For each branch pair (simple case / full case): does every condition in one have a counterpart in the other? | The richer path carries the consequences and the thinner test |
| 14 | Does a condition that suppresses output also suppress the thing that would report the suppression? | Alarm and backstop removed by one condition |
| 15 | Every consumer of your wire enumerated — not just the ones who asked? | Settling with whoever asked is guard-the-instance again |
| 16 | Does each counter you publish say what its anomaly *looks like*? | A gauge nobody reads as a detector |
| 17 | Would a *new entry* in the thing you are asserting over be routine or alarming? | Routine and you asserted a count: healthy growth reads as breakage. Alarming and you asserted a property: silent shrinkage reads as green |
| 18 | What does this test double *refuse* — and can it refuse *selectively*? | A route that never refuses certifies both answers; a rejection that is all-or-nothing makes every partial-failure interleaving unreachable |
| 19 | For a capability grant: what can the granted set do *in combination*? | Rows enter a table one at a time and are read back one at a time, so a composition has no moment at which anyone looks at it |
| 20 | Did you write down the *invariant* or the *remedy*? | A remedy answers the site that broke; an invariant makes the next question mechanical and finds the siblings |
| 21 | For each step in a runbook or checklist: can you name the surface and the field that answers it? | A step nobody traced to a real field is prose, and the operator finds an adjacent signal and reads it as the check |
| 22 | Is the key you store a record under at least as broad as the broadest statement stored under it? | A statement about an account, keyed by device, fences one device; the siblings walk through, and every key component is stable so a mutability check passes it |
| 23 | Where one side produces artifacts the other must consume: which of theirs does nothing of yours read? | Erosion leaves a deletion someone can review; a gap that never closed leaves no diff at all, and the reachability guard that proves the directory resolves reads as proving coverage of it |
| 24 | For a mechanism that produces a state: what does it do *in* that state? | Reviewers check what it does in the state that triggers it; the behaviour after it fires goes unspecified, so a fence blocks the tightening it should permit and a probe keeps running with nothing left to distinguish |
| 25 | When a mutant survives: did the suite miss the behaviour, or did the mutation never happen? | Both read as "unproven". The second is a working bug that just demonstrated itself and got recorded as absence of evidence |
| 26 | For each guard: is it checking the thing it protects, or something that merely correlates with it? | The correlation holds until the surrounding procedure changes shape, then the guard refuses the safe case and permits the dangerous one without a line of it changing |
| 27 | Did an unverified premise *close* a direction rather than open one? | A wrong premise that opens gets tested by whoever builds it; one that closes is never tested, because nothing downstream exists to fail |
| 28 | Did repairing the instrument invalidate the safety readings taken with the broken one? | A broken tool reports "nothing here" and a fixed one reports what is there; a check carried across the repair was answered by the broken version |
| 29 | Was the control run in a state where the failure it guards against *could* occur? | A control run where the fault is impossible proves the check runs, not that it can detect — and it passes for the right reason, so nothing looks wrong |
| 30 | Having tightened one rule of a test double, did you enumerate its others? | A double is a set of independent permissions; fixing one teaches nothing about the rest, and each remaining one certifies a different broken client |
| 31 | Did you verify the target, or only properties of the target you assumed? | Every property can be true of the wrong endpoint; a check whose output omits what it was pointed at cannot expose a wrong target |
| 32 | Does your change break an invariant that nothing currently reads? | No test fails and no alarm fires, because the only thing that would object does not exist yet — the cost lands on whoever writes the first reader |
| 33 | When a filter matches nothing, does the unmatched input flow through as data? | A filter that fails open does not merely lose rows — it turns headers and framing into records, and whether that reads as loud or silent is an accident of content |
| 34 | Does your detector distinguish the *causes* of the state it detects, or only the state? | A correct detector fires correctly on an event it cannot tell apart from another — absence cannot separate deleted from moved, and the consequence lands on the benign cause |
| 35 | Does an await inside a select arm stop the other arms being polled? | The loop has left the select, so a cheaper outcome arriving on another arm waits out the expensive one's full timeout |
| 36 | Before reporting evidence missing, did you check the tools you already run? | An absence found by searching the wrong kind of store is a fact about the search; your own monitoring may already read the thing you are about to escalate |
| 37 | Does the column name describe what the column counts? | A narrower-than-it-sounds name reads as a defect to anyone who did not define it, and the suspicion outlives the question |
| 38 | Is the uncalled component the most detailed description of intended behaviour? | Then it is read as documentation, and every capability it describes but the wired path lacks is a wrong conclusion waiting to be drawn |
| 39 | Is an address or capability the peer *asserted* being treated as one you verified? | An assertion is a usable hint and nothing more; name it a hint in the type, or the first reader treats a claim as a fact |
| 40 | Is the check's passing state also its null state? | Then it cannot detect the null — doing nothing satisfies it, so it reports success across a no-op |
| 41 | Does an error return skip the readback that decides whether the mutation landed? | Propagating the error abandons the only step that can tell "failed" from "succeeded, reply lost" — and every retry then fails correctly, forever |
| 42 | Did you change an artifact after someone verified it, and republish the new value? | A fix applied after verification invalidates the verification, and the more thorough the original check the more expensive the silent invalidation |
| 43 | Is a broken state available right now that you have been unable to test against? | The failing direction of a check is only testable while something is genuinely broken — spend an outage on the measurement, not only the recovery |
| 44 | When renaming an identifier, did you sweep where it is an *authority* reference and not only a *routing* one? | Nothing that resolves routes ever touches a grant list, so it survives every sweep driven by "what breaks if this name is wrong" |
| 45 | If one side of a comparison is missing, does the check say "cannot compare" — or return a verdict? | An absent input rendered as a verdict is wrong in both directions: silently passing, or raising a false alarm that invites undoing correct work |
| 46 | Did anything verify the *destination*, or only the artifact? | Identity checks answer "is this the right file" and say nothing about "does the consumer read from here" — two directories, one filename, and every check passes |
| 47 | When two careful people disagree on a fact, are they describing the same object? | Both measurements can be correct about different things; the ambiguity hides in the definite article, not in the reading |
| 48 | Does the assertion bound a duration, or prove the mechanism by contrast? | A wall-clock bound is a property of the run, not the code — it fails once, under load, when a false alarm is most expensive |
| 49 | Once the live symptom is fixed, is a test the only remaining witness to the defect? | Then that test must be proven red against the pre-fix code, or it is indistinguishable from coverage and quietly retires the finding |
| 50 | Do sibling arms of the same dispatch agree on how they match? | Each arm is defensible read alone and wrong beside its neighbours — a diff shows one arm, so review structurally cannot see it |
| 51 | Did you mutate toward the mistake a future fixer would make, not only toward the original bug? | The plausible wrong fix is the one a green suite blesses |
| 52 | Does one error value carry two meanings — "the remote refused" and "the connection went away"? | The caller retries a call that will refuse identically, and the remote's reason, the only thing saying what to do, is discarded at the conversion |
| 53 | Do two sources describe the same event in incompatible terms? | Something between them is rewriting it — the disagreement is the finding, and a single source alone would read as an ordinary failure |
| 54 | Does an accessor read a field name that is real on a *different* member of the same family? | It returns a plausible absence rather than failing, so it reads as "not set" instead of "looked in the wrong place" |
| 55 | Did the search you are quoting actually finish? | An absence asserted from an unfinished search is a fact about nothing, and it is published with the confidence of a completed one |
| 56 | Is the resource being *consumed*, or *retained*? | A falling number looks identical either way, and every search for a writer is wasted when nothing is writing |
| 57 | Does your existence check discriminate, or does it pass the worst case? | "The path exists" admits a live-but-wrong target; the shape of the path is the test that separates them |
| 58 | Is the state keyed on the thing you are renaming a *cache* or a *fence*? | A lost cache announces itself as slowness; a lost fence reports a clean start, and by the time it is visible the irreversible act has happened |
| 59 | Could your "it survived" check pass by luck? | A count that may legitimately be zero proves nothing; verify an identity only the original could carry |
| 60 | Did you audit the component, or also whoever *lives in* it? | A service and the agent operating it are different subjects; asking an owner "is your state safe" reliably gets the component answer |
| 61 | Does the value you are about to publish depend on a temporary shim? | It will look verified, pass every test, and break when the shim is removed — worse than publishing nothing, which fails loudly on first use |
| 62 | Do both ends spell the same location the same way? | A symlink makes one side report the resolved path and the other the logical one; both are correct, neither can see the other's spelling |
| 63 | Is the survivor a survivor, or does it match everything? | A row with a NULL scope key matches every scope, and reads as evidence of a partial failure when the truth is a scope change |
| 64 | Diagnosing shared infrastructure — did you check the provider's status page first? | It is the cheapest instrument and the one skipped when you already have a hypothesis; one success of your own refutes total unavailability, not an outage |
| 65 | You wrote a caveat — would the conclusion change if the caveated item were deleted? | If not, the caveat was decoration: it discharges the obligation to be rigorous without doing any of the work |
| 66 | Reading a trend — are the points measurements of the same subject? | Real numbers in a real order across different subjects produce a convincing curve that describes nothing |
| 67 | Will anything else change your observable during this operation? | A confound that *decorates* a success is never investigated, because nobody re-derives a number that agrees with them |
| 68 | Comparing two instruments — do they answer the same question? | A pair where one is *defined* to normalise cannot detect whether the other normalised; the difference you see is the definition, not a finding |
| 69 | Can the check read correct while wrong *and* wrong while correct? | Ambiguous in both directions is not weak evidence, it is none — replace it with a behavioural test |
| 71 | When does this check become valid, relative to the decision it gates? | A sound check whose validity window opens after the gate is no check at all |
| 70 | Did the tool answer *your* question, or a well-formed one you did not ask? | "Nothing to review" is a sentence shaped like an answer; a review of an unstaged file is indistinguishable from a clean one |
| 72 | Can this check still *fail*? | A check whose mismatch is expected noise gives no signal in either direction — restoring its ability to fail is what makes it an instrument |
| 73 | Did the instrument touch its target before you read its verdict? | A comment review over an unstaged file, a mutation patch that failed to apply, a query matching nothing on a formatting mismatch — all report well-formed success |
| 74 | Two rules collide — which one protects the validity of what you are about to measure? | That one wins; the other is routed to its source rather than applied late |
| 75 | Your rule requires a value to be *stable* — does it say which value? | Stability and correctness are different properties; whoever applies it fills the gap with the only name in front of them |
| 76 | Writing a rule — which of its terms would a careful reader have to guess? | That term will be filled in wrong, with the nearest available value, by someone following the rule correctly |
| 77 | Who does this fix arm? | Applying a rule to something already carrying a wrong value converts a dormant defect into a live one; the trigger is the remediation |
| 78 | "Fixed" — in the place it was *made* true, or the place it must *be* true? | Merged is not deployed, published is not consumed, written is not read; two parties reading the same correct-but-wrong-scoped source will agree |
| 79 | Handing something over — are you publishing values that *discriminate* it, or values that *describe* it? | A hash describes; an identity discriminates. Publishing the discriminating value is what lets the author find their own error first |
| 80 | Two people report the same metric and disagree — are they measuring the same column? | Two correct measurements of different quantities wearing one name look exactly like a defect |
| 81 | Widening a reply shape — how long until every consumer can parse it? | A service redeploys in minutes and its consumers on restart, so a widened reply is a narrowing at the far end; make it opt-in |
| 82 | How much of each file did your scan actually read? | A scan reporting on a third of a file is not a sweep with a caveat, it is a different question wearing the same name |
| 83 | A tool refuses to open something — is the refusal the obstacle or the finding? | Retrying with a laxer flag converts a correct alarm into a silent wrong answer |
| 84 | Right conclusion — is the mechanism behind it right too? | A wrong mechanism sends the next reader hunting for a state that never existed, even when the advice it produced was correct |
| 85 | A fix landed upstream — which local workarounds did it just make unreachable? | Nothing fails when a workaround is superseded, so it survives as dead code wearing a safety costume |
| 86 | Is this gate expensive enough that someone will route around it? | A gate that gets avoided is replaced by something worse and unmonitored; cost is a security property, not an ergonomic one |
| 87 | Checking a citation in someone else's document — whose ruling is it? | Finding your own words landed there reads as independent corroboration, and the check that was supposed to catch that is what delivers it |

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

### The cheapest way to satisfy an upper bound is to do nothing

ENGRAM fixed a status call that minted six cloud credentials where one would do,
and the natural test asserts the fix: exactly one mint. That assertion also
passes on a status that never reaches the cloud at all -- and a parameter shape
that suppresses cloud construction produces precisely that. The test would then
certify a dead code path as an optimised one.

They wrote both arms: **at least one** mint, proving the call reached the cloud,
and **exactly one**, proving the reads share it. The lower bound is the
load-bearing half, because the failure it catches looks like success.

The general form is worth applying without waiting to be bitten. Whenever a test
asserts an upper bound on work done -- at most one call, no more than N writes,
fewer than K allocations -- the cheapest way to satisfy it is to do none, and
doing none is indistinguishable from the path being broken. Pair every ceiling
with a floor.

This is the same family as a suite that generates zero cases from a file that
moved, and as a progress probe watching the one generation guaranteed to be idle:
**a zero-valued success is indistinguishable from a zero-filled failure.**

### A surface whose failure path emits the same fields as its success path

The same status call answers with `retentionN`, `eligibleCount`,
`pendingR2Deletes` and friends. Its **error branch emits those same field names
with zeros**. So `retentionN: 0` is indistinguishable between *healthy account,
no policy configured* and *the read failed*, and nothing in the values separates
them.

ENGRAM verified which branch produced their numbers before quoting them, using
**key presence rather than values**: `error`/`account` present means the failure
branch, `alert`/`pendingR2Deletes`/`quarantines` present means the success one.
Without that check a reader learns nothing and believes they learned something --
which is worse than an error, because an error gets investigated.

When reading any status surface, establish which branch you are on before
quoting a value from it. And when *writing* one, make the branches
distinguishable by shape: a failure that renders as a well-formed zero is a
failure that will be quoted as a fact.

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

## A fingerprint must be tested against the change it is meant to detect

Offered mtime+size as the cheap way to notice a config file changing between
reading it and acting on it. Before adopting it I tested it against the specific
edit the operation makes -- a module-id rename -- on a copy:

  before: mtime 1785101683  size 3301
  after:  mtime 1785101683  size 3301   IDENTICAL
  sha256: 3bf069e2a666aef1 -> b33ffebc33497405   CHANGED

TWO INDEPENDENT REASONS IT MISSES, both properties of this exact change. A rename
is a SUBSTITUTION, so size is preserved when the replacement is the same length.
And mtime granularity is ONE SECOND, so an edit landing in the same second as the
read is invisible -- not exotic when the edit is scripted.

So the conventional fingerprint reports "nothing changed" across a change that
definitely occurred, in the one situation the procedure exists for. mtime+size is
blind to substitutions and to same-second writes -- which is to say, blind to
renames and scripted edits, between them most of what an operator does to a config
during a migration.

RULE: a fingerprint is not adopted because it is conventional. RUN IT AGAINST THE
REAL CHANGE FIRST and confirm it moves. Same shape as the vacuous truncation
fixture -- the check was fine, the input could not exercise it, and only trying the
real case showed the difference.

WHY THIS ONE IS WORSE THAN A WRONG ANSWER: it is a wrong answer that AGREES WITH
THE EXPECTED ONE. On every ordinary run nothing has changed, the detector says
nothing changed, and confidence is confirmed. It diverges only on the day it
matters. A detector whose failure mode is indistinguishable from its success mode
under normal conditions cannot be validated by using it.

AND CONFIRM THE CHANGE ACTUALLY OCCURRED BEFORE CALLING IT A DETECTOR FAILURE.
"mtime and size identical" is also exactly what a NO-OP WRITE looks like, so
without watching the hash move there is a plausible benign explanation available,
and the finding gets filed as "nothing happened". Every negative result about an
instrument needs positive evidence that there was something to detect.

A NAME IS A CLAIM ABOUT CONTENT, NOT EVIDENCE OF IT. The same question underlies
both halves -- is this file the one I think it is, and are these the bytes I read?
Files whose names encode an identifier invite treating the name as authoritative;
it is metadata a writer chose, and nothing revalidates it.

## A failed `cd` runs the rest of your commands somewhere else

Building a deliberately-conflicting branch to prove a merge gate could fail, I
wrote `git worktree add "$T/w" && cd "$T/w" && git checkout -b probe && <sabotage>
&& git commit`. The worktree add failed. `cd` failed. AND EVERY SUBSEQUENT COMMAND
RAN IN THE MAIN REPOSITORY -- so the probe branch was created there, the sabotage
edit was applied to a live file, and it was committed, all while the transcript
showed a plausible result.

I then read the run as "the branch was never created" and tried again, which is
how I learned it HAD been: the second attempt failed with "a branch named
'conflict-probe' already exists". Recovery needed removing the throwaway worktree
first, because it held the master checkout and blocked returning to it.

Three separate rules, each of which alone would have prevented it:
· `set -e` DOES NOT PROTECT A `&&` CHAIN THE WAY PEOPLE EXPECT, and a chain that
  begins with a directory change is a chain whose every later step is conditional
  on that change. VERIFY THE DIRECTORY EXISTS AND ABORT if not -- do not rely on
  `cd` failing loudly enough to stop what follows.
· DESTRUCTIVE PROBES BELONG IN A WORKTREE WHOSE CREATION YOU CONFIRMED, never in a
  chain where the fallback location is the real tree. The blast radius of a
  mistake should not be "the repository I am working in".
· WHEN A COMMAND REPORTS A STATE THAT CONTRADICTS YOUR MODEL, ASK THE AUTHORITY
  RATHER THAN RETRYING. Two retries produced two more confusing errors; one `git
  branch --list` plus `git worktree list` showed the actual state immediately --
  the main worktree sitting on a sabotage commit.

Nothing was pushed and no work was lost, which is luck rather than design: the
edit happened to be to a file I could restore from master.

SWEEPING THE COMMITTED SCRIPTS AFTERWARDS FOUND NOTHING, and the reason is worth
recording because it explains where this class actually lives. Every directory
change in the checked-in tooling is either inside `$( ... )` or `( ... )` -- a
SUBSHELL, whose failure cannot affect the parent's working directory -- or a bare
`cd` with an explicit `|| exit`. Confirmed empirically rather than assumed: a
subshell whose `cd` fails produces empty output and leaves the caller's directory
untouched.

SO THE EXPOSURE IS IN AD-HOC COMMANDS, NOT IN COMMITTED CODE. Scripts get written
deliberately, reviewed once, and reused; ad-hoc chains are composed under time
pressure, run once, and never read again -- which is precisely when a fallback
location of "wherever I happen to be" is most dangerous. The habit that
generalises: WRITE THROWAWAY CHAINS THE WAY YOU WOULD WRITE A COMMITTED SCRIPT,
subshell-scoped, because the throwaway one has no reviewer.

## A procedural mitigation inherits the defect of the tool it replaces

I rejected a CLI preview because it would have located a config file itself --
two rules selecting one subject, the CLI's guess against the daemon's actual path.
Then I proposed doing the preview BY HAND instead, whose first step is locating
the same file myself. IDENTICAL DEFECT, one of the two rules now a human, and
worse for it: a tool's guess is inspectable in source, mine was in my head.

A MANUAL PROCEDURE IS NOT A NEUTRAL FALLBACK. It carries every assumption the
tool would have carried, minus the review. "I will be careful" restates the
problem: a prediction read from the wrong file agrees with itself perfectly.

WHAT ACTUALLY CLOSED IT was deriving both halves from the same authority. The
running set comes from the daemon over the connection the operation will use; the
config path comes from the DAEMON PROCESS'S OWN ENVIRONMENT rather than from my
shell's -- launchd for the pid, then the process environment, then the documented
resolution order. Proven from the executing process instead of guessed from a
similar one. It matched the guess, which is exactly why the guess had been
harmless and why the agreement had never been established.

ORDERING RULE from the same exchange: when a preview and a way to VERIFY the
preview are both on the ledger, the verifier is worth more. A preview alone is
trusted; a published input makes it checkable.

## Prefer evidence that shares a lifetime with its subject

A prune predicate needed to tell a deliberate deletion from a snapshot of a
worktree being torn down. The first design looked the branch up in a task ledger
-- and the ledger's rows expire, so the OLDEST branches, the ones most worth
pruning, had outlived their evidence. The predicate was strongest where it was
least needed.

The replacement reads only the two commits: are the deleted paths still present on
the base branch? If they are, nothing was removed from the project and the commit
records an absence that existed only inside a dying worktree. Same question,
answered from the artifact rather than from a side table.

THE PROPERTY IS NOT "NO EXPIRY", AND SAYING SO WOULD BE THE OVERSTATEMENT THAT
GETS LEANED ON LATER. The check still depends on the branch commit being
reachable, so it survives a branch outliving its ledger row and does NOT survive
the ref being deleted. What it buys is that THE EVIDENCE AND THE SUBJECT NOW SHARE
A LIFETIME -- the evidence cannot expire while the thing being judged still exists.
That is strictly better than a side table and materially different from immortal.

Two consequences that follow only once it is stated that way: adjudicate BEFORE
deleting a ref, never after; and RECORD THE VERDICT DURABLY, because it outlives
the ability to re-derive it.

GENERAL FORM: when a predicate needs supporting evidence, ask whether that
evidence can vanish INDEPENDENTLY of its subject. If it can, the predicate has a
blind spot that grows with age, and it will be blindest on the oldest cases --
which are usually the ones anyone bothers to run it on.

AND THE ORDERING RULE IS WORTH STATING WHILE IT IS STILL OBVIOUS: in any cleanup,
ADJUDICATE BEFORE DELETING, never after. It reads as too obvious to write down,
which is exactly why cleanup scripts get it backwards -- THE DELETION IS THE
SATISFYING STEP AND THE CLASSIFICATION IS THE CHORE, so the order that feels
productive is the order that destroys the evidence. Write the coupling down while
nothing automates the deletion yet; afterwards it is a bug report rather than a
constraint.

## Is this commit comment-only? A line-based check cannot answer it

Asked on every deploy, to decide whether a pending commit needs shipping. Ran the
obvious version -- strip comment lines and blanks, hash the rest, compare parent
against commit -- and it reported REAL CODE CHANGE on two commits that were purely
comment edits.

THE TELL WAS THAT THE TWO HASHES SWAPPED. Commit A moved x -> y and commit B moved
y -> x, net zero across the pair. Behaviour does not usually return exactly to a
previous state through two unrelated commits; REFORMATTING does, because the
formatter re-wraps an expression when a comment above it changes its length, and
then re-wraps it back. A line-oriented normalisation sees re-wrapped lines as
different content -- it is comparing LAYOUT, not code.

Whitespace-blind normalisation (strip comments, then delete ALL whitespace) gave
the correct answer: identical either side, and identical across the pair. The
control -- a commit that genuinely changed behaviour -- still differed under the
same normalisation, which is what proves the comparison did not simply stop being
able to see anything.

WHY THE FAILURE DIRECTION MATTERS: it over-reports. A comment-only commit read as
a code change costs a deploy nobody needed, which is the safe side. But the same
mechanism under-reports whenever a formatter's re-wrap happens to CANCEL a real
change in the hash, so the honest statement is that a line-based check measures
layout and any agreement with behaviour is incidental.

GENERAL FORM: WHEN NORMALISING BEFORE A COMPARISON, ASK WHAT THE NORMALISATION
STILL LETS THROUGH. Stripping comments removes one confound and leaves another
standing, and the residual one is invisible precisely because the check now looks
principled.

## A verification step invalidated by the procedure it verifies

Our deploy ritual signs the binary AT THE DESTINATION, because an unsigned copy is
killed by the kernel on first exec. Our deploy CHECK compared the staged artifact's
sha256 against the deployed file's. Signing rewrites bytes inside the binary, so
THOSE TWO CAN NEVER MATCH -- the check reports a false mismatch on every correct
deploy, and its natural remedy is redeploying a binary that was already right.

The check and the ritual were quietly incompatible. Nothing surfaces that until the
day it fires, on a real deploy, under time pressure, telling an operator that
correct bytes are wrong.

AUDIT THE CLASS, NOT THE INSTANCE: a verification step whose validity depends on a
step the procedure itself performs. Pointing that at the rest of the same ritual
found a second immediately -- BYTE SIZE -- and the second one is worse than it first
appeared, for a reason that only came out on a retest.

The first reading was "size changes across signing, 9,409,008 to 9,372,416". WRONG,
and verified wrong independently: re-signing an already-signed binary leaves the
size UNCHANGED. The observed change was cargo's output to the first re-sign, and the
cause is that THE LINKER ALREADY SIGNS ITS OUTPUT (`flags=0x20002(adhoc,
linker-signed)`). So the first codesign REPLACES a linker signature and resizes;
every signing after that does not.

WHICH MAKES SIZE MORE DANGEROUS THAN AN ALWAYS-WRONG CHECK, NOT LESS. It does not
always disagree -- it disagrees ON THE FIRST SIGNING ONLY. A CHECK THAT USUALLY
PASSES IS TRUSTED FAR MORE THAN ONE THAT NEVER DOES, and the single case where size
legitimately disagrees is exactly the case an operator learns to wave through as
"that's just the signing". Since a truncated copy also reads smaller, the fault size
exists to catch arrives wearing the one excuse that has been trained into the reader.

A marker-string check SURVIVES, because signing rewrites the signature blob rather
than the string table. That was measured rather than assumed, and then tested AS A
PREDICTION on a case not yet examined -- 22,164 strings before and after, identical
-- which is what separates a mechanism from a story fitted to what you already saw.

WHAT TO USE INSTEAD, ranked by the question each answers:
· INODE -- "is this pid executing this file". Unaffected by signing. Rung 1.
· LC_UUID (`otool -l`, one line) or the __TEXT segment bytes -- "is this file the
  build I meant". Both survive re-signing; verified agreeing in both directions,
  identical across a re-sign and differing across builds.
· WHOLE-FILE SHA AND FILE SIZE -- valid only when nothing re-signs after staging,
  which our ritual violates by design.

AND BOTH REPLACEMENTS CARRY THE EMPTY-RESULT TRAP. `otool` on a non-Mach-O yields
nothing, so TWO UNREADABLE PATHS COMPARE EQUAL and report SAME BUILD -- the
strongest possible agreement, from two measurements that did not happen. Same shape
as an lsof invoked with an empty pid.

PUT THE GUARD IN THE EXTRACTION, NOT THE COMPARISON. This shape appeared THREE TIMES
IN ONE WEEKEND INSIDE FIXES FOR ITSELF, and the reason is structural: THE FIX IS
ALWAYS A NEW COMPARISON, AND EVERY NEW COMPARISON IS A FRESH CHANCE FOR BOTH SIDES
TO BE EMPTY AND AGREE. The class reproduces through its own remedy. A guarded
comparison protects only callers who remember; AN EXTRACTOR THAT RAISES RATHER THAN
RETURNING A SENTINEL CANNOT FEED A FALSE MATCH DOWNSTREAM AT ALL.

The test that shows the difference is the CARELESS CALLER: write the naive
`identity(a) == identity(b)` over two unreadable paths. With a raising extractor it
cannot run. With a sentinel it returns True. Mutation-prove it by reverting the
extractor to a sentinel and watching the false match come back -- a guard verified
only through its careful caller has not been tested where it matters.

## Reach for the authoritative check before the clever one

After running about ten mutations in one session, I swept for sabotage left behind.
Built a positional detector -- a `return false;` immediately after a function
signature -- reasoning that the count alone cannot separate a mutant from ordinary
Rust. Proved it could fire by planting one in a throwaway tree. Clean.

Then ran `git diff origin/master` and it was ZERO. A tree with no diff against the
remote cannot contain a mutation, so the positional check was redundant before I
wrote it. The cheap authoritative check SUBSUMES the clever one entirely.

WHY THE CLEVER ONE CAME FIRST, since the ordering is the finding rather than the
waste: I was thinking about the PROPERTY (what does a leftover mutation look like)
rather than the QUESTION (is anything left). The property invites a detector; the
question has a one-command answer. The same inversion produced a false report about
another repo's CI earlier the same night -- I reasoned from a mechanism instead of
asking the authoritative query, and published the inference over a measurement I
had already made.

BEFORE BUILDING A DETECTOR, ASK WHETHER SOMETHING ALREADY KNOWS THE ANSWER. Version
control, the process supervisor, the package manager, the API -- these hold
authoritative state and answer in one command. A detector reconstructs that state
from evidence, and every reconstruction is a chance to be wrong in a way that reads
as a result.

THE CLEVER CHECK IS NOT WASTED WHEN THE AUTHORITY IS UNAVAILABLE -- a dirty tree,
an unpushed branch, a machine with no remote. Keep it for that case, and reach for
it second.

## Fenced, unfenced, and fenced-by-accident

Running the constant-function mutation across a module's guards produces three
outcomes, not two, and the third needs a different response from either.

FENCED. Breaking the guard reddens a test NAMED FOR IT. Nothing to do.

UNFENCED. Breaking it changes nothing, or -- worse -- produces a HANG. Write the
missing test.

FENCED BY ACCIDENT. Breaking it reddens several tests and EVERY ONE IS NAMED FOR
SOMETHING ELSE. Measured instance: a health-advertisement check whose accept
direction is held up by five tests about capability relay, probe demultiplexing and
supervision-only probing. Each exercises a successful advertisement check on the
way to its own subject.

THE THIRD IS REAL PROTECTION WITH A SPECIFIC FRAGILITY: narrowing any of those
tests to focus on its stated subject silently removes coverage nobody knows they
are carrying. It is also INVISIBLE TO AN AUDIT BY NAME -- reading the test list
suggests the guard is uncovered, and reading the mutation result suggests it is
fine. Only the two together locate it.

THE RESPONSE IS A COMMENT AT THE GUARD NAMING WHICH TESTS HOLD IT UP, not a new
test. A new test adds coverage while leaving the next person just as unaware of
what the existing ones quietly do -- and they are the ones at risk of being
refactored.

A CHEAP PREDICTOR, since the full sweep is not free. Test names rank where to spend
the mutation, in three shapes of increasing danger:

RULE-NAMED ("requires a matching nonce") tends to cover the whole truth table.
Measured: covered both directions, accept case asserting a real effect.

REFUSAL-NAMED ("refuses an unattested consumer") tends to cover refusals only.
Measured: the guard admitted nobody with the suite green.

ABSENCE-NAMED ("never contains the input", "does not leak", "is not reachable") is
the worst, and it inverts the usual intuition: THE DEGENERATE IMPLEMENTATION IS
MORE ABSENT THAN THE CORRECT ONE. A second seat found a fingerprint function whose
three assertions all checked output SHAPE -- prefix, length, and does-not-contain
the input -- and A HARDCODED CONSTANT SATISFIED ALL THREE PERFECTLY. The
strictest-looking assertion in the file is the one a constant passes most easily,
because a constant contains the input LESS than a real digest does.

The consequence there was not cosmetic: the fingerprint was used as IDENTITY, so a
constant collapses every distinct handle onto one -- one credential's response
served for another's lookup, one audit trail for all of them -- while reading as
correct in every log line. Same total collapse of a primitive as an all-green trust
guard, reached through a different door.

SO GREP TEST NAMES FOR refuses/rejects/denies AND FOR never/not/without, and for
each ask what the corresponding PRESENCE assertion would be. For an absence-named
test the missing counterpart is usually "distinct inputs produce distinct outputs",
and it needs proving against TWO mutants: a constant, and a function that reads
only part of its input -- the second produces different outputs for different
inputs, so a naive assertion over two very different values passes it.

## An optional field is ignored by anything that predates it

Added a `preview` flag to a control operation that retires processes, so an
operator could see the decision before it executed. Ran the new client against the
RUNNING daemon as a live check. It performed a REAL reconciliation: the daemon
predates the field, unknown fields are dropped during deserialisation, and nothing
anywhere errored. The fleet survived because the config happened to match the
running set.

THE FAILURE DIRECTION IS THE WHOLE POINT. For most added fields, being ignored
degrades a feature. For a field whose meaning is DO NOT DO THE THING, being
ignored means DOING THE THING -- while the caller is told, by a normal-looking
success, that nothing happened. A safety flag that can be silently dropped is not
a weaker safety flag; it is an unsafety flag with a reassuring name.

THE FIX IS AN ECHO PLUS A REFUSAL. The response carries the flag back, produced
only by the path that honours it, so an older peer CANNOT fabricate it. The client
then refuses -- loudly, nonzero, naming what may already have happened -- when the
echo is absent, rather than printing the result it was given. Silence on the echo
is the one case where reporting success is a lie.

GENERAL RULE: WHEN ADDING AN OPTIONAL FIELD, ASK WHAT ITS ABSENCE MEANS TO A PEER
THAT NEVER LEARNED IT. If absence is benign, defaulting is fine. If absence
inverts the meaning -- suppression flags, dry-run flags, confirmation tokens,
idempotency keys -- the receiver must PROVE it understood, and the sender must
treat missing proof as failure. This is the deploy-order rule (a narrowing shipped
ahead of its producer silently drops what it narrows) arriving through the field
rather than through the schema.

AND IT WAS ONLY FOUND BY RUNNING IT AGAINST A LIVE OLD PEER. Every test passed:
the tests build both halves from the same source tree, so version skew is exactly
the condition a same-repo suite structurally cannot construct.

THREE STRENGTHS, AND THE STRONGEST IS NOT THE ONE I SHIPPED. A second seat swept
their own wire against this class and found their mutating plane STRUCTURALLY
IMMUNE -- every field rides inside a body whose digest is bound into the request
signature, recomputed by the receiver and compared. Dropping a field CHANGES THE
DIGEST, so an older receiver does not ignore it, it FAILS VERIFICATION. There is
no ignored state available to the field at all.

So the ladder is, strongest first:

NEGOTIATE. The sender emits the field ONLY to a peer that proved it understands,
during capability negotiation, and REFUSES the operation outright when the peer did
not -- rather than sending the degraded form. The receiver then REQUIRES the field
on any negotiated session and treats its absence as a protocol violation. Absence
never occurs rather than being detected, and the proof of understanding is the
negotiation, which an old peer cannot fabricate. A third seat found their one
inverting field closed exactly this way: a `mutating` flag whose absence would have
routed a call around the exactly-once ledger entirely, emitted only to peers that
negotiated the capability.

THE LOAD-BEARING HALF IS THE REFUSAL, and it is the half most implementations get
wrong: the tempting behaviour when a peer lacks the capability is to send the
request in its degraded form and hope. That reintroduces the exact inversion the
negotiation exists to prevent, with a green handshake in front of it.

BIND. Put the field inside something the receiver must verify -- a digest folded
into the signature -- so dropping it changes the digest and the request FAILS
VERIFICATION rather than degrading. Absence is unrepresentable.

ECHO PLUS REFUSAL. The receiver proves it understood in the response, and the
caller treats missing proof as failure. Detection after the fact, and it depends on
the caller actually checking. This is the retrofit for a plane that can neither
negotiate nor bind -- a plain JSON control plane, a query string, anything where
the receiver cannot tell the field was ever there.

NOTHING, and the inversion is silent.

A RELATED STRUCTURAL FIX WORTH PREFERRING TO ALL THREE: make the field REQUIRED
rather than optional. The same seat's idempotency key rides a required struct, so a
peer that drops it produces a schema violation rather than a duplicate execution.
Where the dedup identity is inseparable from the request, the hazard cannot be
constructed.

THEIR SENTENCE IS THE ONE TO KEEP: the signed plane cannot have the defect and the
unsigned plane cannot avoid it by care. The difference is not diligence. It is
whether the receiver can tell the field was ever there. Note also WHY their plane
has the property -- it was designed against a hostile server, not for versioning,
and version-skew immunity fell out of it. Integrity mechanisms buy skew safety for
free; versioning discipline does not buy integrity.

SWEEP RESULT ON THE SAME WIRE, recorded because a null is worth as much as a hit
and is the only thing that sizes the class. Five optional fields on the control
plane, each read for what its absence means to a peer that never learned it.
CONSUMER IDENTITY: dropped, the caller is attested as the WEAKER principal, so a
provider gating on the stronger one REFUSES -- fail-closed. CONSUMER CAPABILITIES:
dropped, the provider concludes the client cannot accept reverse requests and does
not send any -- a lost feature, not an unsafe act. ADMISSION FACTS: dropped, a
gate expecting them denies. Only PREVIEW inverted, and the reason is visible in
the list: THE OTHER FOUR DESCRIBE WHAT THE CALLER IS OR CAN DO, WHILE PREVIEW
DESCRIBES WHAT THE RECEIVER MUST NOT DO. Capability-shaped fields degrade toward
refusal when lost; instruction-shaped fields degrade toward action. That is the
cheap discriminator to apply before tracing anything.

## A destructive operation whose premise is unauditable before it fires

A sweep deletes complete, signed snapshots when the chain head they planned
against no longer matches the current one. Correct behaviour. But the planned head
lives only inside a sealed blob, so THE INPUT TO THE DESTRUCTIVE DECISION CANNOT
BE INSPECTED FROM OUTSIDE -- and if the implementation's read of it were wrong,
the evidence would be gone by the time anyone looked.

That is a distinct hazard from an ordinary unverifiable claim. Most such claims
leave their subject in place to be checked later; this one consumes it. The
asymmetry is what makes it worth naming: a WRONG PRESERVE is recoverable, a WRONG
DELETE is not, so the two directions do not deserve the same standard of evidence.

THE CHEAP FIX IS ALWAYS THE SAME SHAPE: expose the predicate's inputs next to its
current comparison value, read-only, so the decision is checkable BEFORE it
executes rather than reconstructible afterwards. No behaviour change, no new
failure mode.

AND THE DECISION NOT TO FIX IT NOW IS LEGITIMATE, provided it is recorded as a
decision. Here: the sweep has run in production for months, so the gap predates
the incident and is not urgent; a fix would only pay off if deployed ahead of the
very recovery it would audit; and adding a commit to a payload someone must verify
under time pressure has its own cost. AN UNRECORDED DEFERRAL DECAYS INTO AN
APPARENT OVERSIGHT -- the same reason a deliberate refusal in code needs its
justification written at the refusal.

## Two lines naming the same subject must select it by the same rule

A report section printed a header naming the generation being uploaded, then
sampled a progress file to say whether it was moving. The header selected by
sequence number; the probe selected by "most recently modified file anywhere in
the staging area". Both rules are reasonable and they agree almost always.

They diverged the moment a FINISHED generation's residue outlived an UNSTARTED
one: the header said generation 99 while the probe watched generation 96's file
and reported NOT MOVING. True of that file, silent about the subject. And the
failure lands exactly on the distinction the section exists to make -- A FINISHED
UPLOAD IS INDISTINGUISHABLE FROM A STALLED ONE ONCE YOU ARE WATCHING THE WRONG
FILE.

What makes this worse than an ordinary bug is that the output stayed fluent. Two
adjacent lines, consistent grammar, no error anywhere -- the reader has nothing to
notice.

RULES: resolve the subject ONCE and pass its identity down, rather than letting
each line re-derive it. Where that is impractical, MAKE EACH LINE NAME THE SUBJECT
IT MEASURED, so a divergence appears in the output instead of only in the code.
And distinguish NOT STARTED from STARTED-THEN-STOPPED: the absence of a progress
artifact is a different state from a stalled one, with a different remedy.

## A single sample cannot distinguish a state from a trajectory

Three times in one night, on three unrelated resources, the same shape: a level
read as a trend, with the alarming interpretation available for free.

· A debug capture directory: newest file written minutes ago, so the directory
  was "growing". It was a ring at equilibrium. NEWEST TELLS YOU THE WRITER IS
  LIVE; ONLY OLDEST TELLS YOU WHETHER IT IS BOUNDED.
· A population of empty files: zero created in the last 24 hours, so creation had
  "stopped". Grouped by absolute date it was ongoing and bursty.
· Free RAM at 0.5 GB: the box was "degrading". It was loaded and stable.

THE DISCRIMINATOR IS NEVER THE LEVEL. For memory specifically it is whether the
kernel is EXTENDING THE SWAP FILE: a degrading box grows the file and free RAM
oscillates by a gigabyte on a 30-second timescale; a merely loaded one shows a
fixed file with steady occupancy. Same "0.5 GB free" headline, opposite
conditions, and the remedy differs completely -- one needs processes to exit, the
other needs nothing.

GENERAL FORM: before reporting a resource as a problem, TAKE A SECOND SAMPLE AND
NAME WHICH QUANTITY YOU EXPECT TO HAVE MOVED. If you cannot name one, you are
reporting a level and calling it a trend.

AND NAME THE RESOURCE. "0.1 GB free" with no unit of what reads as whichever
resource the reader has most recently been worrying about -- disk pressure wants
deletion, memory pressure wants processes to exit, and the two share no remedy.

A PERCENTAGE WITHOUT ITS DENOMINATOR IS THE SAME DEFECT WEARING A NUMBER. I put
"the disk is 95% full" into two decisions asking a busy person to act. True, and
it overstated the case badly: the disk is a 4 TB container with 215 GB free. 95%
of 4 TB and 95% of 256 GB are not the same situation, and the percentage is the
form that travels. Give both, or give the absolute -- a ratio is a claim about
proportion that readers convert into a claim about urgency.

SAME UNIT TRAP ONE STEP OVER: the daemon log read "1.34 GB" once and "1.25 GB"
later, which for an append-only file should be impossible and briefly looked like
something had rotated it. Same file, same bytes, GB against GiB. WHEN A QUANTITY
MOVES IN A DIRECTION ITS MECHANISM FORBIDS, SUSPECT THE UNITS BEFORE THE
MECHANISM.

## A relative-time window is anchored on the moment you run it

Asked whether a population of files was still growing, I ran a "created in the
last 24 hours" filter at 22:12, got zero, and reported that creation had stopped.
The filter was correct and its window was not what I read it to be: a rolling 24
hours anchored on the current clock reaches back to 22:12 YESTERDAY, so it
excluded almost all of the previous calendar day -- which was the busiest recent
day. Grouped by absolute date, the population was ongoing and bursty.

THE COMPOUNDING IS THE PART THAT MATTERS. I combined two true measurements --
zero in the last day, most older than a week -- into a STORY about a population
that accumulated under some earlier condition and then stopped, and sent the story
rather than the numbers. A false conclusion drawn from correct measurements is
harder to catch than a wrong number, because nothing in it looks wrong.

RULE: when the question is "is this still happening", GROUP BY ABSOLUTE DATE
rather than filtering by relative age. A distribution cannot hide a trough; a
threshold cannot show one. And the same command means something different at
09:00 than at 22:00, which makes a relative window unreproducible by anyone
reading your report later.

THE DISCRIMINATOR, because both shapes look like time arithmetic: a DIFFERENCE
BETWEEN TWO ABSOLUTE TIMESTAMPS is stable -- a commit date against a file mtime
gives the same answer whenever you run it, so gating on it is sound. A FILTER
RELATIVE TO NOW is not, because the window moves under you. Sweeping this repo's
fleet tooling found only the first form (deploy gap = head commit time minus
binary mtime; module age reported by the module itself), so the sweep came back
clean -- but the two are one keystroke apart and read identically in review.

## Every broken instrument was caught by a contradiction, never by inspection

Six instrument defects in one day, across two seats: a pipeline reporting `tail`'s
exit status instead of the command's; a parser reporting a whole tree torn; an
`lsof` on an empty pid listing every process on the machine; a callgraph control
drawn from build artifacts; a truncation fixture built on a zero-byte file; a
search anchored so it could never match. Different populations, one shape: THE
INSTRUMENT ANSWERED A QUESTION ADJACENT TO THE ONE ASKED, plausibly enough to act
on. Four of the six failed in the direction "the gate does not fire".

NOT ONE WAS FOUND BY READING THE COMMAND. Every one surfaced because a result
disagreed with something already known -- a count that could not be zero, a
process that was plainly running, a file that plainly had frames in it.

That is an argument about METHOD, not diligence. Reviewing instruments more
carefully does not work, because a broken instrument reads as correct; what works
is KEEPING A KNOWN QUANTITY IN HAND TO CHECK THE ANSWER AGAINST. When there is no
such quantity, manufacture one before running the real query: a positive control
scoped to the same population, a planted item you know the answer for, a tampered
input that must be rejected.

A baseline is this discipline made durable. The migration gate above is not a
special-purpose tool -- it is the same instrument-check habit, written down so it
survives the moment when attention is thinnest.

## A control drawn from the same population as the query proves nothing

Checking whether a Swift method had callers, I ran a text search, got 21 hits
against a callgraph reporting zero, and ran a positive control -- a method I knew
was called -- which returned 23. Control passed, so I believed the 21.

Both numbers were build artifacts. The package's `.build` directory carries index
databases and serialised modules that mention every symbol in the package, so
BOTH the query and its control were dominated by the same non-source population.
The control could not have failed: it was measuring the same thing the query was
measuring wrongly. Restricted to tracked files, the real answer was ONE caller,
and the callgraph's zero was also wrong.

This is the independence rule landing on a control I built myself minutes
earlier, which is what makes it worth writing down. The check for it is
mechanical: NAME THE POPULATION THE CONTROL IS DRAWN FROM AND ASK WHETHER IT IS
THE SAME ONE THE QUERY SEARCHES. If it is, the control tests reachability of a
set, not correctness of a filter.

PRACTICAL FORM FOR REPOSITORY SEARCHES: scope to tracked files (`git ls-files`)
rather than walking the working tree. Generated output, build caches, and vendored
copies all answer searches and all look like source. The same discipline that
keeps stale audit evidence out of a repo applies to the search itself.

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

The rule is not about comments. It has three instances, ordered by FEEDBACK-LOOP
LENGTH, which is what decides how much damage each does:

- A TEST asserting half a tradeoff fails visibly the moment someone touches the
  other half. Self-correcting, fastest loop.
- A COMMENT split across two files misleads one reader at a time, and only the
  reader who happens to arrive.
- A METRIC showing one side of a coupled pair is read CONTINUOUSLY, by people
  making decisions, with nothing indicating the other half exists. It does not
  fail to warn -- IT ACTIVELY RECOMMENDS optimising the visible half, and keeps
  recommending it. Worked case: a backup metric reported bytes and not round
  trips, so every reading argued for smaller chunks while the real cost was
  per-object.

The dashboard was not wrong about bytes. IT WAS WRONG ABOUT WHAT QUESTION IT
APPEARED TO ANSWER -- which makes it the same defect as an unscoped severity
label, arriving through the widest possible door: AN ARTIFACT THAT IS LOCALLY
ACCURATE AND SILENTLY ANSWERS A BROADER QUESTION THAN THE ONE IT MEASURED. The
reader cannot detect it from the artifact alone, because nothing in a correct
bytes-number hints that round trips exist.

AND THE SAME STRUCTURE HOLDS ONE LEVEL UP: THE FINDINGS BUY THE RIGHT TO HAVE THE
ARGUMENT. When a night's method corrections outlast every defect it found, the
tempting conclusion is that method is what matters, therefore hold method
discussions. That is the rule degrading into its own failure mode. Every
correction worth keeping was forced by a concrete case neither party could
hand-wave past -- a stack-local mutex that looked like a hazard, flags in a crate
the binary does not link, a panel reporting bytes without round trips. Strip the
cases and you get agreeable generalities, several of them wrong, with nothing
present to catch which.

THE SWEEP BUYS THE RIGHT TO MAKE THE CLAIM, AND THE CLAIM IS WHAT SURVIVES. You
cannot write "these guards cannot poison, and here is why" until you have read
all of them -- so the reading is not overhead on the way to a finding, it IS the
purchase. That inverts the intuition that a clean sweep is wasted effort.

Which changes when to run one: NOT "I SUSPECT A DEFECT HERE" BUT "I WANT TO BE
ABLE TO STATE THIS INVARIANT AT THE SITES THAT DEPEND ON IT". Those pick
different sweeps, and only the second pays regardless of outcome. It also
predicts its own scope -- the invariant you want to state names the population you
must read, where "find the bug" gives no stopping rule at all. And it is why the
first framing produces speculative hardening: A SWEEP CHARTERED TO FIND SOMETHING
MUST PRODUCE SOMETHING.

MEASURE AN AUDIT IN CONSTRAINTS LANDED AT SITES, NOT FINDINGS REPORTED. A report
is a claim about a moment; a comment at the site is a constraint that travels
with the code. The census, the flags and the negative result are all invalidated
by the next edit to those functions, and none of them are visible to the person
making that edit. In the sweep above, three comments stating why particular
guards cannot poison were the entire durable yield -- they make a future fallible
addition legible AS a change to an invariant, to someone who never read the
audit. Eleven flags yielded nothing.

A TOOL THAT INHERITS AN IDENTITY IT DID NOT EARN WORKS UNTIL THE MOMENT IT
MATTERS. A monitoring probe run from a shell spawned by a supervised process
inherits that process's identity environment, and a client library that
auto-attaches those variables will assert the identity on every call. It is
validated: the daemon checks the spawn nonce it minted, so the probe fails the
instant that process restarts and the inherited value goes stale.

The timing is the whole defect. IT WORKS RIGHT UP UNTIL A RESTART, which is
precisely when monitoring is worth having -- so the probe is reliable in every
condition except the one it exists for. Strip inherited credentials in any tool
that has no business claiming them.

REPRODUCING IT NEEDED THE FAILURE CONSTRUCTED, NOT RE-RUN. Reverting the fix and
re-running passed, because each new shell inherits a FRESH nonce from the
respawned process -- the failure window had already closed. A transient condition
cannot be reproduced by repeating the action that hit it; it has to be rebuilt
deliberately (here, by supplying a deliberately stale value). A negative control
that passes because the world moved on is not evidence the fix was unnecessary.

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

## Any intermediary between a check and its report can substitute its own verdict

The pipeline case below is one instance of a more general shape, and naming only
the instance leaves the others invisible.

THE WORKFLOW-RUNNER INSTANCE: GitHub skips later steps once one fails. THALAMUS's
test steps carried `if: !cancelled()` to defeat that; THEIR LINT STEP DID NOT. So
a formatting slip -- the cheapest and most easily tripped check in the file --
cancelled the lints AND both test suites, and the job reported one red mark a
reader cannot distinguish from "style problem, code fine". A run in their own
history proves it had already fired: format success, clippy failure, then
`skipped / skipped`.

I HAD THE IDENTICAL GAP and found it only because they described theirs. My test
steps were guarded and my two clippy steps were not, so a stray blank line would
have cancelled every lint and test answer beneath it.

THE GUARD IS ONLY MEANINGFUL ON A CHECK THAT FOLLOWS ANOTHER CHECK. Leave setup
steps unguarded: if dependency install fails, the check after it is IMPOSSIBLE
rather than skipped-by-policy, and blanket-applying the guard erases that
distinction. Enumerate every step and classify it as setup or check rather than
applying the guard by pattern.

THE TWO DIRECTIONS OF THE SAME DEFECT, worth seeing together: swallowing a status
with `|| true` turns a real failure into a pass; an unguarded step lets a real
failure SUPPRESS FOUR OTHER ANSWERS. Both are the check no longer being the thing
that reports.

A GREEN RUN CANNOT DEMONSTRATE THIS GUARD, which is worth knowing before you go
looking for reassurance in one. The guard only does anything when an earlier step
FAILS, so every passing run exercises the same path as before the fix and proves
nothing about it. Two checks are available without spending a deliberately-red
run: confirm the guarded steps EXECUTED (not merely that the job was green), and
parse the workflow to confirm the guard sits on exactly the check steps and not
on the setup steps. Neither is the real proof; both are honest about being
weaker, which the green run is not.

AND WATCH WHICH COMMIT THE GREEN BELONGS TO. My workflow edit's own run was
CANCELLED by the concurrency group, so the first completed run carrying it was
the NEXT commit -- a docs-only change whose green would ordinarily say nothing.
A run list showing passes is a claim about the tip, never about the commit that
changed the logic.

AND AN INSTRUMENT WHOSE COMPLAINTS YOU HAVE LEARNED TO DISMISS IS
INDISTINGUISHABLE FROM ONE THAT HAS STOPPED WORKING. THALAMUS validated their
workflow edit with a linter that emitted two known-stale complaints; rather than
trust a tool whose only output was noise, they PLANTED A TYPO and confirmed it
flagged that too. Any tool with standing ignorable output needs that check before
its silence means anything.

## A derived claim and a measured one must not share a format

I published a false claim about another repo TWENTY MINUTES AFTER MEASURING THE
TRUTH, and the measurement was in my own output the whole time.

THE SEQUENCE: a sweep printed `plexus: clean`, from a loop that opened with
`[ -d "$r/.github/workflows" ] || continue` -- so `clean` MEANT the repo had
workflows and was scanned. Twenty minutes later I read that repo's Cargo.toml,
saw absolute path dependencies, reasoned "a hosted runner cannot build this,
therefore CI is impossible here", and sent two seats a table reading `NO CI`.
The repo had been green on every push for two days.

I DID NOT FAIL TO CHECK. I CHECKED, GOT THE RIGHT ANSWER, AND OVERWROTE IT WITH
AN INFERENCE. The inference was more recent, more vivid, and had a MECHANISM --
and a mechanism feels like understanding in a way a bare observation does not.
The measurement was one word in a scroll-back list.

THE TABLE IS WHERE IT BECAME UNRECOVERABLE. Aligned columns present a filesystem
observation and a thirty-second-old conclusion as THE SAME KIND OF FACT. Erasing
provenance is what tables are FOR, and it is exactly wrong when the rows differ
in epistemic status. In prose I would have had to write "plexus, which I have not
checked, presumably" -- and I would have checked.

TWO RULES:
· WHEN A MECHANISM CONTRADICTS SOMETHING YOU MEASURED, THE MEASUREMENT WINS AND
  THE MECHANISM IS THE SUSPECT. Reversing that is the natural direction, not
  carelessness; the explanation arrives with more force than the fact.
· ASK THE AUTHORITATIVE QUERY, NOT A PROXY FOR IT. "Does this repo verify on
  push" is answered by `gh run list`, which cannot be satisfied by a repo that
  does not. File layout and dependency-path form are proxies, and a proxy is
  where a plausible mechanism gets to substitute itself for an answer.

A SECOND INSTANCE, from PLEX the same night, which is what makes this a class
rather than one person's lapse: their post-merge gate failed, they had a
MECHANISM (new transport, new blocking calls, plausible boot hang), and it
outranked TWO measurements they already held -- the same failure signature on
main, and a pre-merge flake rate they had measured themselves at 10/12. They
bisected the diff before checking the environment. The cause was the machine.
Same shape, and it cost a detour rather than a false claim only because they
were arguing with a test rather than with another seat.

AND THE UNCOMFORTABLE COROLLARY, theirs: THE TIDIER THE OUTPUT FORMAT, THE MORE
PROVENANCE IT DESTROYS -- and we reach for tables precisely when reporting to
someone else, which is the moment the loss matters most. A table sent to a peer
is the highest-stakes place to erase how you know each cell.

## A pending check is the one status nobody reads as broken

A workflow that CANNOT ACQUIRE A RUNNER does not fail. It queues, renders as
pending, and gets cancelled by the next push -- so it produces neither a red
mark nor a green one, and a reader scanning a checks list sees "still running"
for as long as the repo exists.

MEASURED IN MY OWN REPO, minutes after landing the authoritative-query rule and
applying it here rather than reading the workflow file: 31 runs, 30 cancelled,
one in flight. Zero successes, zero failures, and ZERO STEPS EXECUTED in any of
them. Run lifetimes from 8 minutes to 7 hours, which is not a build -- it is a
queue wait terminated by the next push. The lane had never verified anything and
nothing anywhere said so.

WHY IT SURVIVES: every other absence has a signal. A deleted workflow disappears
from the checks list; a failing one is red; a skipped one says skipped. QUEUED
IS THE ONLY STATE THAT LOOKS LIKE WORK IN PROGRESS, and it looks like that
permanently.

THE CHECK: for each workflow, ask for its CONCLUSION DISTRIBUTION, not its most
recent status. `gh run list --workflow=X --limit 60 --json conclusion` grouped by
value answers "has this ever completed" in one command. A tail of cancellations
is normal on a busy branch; ZERO SUCCESSES ACROSS THE WHOLE HISTORY is a lane
that has never run.

AND CHECK WHETHER ANYTHING CALLS IT before touching it. A workflow declaring
`workflow_call` may be a dependency of a release lane, in which case a
never-acquirable runner is not a dead lane but a HANG in something that matters.

WHEN THE CAUSE IS UNFIXABLE FROM THE FILE (here: GitHub-hosted runners blocked
for a free-plan private org, and the substitute runner class has no macOS),
annotate rather than delete. The job definitions are the specification of what
should run once a runner exists -- but leaving them UNANNOTATED is worse than
deleting them, because the pending check keeps reading as progress.

## A green run on a platform that lacks the expensive step is not evidence about
## the platform that has it

CEREB measured their conformance suite's `live_daemon` test at 0.19s on Linux CI
against 5-22s for a first run on macOS -- because the macOS first-exec
code-signature validation does not exist on Linux. THEIR CI IS THEREFORE
SYSTEMATICALLY BLIND TO THE ONE TIMING FAULT THAT REPO HAS ALREADY HAD.

The trap is in how the result reads. A green conformance run naturally reads as
"registration works"; what it establishes is "registration works WHERE THE
EXPENSIVE STEP DOES NOT HAPPEN". Narrow-but-true again, this time with the
narrowing supplied by the runner rather than by the check.

IT HAS A SIBLING WORTH SEEING TOGETHER. My own Swift lane asks for a macOS
runner this org's billing cannot provide: 31 runs, zero steps executed ever. So
across one fleet, one repo cannot test on macOS AT ALL and another tests on Linux
and inherits a blind spot for a macOS-only fault. Nobody chose either; both are
downstream of the same runner constraint. AND NEITHER IS VISIBLE IN A CHECKS
LIST -- one renders as pending, the other as green.

SO: when a fault class is platform-specific, ask whether the platform that
EXHIBITS it is the platform that RUNS the tests. If not, say so in the workflow.
The next reader will otherwise take the green at face value, and they will be
right to -- nothing in the output says which platform's answer they are holding.

## Two provisioning shapes, one with strictly more ways to be half-done

Same credential, two spellings, different failure surfaces. The fleet's CI token
minting exists in both: one form needs a single SECRET, the other needs a secret
AND a repository VARIABLE. Secrets and variables are set by different commands,
listed by different commands, and are easily provisioned in separate passes
months apart -- so the two-artifact form has strictly more ways to be
half-configured, and a missing variable surfaces as a TOKEN-MINTING FAILURE that
reads as a permissions problem.

CEREB caught theirs pre-flight by checking credentials BEFORE the first run
rather than debugging the failure after. That ordering is the transferable part:
for any new lane, enumerate what it reads from repo configuration and confirm
each one EXISTS, before spending a run to discover it.

THE FINDING IS NOT "ONE REPO WAS MISSING A VARIABLE" but "half the fleet is on
the shape where that is possible". I swept and found no live gap -- every repo
referencing a credential has it. Recording the asymmetry is the cheap half;
unifying four working workflows to remove a failure mode that is not firing is
churn with a live-credential blast radius.

## The field that would show the problem is often the field the problem freezes

A cached gauge refreshed by the operation that is stuck reports the state from
just before the stall, forever. It is not lying -- it answers a narrower question
than an operator reads it as -- and it fails in the direction of reassurance.

PROOF CASE: a backup module's health cache refreshes on publish. Publishing was
blocked, so `snapshotAgeMs` froze at the moment of the block and `ck health`
reported `ok` while the module had taken no new snapshot for 6.6 hours. Reading
the module's SQLite store directly is what surfaced it -- durable state moves
when the cached projection cannot.

THE SAME INCIDENT CARRIED A SECOND REPORTING DEFECT WORTH SEEING SEPARATELY.
The honest field WAS present and correct: `stagedUnpublished: 3`. But 3 is also
the hardcoded cap at which the scheduler stops capturing, so THE NUMBER THAT
MEANS "STOPPED" LOOKS EXACTLY LIKE THE NUMBER THAT MEANS "SLIGHTLY BEHIND". A
reader has to know the threshold to read the value as terminal. Where a gauge
crossing a threshold changes the system's BEHAVIOUR, the status must say so IN
WORDS -- "capture halted: backpressure cap reached" -- because a bare number
cannot distinguish running, behind-but-progressing, and stopped-pending-operator.

AND THE AUDIBLE SIGNAL WAS INAUDIBLE. The gate did log its refusal, to stderr,
from a supervised child whose stderr is inherited rather than captured to a file.
A correct log line that lands nowhere durable is the same as no log line. Check
where a component's stderr actually goes BEFORE relying on "it logs that".

THE GENERAL CHECK: for any cached health field, ask WHAT REFRESHES IT, and
whether that thing is inside the failure it is supposed to reveal. If it is, the
field is structurally incapable of reporting its own stall.

## Two stalls can wear one explanation

When a known outage is in progress, a second unrelated stall gets absorbed into
it -- the first explanation is available, plausible, and already believed.

In the case above, publishing was blocked on a credential deadlock that was
understood and being waited on. Captures stopping LOOKED like part of the same
story. It was not: captures are entirely local and need no credential, so the
deadlock could not explain them. Two stalls, one explanation, and the explanation
fitted the loud one well enough that nobody examined the quiet one.

THE DISCRIMINATOR IS DEPENDENCY, NOT TIMING: does the stalled thing actually
require the resource the known outage removed? If not, it is a second incident
wearing the first one's clothes, and the fact that it started at the same time is
not evidence -- a shared trigger and a shared cause are different claims.

## A single sample lands on one side of a split and reads as decisive either way

Checking ONE instance of a repeated condition tells you which side of the
distribution that instance is on, and nothing about the distribution.

PROOF CASE: a reclaim loop logged the same refusal for 422 distinct paths. I
checked one, found the directory present, and was about to report "the loop is
retrying live worktrees." Resolving all 422 gave 413 ABSENT and 9 present -- a
98/2 split, and my sample had landed in the 2%. The one-sample version was not
merely weaker; IT SUPPORTED THE OPPOSITE CONCLUSION, with the same confidence.

WHAT MAKES THIS WORSE THAN AN ORDINARY SMALL-SAMPLE PROBLEM: a repeated log line
INVITES the single check, because every instance looks like every other one. The
repetition that makes the sample feel representative is a property of the
MESSAGE, not of the underlying state.

CHEAP FIX: when the population is enumerable from the evidence you already have
(all the paths are IN the log), resolve ALL of it. 422 filesystem checks cost
under a second. Sample only when enumerating is genuinely expensive, and say so.

## A log line repeated per-item per-sweep broadcasts a static backlog

A loop that retries a permanently-failing item and logs each failure converts a
fixed backlog into unbounded output. Measured here: 8547 of 20000 lines (43%) in
a 3.5-hour window, one message, 422 items, ~84 retries each -- roughly 40 MB/day
into an unrotated 1.34 GB file.

THE SECOND-ORDER COST IS THE REAL ONE. Underneath those 8547 lines sat 828 and
776 instances of two DIFFERENT warnings that may be real. A dominant repeated
line does not merely waste bytes, it buries the signal you would need on the day
something else breaks -- and it does so in the single durable record of module
stderr, which is the thing you reach for during an incident.

IF A REFUSAL IS PERMANENT PENDING AN OPERATOR, SAY IT ONCE. Re-announcing a
static state on every sweep is the logging equivalent of a health gauge that
cannot distinguish stopped from slow: the reader cannot tell a new failure from
the eighty-fourth repeat of an old one.

AND CHECK WHETHER THE REFUSAL IS EVEN CORRECT. Here "cannot inspect" was being
returned for paths that DO NOT EXIST, where the reclaim goal is already
satisfied. Absence read as corruption -- the same absent-vs-unknown discriminator
as a missing config file read as an empty module list.

## A window sized in lines is a window sized in time at whatever rate the log runs

Verifying a fix that CHANGES A LOG'S RATE, using a line-bounded window, measures
mostly the period before the fix -- and attributes it to after.

PROOF CASE: a peer deployed a fix that cut a repeated error by 98%. I checked
`tail -20000`, counted 9093 instances, and nearly reported that the fix had
barely moved anything. Those 20,000 lines spanned 15:01 to 18:26; the deploy was
minutes old. Bounding to the recent tail gave 24.

THE MECHANISM IS SELF-REFERENTIAL, WHICH IS WHY IT IS EASY TO MISS: the line rate
is the quantity the fix changed, so a line-bounded window is sized by the very
variable under test. The higher the pre-fix volume, the further back the same
line count reaches, so THE MORE EFFECTIVE THE FIX, THE MORE PRE-FIX DATA THE
WINDOW SWALLOWS.

BOUND VERIFICATION WINDOWS IN TIME, or by a marker you can see (the deploy's own
first log line), never in lines -- and print the window's actual start timestamp
beside the count, so a wrong window announces itself instead of reading as a
result.

PAIRED WITH ITS MIRROR, from the same evening: checking ONE path out of 422 put
me on the wrong side of a 98/2 split. Too little data landed on the wrong side;
too much data landed in the wrong era. BOTH READ AS DECISIVE, and neither
announces its scope.

AND VERIFY A PEER'S FIX FROM YOUR OWN VANTAGE ANYWAY. Accepting the headline
would have been correct here -- but running it myself found 6 residual cases
outside the population they enumerated, which their own instrument could not see
because they were counting refusal REASONS while I was resolving PATHS.

## A claim about an action is not the action, and it hides the gap from you

I told a peer something false, discovered the truth two hours later while doing
unrelated work, mentioned the correction TO A DIFFERENT SEAT in passing, wrote in
my own summary that I had corrected the record with them -- AND NEVER SENT THEM
A MESSAGE. They rediscovered it independently and told me.

The ordinary version of this is a stale belief you fail to update. This was
worse: the correction was MADE, and the delivery was ASSERTED, so every later
review of my own state found the item closed. THE FAILURE WAS INVISIBLE TO ME AT
EXACTLY THE MOMENT I WAS RECORDING THE CATCH.

WHY IT GENERALISES: telling someone else about a correction, and writing that you
delivered it, both FEEL like the delivery. The satisfying part of fixing an error
is the knowing; the part that matters to anyone else is the sending, and only the
first produces any internal signal.

CHECK: when a summary says a correction was delivered, THE EVIDENCE IS A SENT
MESSAGE, not a memory of intending to send one. Same standard as "if there is no
tool result, it did not happen" -- applied to your own reports about yourself.

## A doc comment is an untested assertion unless something fails when it stops
## being true -- and the question is ENFORCEMENT, not truth

MY FIRST FORM OF THIS RULE ASKED THE WRONG QUESTION. "Is the comment true" is a
snapshot: you can only answer it for today, and when the answer is yes there is
no follow-up. ENGRAM's refinement transfers instead: THE EXPOSURE IS NOT THAT A
COMMENT IS WRONG, IT IS THAT IT IS UNENFORCED. Truth is a property of the moment;
enforcement is a property of the code.

THE DISCRIMINATOR IS WHETHER A FUTURE EDIT VIOLATING THE CLAIM WOULD FAIL
SOMETHING -- compile, test, or nothing. Not whether the claim is true, and NOT
whether it was hard to verify.

I FIRST WROTE THAT DIFFICULTY-OF-VERIFICATION WAS THE TELL, AND IT IS ONLY A
CORRELATION. ENGRAM broke it on an example I had graded myself: a function whose
comment said "called only by the Worker after auth", which I called structurally
enforced because the call graph had exactly one production caller. WALKING A CALL
GRAPH TELLS YOU WHAT IS; ENFORCEMENT IS ABOUT WHAT CANNOT BE. Nothing refuses a
second caller, and adding one breaks no compile and no test -- so it is
convention, not structure. A claim that is hand-checked can turn out structural
(a call in the Ok arm of a transaction), and a claim that reads structural can be
mere fact.

THREE CATEGORIES, ONLY ONE OF WHICH IS EXPOSURE:
· TESTED -- something fails when the claim goes false.
· STRUCTURALLY ENFORCED -- control flow, a type, or a schema holds it: a call
  sitting in the Ok arm of a transaction; `&self` making a "read-only" claim a
  compile error to violate; SQLite AUTOINCREMENT. STRONGEST OF THE THREE, because
  an edit cannot break it while leaving tests green. NOTE THAT ONE COMMENT CAN
  SPLIT: "read-only projection called after auth" is structural in its first half
  (`&self`) and convention in its second (a promise about callers).
· TRUE BY CONVENTION -- true today, held by nothing. This is the whole exposure.

THE UNIT OF ENFORCEMENT IS THE CLAIM, NOT THE COMMENT. ENGRAM's sharpest
observation: one sentence can carry two claims in two categories, and the
structural half LENDS ITS CREDIBILITY TO THE CONVENTIONAL ONE, so the whole
sentence reads as uniformly reliable. Split a comment into its claims before
grading any of them.

AND WHEN CONVERTING CONVENTION TO ENFORCEMENT, ORDER BY WHETHER A VIOLATION
WOULD BE SILENT. "The scheduler retries every tick" going false surfaces as a
stall someone notices; "called only after auth" going false surfaces as nothing
at all, until it matters. Same ordering principle as fixing instruments before
findings.

MEASURE PREVALENCE BEFORE LEGISLATING. Their sweep found 1 defect in 8
candidates. Endemic would justify a policy (test every behavioural comment); rare
justifies the cheap habit of checking a comment while you are in the file anyway.
A POLICY SIZED TO A RATE NOBODY MEASURED IS HOW A CODEBASE ACQUIRES CEREMONY THAT
CATCHES NOTHING.

AND REPORTING THE NEAR-NULL IS THE HARDER HALF: a sweep chartered to find
something produces something. Promoting a borderline case to a finding, to
justify the sweep, is the failure mode -- the round's result has to bind in the
direction where it says the thing is not here.

ENGRAM found two doc comments claiming a cache refreshed "at startup, after every
run, and on scheduler ticks". THE THIRD CLAUSE HAD NO CALL SITE ANYWHERE. A
comment asserting a behaviour that was never implemented, sitting directly above
the code that would have implemented it, with nothing testing the claim -- so it
read as documentation of a fact for as long as it has been false.

THIS ALSO CORRECTED MY DIAGNOSIS OF THE SYMPTOM. I had described the frozen
health gauge as a PROPERTY of a cache refreshed by the stalled operation --
structurally incapable of reporting its own stall. With the tick refresh the
comment promised, it was never structurally incapable: it was CAPABLE AND
UNWIRED. Right about the symptom, wrong about the mechanism, and the difference
matters because the two want different fixes (reinterpret the field vs wire the
refresh).

SO WHEN A COMMENT ENUMERATES CALL SITES OR TRIGGERS, RESOLVE THEM. A list of
three conditions where only two exist is the same shape as a transcribed
allowlist agreeing with its source only at the moment it was typed.

## An error return that skips the readback

A publish appeared stuck for thirteen hours. It had **already succeeded** on its
first attempt.

The server applied the change and finalised it in one transaction, then the
response was lost to a transport error. The client propagated that error
immediately — which skipped the very next line, a readback whose entire job is
deciding whether the change landed. Every attempt afterwards re-reserved the same
operation, was correctly told the reservation was already finalised, and failed.
**Each refusal was right. The recovery they demanded was unreachable.**

The general shape: **between a mutation and the check that determines its
outcome, an early return is not a safe default.** A failed send and a successful
send whose reply was lost are indistinguishable at the call site, and the
readback is the only thing that separates them. Returning the error throws away
the distinction and converts a transient into a permanent one.

Two diagnostic notes worth as much as the mechanism:

**The error text changed, and that was the signal.** Before a fix went in, the
failure said one thing; after, it said another. That transition proved the first
defect was genuinely repaired and had been *concealing* a second on the same
path. A fix that changes the symptom without removing it is evidence, not
disappointment.

**The stall was called on "neither expected outcome, and nothing in the log"** —
defined in advance as its own result rather than as slowness. Waiting longer
would have produced more of nothing. The state that looked like a dead loop was
actually a completed operation nobody could observe.

And the belief everyone held all day — that the last good backup was the previous
generation — was wrong in the reassuring direction. The data had been safe since
morning.

## Renaming past a fence

Planning a set of module renames, I enumerated the state keyed on the old names:
routing tables, permission lists, peer registries, worktree backpointers. All of
it fails as **silence**, and all of it is repairable afterwards by re-pointing.

The module's owner named the case I had missed. Their crash-safety journal — the
record that prevents a banked credit being spent twice — lives under a path
derived from the binary name. If a rename moves that directory, **the new binary
starts with an empty journal, and an empty journal is indistinguishable from a
journal with nothing pending.** The module reports healthy either way. The
failure is spending something real, twice.

So the classification that matters before any rename is **cache or fence**:

- A **cache** keyed on the old name fails as slowness. It announces itself, and
  rebuilding costs time.
- A **fence** keyed on the old name fails as a *clean start*. Nothing reports a
  fence failure, because a fence with no history looks exactly like a fence with
  nothing to stop.

Caches can be repaired after the fact. Fences must be migrated **before** the
flip, and verified with a check that distinguishes *legitimately empty* from
*empty because it moved* — inspect the pending set, not the file's existence.

The owner's own sentence is the one to keep: *"renamed and healthy will look
identical whether or not I got this right."* Health is not evidence for a
property health does not measure.

### The component is not the only resident

Before renaming a directory I asked its owner whether any of their durable state
was keyed on the old path. They audited thoroughly and answered from the running
process rather than from source: every path came from configuration supplied
externally, so the folder could not reach them even in principle. Correct, and
verified better than I had asked for.

The rename **took their entire tool surface down.** Their working session binds a
project root at startup and gates every operation on that path existing. Not the
component's state — the *agent's own footing*. They could not read a file or run a
command to diagnose it.

**The service and whoever operates it are different subjects with different
failure modes**, and asking an owner "is your state safe" reliably gets the
component answer, because component state is what an owner thinks of as theirs. I
knew the operation would delete a directory somebody was working inside, had a
full day of evidence that path-keyed state is the biting class, and still scoped
the question to components because components were what I had been enumerating.

Two procedure changes, both from the person I broke:

**Put the compatibility link in before the move, not after the breakage.** Created
ahead of time it converts a hard break into a soft one at no cost. Created
afterwards it is a recovery, and the resident spends the intervening minutes
unable to run the very commands needed to diagnose it.

**Ask the resident to verify their own footing as a separate question.** Same
rename, different subject, different answer.

### The shim repairs one faculty and breaks another

The compatibility link that restored the resident's tools then broke their
**incoming messages**, and the mechanism is worth reading carefully because every
value involved was correct.

Their session resolves the link and reports its location as the *new* path. Every
peer that writes to them still records the *old* one. Both strings name the same
directory. Neither side can detect the other's spelling. So messages were stored
under one key and queried under the other: the wake arrived, the notification
preview arrived, and the body was unreachable. **Their inbox reported empty.**

The same schema property hid four of their five contacts — rows scoped by
location, written under the old spelling, invisible to a query under the new one.
**Not deleted. Invisible.**

Three things generalise:

**A resolved-versus-logical path disagreement is undetectable from either end.**
Storing a canonical form at write time — fully resolved, links followed — removes
the class rather than mitigating it.

**Two surfaces keyed on the same value independently break separately.** The
resident repaired their contact list and their inbox stayed blind, which correctly
proved the two were not one lookup. Repairing one tells you nothing about the
other.

**Reporting "empty" for a scope with no rows cannot distinguish no-mail from
wrong-scope.** A count of items addressed to this recipient *regardless of scope*
makes the difference legible immediately: 699 waiting under a key you are not
reading is a diagnosis; "empty" is not.

### The survivor that matches everything

One contact survived while four vanished, stamped from weeks earlier. That
asymmetry drove the diagnosis toward "some state moved and some did not" — a
partial failure, which is a much harder thing to reason about than a scope change.

The survivor had been written before the scoping column was populated, so its key
was **NULL, and a NULL scope matches every scope.** It had not survived anything;
it was never scoped in the first place.

The resident's own reasoning was sound and the conclusion was one step too far:
*"a fresh store would not hold a 34-day-old entry"* is correct, and does not imply
a partition. **A legacy row with an empty key is indistinguishable from a row that
survived a migration**, and it will always point at the more alarming explanation.

### The shim that quietly becomes load-bearing

The same person then corrected their own advice, having lived through it: the
compatibility link is right and **insufficient**. A link created ahead of the move
keeps the resident alive, but leaves it bound to a path that is now a shim —
functional, and quietly wrong in a way that surfaces only when someone tidies up.

So the procedure is three steps, not two: rename, link, **restart the resident so
it re-binds to the real path**, and only then remove the link.

They also declined to hand over their identifier for broadcast, which was the
sharper call. Their session was still bound to the old path — the link was what
made it resolve — so publishing that identifier would have put a value into every
registry in the fleet **that works only while the shim exists.** It would have
looked verified, passed every test, and broken silently the moment the shim came
out. Strictly worse than publishing nothing: a missing entry fails loudly on first
use, while a shim-dependent one fails weeks later with no proximate cause and
nobody remembering the link was load-bearing.

That reordering matters generally: **the restart is not cleanup, it is a
dependency.** Everything downstream — the identifier, the broadcast, the link's
removal — gates on it.

One thing went right and is worth copying: **the failure was loud.** The refusal
named the path, the condition, and the likely cause, so it cost minutes. Every
other path-keyed failure that day — permissions, registries, routing — failed as
silence. Whoever wrote that check chose the right failure mode, which is why the
victim could hand me a precise remedy instead of a symptom.

### The survival check that passes by luck

I wrote that rule as: *verify the pending set, not the file's existence.* A second
owner found the hole. Their pending table was **legitimately empty that day**, so
"empty" was consistent with both a correctly migrated store and a freshly created
one. The check passes by coincidence in exactly the case it exists to catch — the
same defect as an existence check passing a stale path, one level in.

What they proposed instead: their store carries an **incarnation identifier minted
once at creation and never recomputed.** A recreated store produces a *different*
one rather than a missing one, so the check cannot be satisfied by an empty
state.

The general form: **verify an identity only the original could carry.** Not an
absence, and not a count that might legitimately be zero. Counts are
corroboration; the minted identifier is the proof.

Two further details from the same review, both of which I would have got right for
the wrong reason:

**Move, never copy.** I would have moved out of tidiness. The actual reason is
that the single-writer lock is per-location, so a copy leaves a second openable
store and two live writers become possible.

**Move while stopped, before restart.** The storage location is handed to the
module when it connects, so a module that restarts before the move **creates a
fresh empty store at the new location** — leaving two to reconcile. That is a
narrow ordering window, and it belongs in the procedure as a hard sequence rather
than as a note.

The same owner sharpened a check I had been broadcasting. I had said: after
re-registering, confirm the bound directory exists. **During a rename both the
old and new paths may exist simultaneously**, so an existence check passes for the
old path throughout the exact window in which messages are silently going
nowhere. The check must be that the bound directory **is the new root** — a
positive identity, not an absence.

## An existence check that passes the worst case

After a rename broke message routing between agents, I told several teams the
remedy: re-register the moved peer, then confirm the recorded directory
**exists**.

That check passes the worst case. The recorded directory is derived from wherever
the named session happens to run, and a session id can belong to a short-lived
background worker rather than to the agent itself. **A worker whose working
directory is still live yields an existing path and an undeliverable inbox** —
clean by my rule. Existence only catches the workers whose directories have
already been cleaned up.

The discriminating check is **the shape of the path**: a real project root means a
real session; anything under a worktree directory means the id belongs to a
worker. Same output, different question — it turns the recorded directory from a
property you inspect into a **test of the identifier**.

Two details worth carrying:

One team had the worktree path in their own output, reported it verbatim, and
read it as a symptom rather than as the diagnosis — then re-ran the registration
twice to confirm the path was stable. **A test of the wrong hypothesis, executed
carefully**, which is the expensive kind: the rigour makes the conclusion feel
earned.

And the failure mode is why a structural check is required rather than an
attentive one: **the only signal is a reply that never arrives, and silence is
indistinguishable from a busy recipient.** Someone did ask for confirmation
through a bad entry and got none — which was the evidence, visible only in
hindsight.

## Consumed, or merely retained

Free disk space fell steadily for an hour — measured at 737 MB/min over a full
minute — while **every directory sampled read flat.** I spent that hour hunting a
writer: databases, staging directories, container images, log stores,
deleted-but-open files, backup processes. All flat. One candidate looked live
only because a modified-time search matched a *touch* rather than growth, which I
reported and withdrew.

The owner ended it in one move: he deleted about 150 GB and **free space did not
change.**

That is not consumption. A filesystem snapshot taken that morning was retaining
every file deleted since — unlinked, but still referenced, so nothing was ever
released. Removing the snapshot returned the space immediately, and it kept
climbing for minutes as blocks were released.

**A falling free-space number looks identical under consumption and under
retention, and the whole search strategy differs.** Under consumption you hunt a
writer; under retention there is no writer to find, which is exactly why every
directory read flat. That flatness was the answer, and I read it as a failure to
locate the cause.

The discriminating test costs one command: **delete something large and see
whether the free number moves.** If it does not, stop looking for a writer. That
is a far sharper instrument than watching a number fall, and it was available the
whole time.

Generalises past disk: any exhausted resource whose usage only rises — memory
held by a cache that never evicts, connections held by a pool that never reaps,
file descriptors retained after close. **Ask whether release works before asking
who is allocating.**

## A rule that says "stable" without saying "which"

We adopted a rule requiring a binary's signature identity to be **stable across
rebuilds**, because a value that changes every build silently revokes the
permissions attached to it.

Within hours it produced a collision. A test binary and a production binary, under
two different supervisors, were sharing one identity — so the test instance was
operating under production's permissions. It worked, which is what made it a
problem rather than an error: nothing denied, nothing logged.

The author's own diagnosis is the lesson: **the rule said the identity must be
stable. It did not say which identity.** They filled the gap with the only name in
front of them — the artifact's build-time filename — and never asked what the file
would be *called* where it was going. **Stability and correctness are different
properties; they verified the first and assumed the second.**

The repair is to state the rule so it is checkable rather than rememberable: the
identity must match **the filename the binary is deployed as, read from the
consumer's configuration.** That turns it into one line per binary.

My own wording had the same hole from the other side. The runbook said *use a
distinct identifier per environment* — true, and silent on where to get the name.

### A defect whose trigger is the remediation

The sweep's sharpest residual was a test binary carrying **production's name in
the unstable form.** It is not colliding today only because it was never signed
with an explicit identity — the changing suffix keeps it distinct by accident.

Apply the stability rule to it, correctly, using the name it already carries, and
it becomes production's principal. **The fix arms the defect.**

That is worth holding as its own class rather than as an instance: a dormant
problem that the remediation converts into a live one. It is invisible to the
partial rule, because "make this stable" is satisfied by pinning whatever value is
already there — and the person doing it will have followed the instruction
exactly.

The question to carry into any cleanup pass: **what does this fix arm?** Applying
a rule to something already holding a wrong value is not the same as applying it
to something holding none.

### The term a careful reader has to guess

Their generalisation is the most useful rule-writing check I have been given:
**when you write a rule, ask which of its terms a careful reader would have to
guess. That is the term that will be filled in wrong, and it will be filled in
wrong by someone following the rule correctly.**

It beats "write clearer rules" because it is a check rather than an aspiration,
and it runs at authoring time against the rule's own text.

It also predicts the *failure mode*, not merely the risk. An underspecified term
is not filled in randomly — it is filled with **the nearest available value**,
which here was the artifact's build-time filename, the only name in the room. That
is why the result looked so clean: every instrument measured the property the rule
named, and the rule named the wrong property.

The part worth sitting with is that **compliance produced the defect.** Not a
shortcut and not carelessness — the author followed their own rule exactly, and
the rule was silent where it mattered. Diligence cannot catch this class, because
diligence is applied to the stated term.

Sweeping the fleet that way found the original collision plus two things nobody
was looking for: a binary whose identity was applied by the build tool and never
signed by anyone, and a test binary carrying production's *name* in derived form —
latent, and guaranteed to collide the moment someone did the right thing by
halves.

### Same build, different principal

The corrected artifact came with a deliberately constructed pair: two binaries
with the **same build identifier** and **different signature identities**, and
therefore different content hashes.

A hash comparison calls them different software. They are the same software with
different principals. Worth keeping as the standing demonstration that a digest
conflates *what was built* with *who it claims to be*, and that the build
identifier and the signature identity answer those two questions separately.

## Your own ruling, cited back at you

Adjudicating a design question from someone else's quotations of their own
document, I disclosed that I could not read their source and asked them to correct
me rather than take my agreement as confirmation.

They checked, and their quotations were verbatim. **But one of the rulings they
cited was mine** — sent to them earlier that day, not yet written into their
document. So when I went looking for it in their repo to check the citation, I was
looking for my own words. **Had I found them, I would have read them back as
independent corroboration.**

The circularity the disclosure guards against was present; it simply sat one
citation over from where I looked. And the mechanism is nastier than ordinary
circular reasoning: **the act of verifying is what delivers it.** A ruling that has
travelled into someone else's document looks exactly like an independent source,
because by then it *is* in an independent document.

Two practices follow, both theirs:

**Attribute a borrowed ruling where it lands.** One ruling cited once, with its
origin recorded, rather than two mentions of the same thing that a later reader
counts as two.

**Prefer the argument that does not depend on the borrowed judgement.** The seam
question had two supports: one resting on my judgement about a specific category,
the other on a structural property. They adopted it on the structural one — which
is the right move whenever support turns out to be self-referential, because it
leaves the conclusion resting on something neither party supplied.

## A gate expensive enough to route around

Adjudicating where a permission check belongs for a new capability, the strongest
argument turned out to be one that reads like convenience: **an agent that cannot
cheaply glance at its own work will reach for the disruptive path instead.**

That is a security argument wearing ergonomic clothes. A gate expensive enough to
avoid **gets avoided**, and what replaces it is worse and unmonitored. Prompting
before every read does not buy safety; it buys a workaround.

It needs writing down explicitly wherever it applies, precisely because a future
reviewer will read the cheap path as laxity and tighten it — correctly by local
reasoning, and wrongly overall.

### Put the seam on a property, not on a classification

The same decision asked whether the gate should key on which subsystem a
capability lives in, or on what the capability *does*.

A subsystem boundary is a classification **someone maintains**, so every new
capability inherits its gate from where it happened to land — which is how a
write ends up behind a read's gate one reorganisation later. Keying on the
action's own declared kind makes the gate follow the property, and lets anything
that declares nothing default to the strict side.

### A grant nobody could say they issued

One rejected option derived the permission from ambient state — whichever
application happened to be in front. The obvious objection is the race between
deciding and acting. The disqualifying objection is different: **the permission
would come from state the user does not experience as a decision.** Clicking a
window is not consenting to it being driven.

Every other option had somebody making a choice. That one produced a permission
**nobody could later say they issued**, which rules it out independently of any
race.

## A workaround that survives its own obsolescence

A missing conformance meant errors reached one client as a dump of internal
structure rather than a sentence. That client wrote a local repair, and reported
the gap rather than keeping the fix to itself — correctly, since a client-side
workaround only helps the clients that write one.

When the fix landed at the source, they found **two** local repairs superseded, not
one. The second was a mapping from bare status words to friendly sentences. It
matched those words *whole*, deliberately, so richer messages kept their detail —
and the new descriptions render those same cases as full sentences, so **the match
can never succeed again.**

Nothing failed. Nothing warned. A rule that no input can reach still reads to the
next person as an active safeguard, which is worse than absent code because it is
trusted.

**Its test would have passed forever**, and the reason is the shape this family
keeps producing: the test *constructed* the bare status word itself rather than
obtaining one from the system. So it proved the function worked and never that
anything could reach it. It took an unrelated upstream fix to expose it.

The generalisation is the part worth keeping: **a workaround for a missing
upstream capability does not merely fail to help others — it survives its own
obsolescence silently**, because nothing breaks when the real fix arrives. That is
a second, independent reason to report a gap rather than patch around it.

Worth copying too: they verified the upstream fix **before** deleting their
workaround, by bypassing it and exercising the real path. Deleting first and
checking after would have proven nothing about which of the two was doing the
work.

## A wrong mechanism behind a right conclusion

A colleague testing their own migration checks found that copying a database's
main file alone — leaving behind the sidecar holding recently committed data —
passed three of their four checks. The values those checks read were old enough to
have been folded into the main file already; 520 recent rows existed only in the
sidecar.

I reproduced it and got a **different result**: my copy would not open read-only
at all. Opened read-write it opened fine, and a subsequent read-only open then
also succeeded. I concluded the read-write open had **recovered** the database,
and wrote that inspecting it destroys the evidence of the loss.

They checked and refuted it. The main file is **byte-identical** before and after
— I verified the same checksum myself. What a read-write open creates is the two
sidecar files; nothing is folded into the main one. The second read-only open
succeeds only because the file it needs now exists.

My conclusion survived; **my mechanism was wrong, and it mattered.** "Do not
inspect it read-write or you will destroy the evidence" is false, and would send
someone hunting for a recoverable state that never existed. The true statement is
simpler and worse: **the copy is missing its tail from the instant it is made**,
and no flag on the reader changes that.

The read-only flag is still load-bearing, for a sharper reason than I gave. Not
because read-write is destructive — because **read-only cannot create the sidecars
it needs**, so an incomplete copy is *unreadable* under it and *silently short*
without it. The flag converts a silent wrong answer into a hard error.

Which makes **the error the finding.** A refusal to open, on a copy just made, is
not an obstacle to work around by dropping the flag — it is the result. Anyone who
retries without it has converted a correct alarm into a wrong number, and that is
the likeliest way this bites in practice.

**Add one continuously growing table to any such check.** Values that stopped
changing long ago cannot detect a lost tail; only something that grows can.

And the divergence has its own lesson, which they stated better than I could:
their written instruction said read-only, and they measured with a bare command.
**A procedure verified by its author running something adjacent to it is not
verified.** It surfaced in my hands rather than theirs, because I followed their
written instruction and they followed their habit.

## A scan that reads a fraction of the file

A colleague's sweep excluded test code by cutting each file at the first
test-only attribute. Some crates put that attribute on **production** items — a
test-only constructor beside the real one, an injection point — so the cut landed
near the top. Their scan read **32% of one file** and reported a property missing
that was present twenty-four lines past where it stopped looking.

I ran the same measurement against my own crates and it is worse here:

- the module registry: cut at line 17 of 202 — **reads 8%**
- the routing table, where most of the interesting invariants live: line 407 of
  2562 — **reads 15%**

Any sweep of mine using that anchor would have reported on a twelfth of the most
important file and produced clean, plausible output.

**The direction is what makes it expensive.** Truncation reports things *absent*
that are present, never the reverse. So every artefact is a false "missing", which
reads as a finding — you investigate, discover it is fine, and file it as a false
positive. **A real gap in the same run is indistinguishable from that noise.** The
instrument does not fail loudly; it fails by producing exactly the kind of output
a sweep is supposed to produce.

Two fixes, and the second is the one that generalises: anchor on the attribute
*immediately followed by* a test module, and **print how much of each file the scan
read** before believing any result. The anchor fix alone is correct and silent;
the coverage line is what makes the next wrong anchor visible. It is the same move
as everywhere else — emit the distinction rather than document it.

Their framing is worth keeping exactly: *a scan reporting on a third of a file is
not a sweep with a caveat, it is a different question wearing the same name.*

## Two correct measurements wearing one name

Verifying someone else's migration, I compared their reported counts against a
baseline I had recorded beforehand. Two numbers disagreed.

Neither was wrong. They had counted one column; I had counted a different one, and
**both are reasonably described as "distinct scopes."** One is the target's
location, the other the owner's — and only the second is what a lookup keys on.
Confirming their column reproduced their number exactly.

A disagreement between two careful parties is worth one question before it becomes
a defect report: **are we measuring the same quantity?** Two correct measurements
of different things wearing one name look exactly like an error.

What kept it cheap was having written the baseline down rather than recalling it,
which let me enumerate **which** scopes had gone and confirm each was slated for
removal. A decrease is acceptable only if you can name every member of it.

A third number I could not confirm at all — they reported a row count four lower
than mine, almost certainly messages delivered in between, including the exchange
itself. I reported what I measured rather than reconciling to theirs, on the rule
that a count taken from a live system is a floor and not an equality.

### Widening a reply is narrowing at the far end

The same deployment broke every running consumer by adding a field to a reply.
The service redeploys in minutes; its consumers reload only when their host
restarts. **So a widened reply arrives at consumers that cannot parse it**, and the
change that looks additive at the source is a breaking change at the destination.

The correct default is opt-in: new callers request the richer shape, existing ones
keep what they had. It fits the general ordering rule — the side that refuses
earlier deploys first, and a consumer that cannot parse is refusing.

## Publish the value that discriminates, not the one that describes

Preparing a joint test run surfaced five distinct defect classes before a single
step of it executed. The tempting conclusion is that careful preparation pays.
That is not what happened.

Every one came from **the preparation forcing someone to state a claim precisely
enough to be wrong in public.** Someone published a rule about keeping an identity
stable, then had to say *which* identity, and found they had guessed. Someone
asserted four items were closed, then had to say *where* closed, and the ancestry
check fell out. Someone published a hash, then had to say which of two siblings,
and the discriminating fact turned out to be the signature rather than the digest.

Each was a claim made specific enough that **its own author could check it.** The
test run would have found the same defects — one at a time, several steps in, each
presenting as something else. **The difference is attribution, not detection.**

So the transferable rule is narrower than "prepare more carefully": when handing
over an artifact, **publish the values that discriminate it, not the ones that
describe it.** A hash describes; an identity discriminates. "Closed" describes; an
ancestry relation discriminates.

The person who measured the wrong sibling put it best: they verified by hash when
the discriminating fact was the signature — **and the hash was correct, which is
exactly what made it useless.** A right answer to the wrong question is the whole
family in one line.

## Closed where it was made true, open where it must be

Two teams' checklists agreed that all four outstanding items were closed. Both
checklists were accurate. One of the items was still live.

The fix was committed, tested and merged — so reading the *repository* gave a
correct answer to "is this closed". The place it had to be true was a **running
binary**, built before the fix landed. Had the run proceeded, one side would have
emitted a field the other silently could not read, reproducing the exact ambiguity
that had failed the previous attempt — with the fix merged, green and pushed the
whole time.

**Cross-checking between two parties does not detect this.** Agreement is precisely
what you get when both sides consult the same correct-but-wrong-scoped source.
Neither ledger was stale; each was accurate about its own subject.

The resolving check is one line and takes no judgement: **is the fix's commit an
ancestor of the commit the running artifact was built from?** Not *is it merged*,
not *is the build green*, not *does the tracker say closed* — an ancestry relation
between the fix and the thing actually executing.

It generalises past deployments, which is why it belongs here rather than in a
deployment procedure. Any claim of the form *X is fixed* has to name **the place
where it must be true**, not the place where it was made true: merged versus
deployed, published versus consumed, written versus read.

### A digest cannot tell you which principal

The same delivery included two binaries from one build, differing only in signing
identity — so same build identifier, different content hashes, and only one
belonged on the test machine.

**Verifying the digest cannot catch placing the wrong one.** The hash tells you
which bytes, not which identity. The filename is a claim; the signature is the
fact. Check the identity explicitly, before placing.

## The one sentence under all of it

A colleague reduced an evening of separate findings to one line, and it holds:

> **Two outcomes rendering identically means the reader supplies the difference
> from expectation.**

Every fix that evening was the same fix. A response field distinguishing
inheritance from refusal. Distinct signing identities. A counter reporting absent
rather than zero. Sending a value even when it is false.

And it unifies the instrument failures with the wire failures, which are easy to
file separately: *nothing to review* over an unstaged file, an inbox reporting
empty over hundreds of waiting messages, a mutation that never applied, a build
cancelled with zero steps. Same shape, different layers.

The reason it keeps costing is that **expectation is a plausible source.** The
reader is not careless — they fill the gap with the most likely value, and most of
the time they are right. That is what makes the failures rare enough to be
expensive.

**The fix is always to emit the distinction rather than to document it.**

## A success report from an instrument that never touched its target

Three instrument failures in one evening, across three tools and three people,
all the same shape:

- A comment review over a file that was not yet staged: *"no changes to review."*
- **A mutation patch that failed to apply**, so the suite ran against the unmodified
  code and its pass was read as *the mutant surviving*.
- A query matching nothing because it compared an eight-character abbreviation
  against a seven-character slice, reporting zero results for something that
  existed.

The middle one is the worst, and inverts its verdict: a passing suite is the
strongest possible evidence that a test is vacuous, and here it was manufactured
by the test being fine. Mutation testing is also the instrument we reach for to
check *other* instruments, so a silent failure there corrupts the layer used to
detect corruption.

What unites them: **a success report from an instrument that never touched its
target.** Every one answered well-formedly. That is what makes the family
dangerous — an absence invites interrogation, while a well-formed positive verdict
closes the question.

The defence is identical in all three: **prove the instrument touched its target
before reading its verdict.** Stage before reviewing. Compare the file before
believing a mutation ran. Run a positive control before trusting an empty result.

### When two rules collide

A test artifact arrived carrying the production signing identity, violating a rule
about keeping those separate. Re-signing it would have violated a different rule:
do not change an artifact after others have verified its bytes.

I decided on consequences and a colleague named the principle: **the rule
protecting the validity of what you are about to measure wins.** The artifact under
test must be the artifact everyone verified — that is a correctness property of
the experiment. Keeping identities separate is hygiene: a real risk, but about a
different failure.

The half that stops this becoming an excuse: **the residual goes to the source.**
Declining to fix something late is only correct if it gets fixed where it belongs.
They recorded it against the artifact itself — *accepted for this round, fix at
next build* — so "not my step" cannot quietly become "nobody's step".

## Recovering an instrument's ability to fail

Correcting when we sign a binary — at build time rather than at deployment — had an
effect neither of us predicted: **whole-file hashes started matching across a
deployment for the first time.** Signing rewrites bytes inside the file, so while
we signed after copying, the deployed file never matched the staged one.

I recorded that as a convenience: the simplest check works again. The colleague
who proposed the change had the better reading. **A mismatch now means
something.** Previously a mismatch was expected noise, so the check gave no signal
on a match *and* no signal on a mismatch — it could not fire usefully in either
direction. Now a mismatch means truncation or substitution.

We did not recover a check. **We recovered its ability to fail**, and an instrument
that cannot fail is not a weak instrument.

It also retires a habit: reaching for harder instruments when the simple one will
do. The others remain for the questions they actually answer — the build
identifier for *is this the same build across a re-sign*, file identity for *is
the running process using the file I placed*. Different questions, not redundant
ones.

## An answer to a question you did not ask

I ran a comment review over a document I had just written and got "no changes to
review." I ran it again more explicitly and got the same. Both replies were
correct: the file was **untracked**, so it was absent from the diff the tool
reads. I nearly took the second reply as a pass.

This belongs with the other absence failures but is worse in one specific way.
An empty result set, a missing run, an unfinished search — each is an *absence*,
and an absence can at least be interrogated. **This one arrives as a
positive-sounding verdict.** "Nothing to review" is a sentence in the shape of an
answer, and it is well-formed whether or not it addresses what you meant.

The tell, once named, is general: **when a tool answers a question you did not
ask, the answer is still well-formed.** Nothing about its phrasing reveals the
substitution.

The fix is structural rather than attentional: **stage first, review second.** That
removes the state in which the two outcomes look alike, instead of requiring
anyone to notice the difference between them.

A colleague ran the same check against their own work after I reported the miss.
Theirs was clean — and they made the point that the check only existed because a
process slip was reported rather than quietly fixed.

## An instrument pair that cannot answer the question

A colleague tested whether a compatibility link is preserved or resolved when a
process works through it. They compared the shell's `pwd` against `pwd -P`, saw
the resolved form, and concluded the link is resolved at use.

I reproduced it and got the opposite answer. Both results were real:

- shell builtin `pwd` → keeps the link
- `pwd -P` → resolves, **by definition**
- `getcwd(2)` → always resolves

The kernel never stored the link. `chdir` records an inode, so the system call can
only ever reconstruct the physical path; the shell separately remembers the
logical path and hands it back.

**Their instrument pair could not have answered their question.** One member is
defined to normalise, so the difference between them is the definition — not
evidence about the other member's behaviour. A comparison where one side is
specified to do the thing you are testing for tells you nothing.

The method that resolved it, in their words: **when two instruments disagree,
establish what each one measures before interpreting the difference.** They had a
disagreement and reached straight for a story about the subject; the disagreement
was a property of the instruments.

Operationally: **add instruments until the disagreements are explained, rather
than explaining the first one you see.** The four-way comparison worked not
because four is a useful number but because the additions were of *different
kinds* — a shell builtin, a separate program, the raw system call. Three tools
that all consult the same remembered value would have agreed with each other and
taught nothing.

### A check whose validity window opens after the gate

They tested my correction rather than accepting it, and found the sharper version.

The shell validates its remembered path against the directory's identity and
discards it silently when they disagree. Measured: a stale logical path survives
only while it still names the same directory. So the string is ambiguous
**precisely and only while the compatibility link exists.**

I had called the check unreliable in general. It is narrower and worse than that:
**uninformative exactly during the interval it would be used, and informative only
after the thing it was meant to authorise has already happened.**

Their statement of it is the general shape: *a check that becomes valid only after
the decision it gates is not a weak check, it is no check.* Worth separating from
the usual question. Most checks are classified by whether they *can* be wrong;
this one classifies by **when** they are wrong relative to the decision. A check
can be perfectly sound and still worthless if its validity window opens after the
gate.

### Ambiguous in both directions is not weak evidence

The consequence was worse than either of us first thought, and it strengthened
their proposal rather than weakening it.

Which path a process reports depends on **how** it obtained it, and that is not
visible from inside. So a verification comparing path strings could read *new*
while still running through the link, or *old* while correctly rebound. Wrong in
both directions is not a weak check; it is not a check.

Their replacement is behavioural and cannot be fooled by which representation
someone captured: **remove the link first, then have the process act.** Correctly
rebound, it works. Still on the link, it fails immediately and unmistakably — the
same failure an unprepared move would cause, but triggered deliberately while
someone is watching and can restore it in one command, rather than surfacing days
later when someone clears out stale links.

The general form: when a state check can be satisfied for the wrong reason,
**replace it with an action whose success requires the state to be right.**

## The confound that decorates a success

Before a rename window, the owner of an adjacent component warned me that an
unrelated deployment would change a number I was about to use as verification.
Today the machine publishes **no** network candidates, so "none" is the correct
baseline; after their next binary update it publishes **three**, and that change
has nothing to do with my rename.

Without the warning I would have renamed, seen the count go from zero to three,
and had no way to attribute it. Worse: **the direction looks like success.** Three
real reachable addresses appearing right after an operation reads as everything
working, so I would not have investigated, and the misattribution would have
entered the record uncorrected.

That is the asymmetry worth naming. A confound that hides a failure eventually
announces itself, because something stays broken. **A confound that decorates a
success is never examined, because nobody re-derives a number that agrees with
them.**

The remedy is cheap and has to happen *before*: record both expected values in
advance — what the observable should read now, and what it should read after the
unrelated change — so that either one is confirmable and anything else is a
finding.

The same exchange carried a second lesson. They ran their interface enumeration
**against the actual machine** rather than trusting their fixtures, and it
immediately surfaced a specimen no fixture would have held: an address that
passes every predicate and silently times out. Twenty-eight interfaces reduced to
three, and the exclusion that mattered was one nobody would have thought to write
down. A fixture encodes what its author believes the world looks like.

## Shared infrastructure: down, or contended

Three teams reported the same shared build pool as unavailable — repeated
zero-progress failures, an annotation naming the pool, and one team's clean
before/after boundary across an unchanged configuration.

I checked my own runs and found **a fully green build on that pool, four minutes
after another team's third consecutive failure on it.** Real work — four jobs,
thirteen executed steps each. I broadcast a correction: not an outage, contention.

**The correction was wrong.** The provider's status page showed a major outage,
opened an hour and a half earlier, with an update reading *"some workflow runs are
failing to start."* The incident's start time sat exactly inside the reporting
team's before/after boundary. Their evidence had been pointing at a real event the
whole time.

Two errors, worth separating:

**One success does not refute an outage — it refutes total unavailability**, which
nobody had claimed. A degraded service serves some requests. I let a single
success argue down seven consecutive failures across two teams: the weaker sample
overruling the stronger, dressed up as a counterexample.

**I never checked the status page.** I had checked it that same morning for a
different false alarm and used it correctly. Hours later, with three teams
reporting, I skipped the cheapest instrument available because I had a hypothesis
I preferred. **A measurement I want to be true is the one I forget to check
against the obvious source.**

The distinction still matters when it applies — an outage means wait, contention
means retry, a per-repository limit means check quotas — but establishing *which*
starts with the provider's own status, not with my sample of one.

### Stating the caveat is not the same as letting it change the conclusion

The team that filed the original report made the mirror-image error, and named it
better than I could have.

Their evidence was a pattern across four repositories, and they had **explicitly
flagged one of the four as inadmissible** — its last run predated the event. Then
they drew the conclusion from the pattern anyway.

That is worse than not noticing, because **the caveat discharges the obligation
without doing the work.** Having said the honest thing, the reasoning feels
rigorous, and the conclusion passes through unchanged. The check for it, and it works at the moment of
writing rather than requiring discipline afterwards: **if removing the caveat
would not change the conclusion, the caveat is decoration.** Theirs survived
deletion untouched — the claim read identically with or without the sentence
admitting one data point was inadmissible.

### Reading a trend across different subjects

I then made the same error in miniature, inside the correction itself. Told that
one failure showed a single executed step and another showed zero, I wrote that
their retries had produced *a degradation curve rather than a flat failure.*

They checked and refuted it: **the two results were different runs on different
commits, not attempts of one run.** Within the run actually retried, all three
attempts were identical — zero steps every time. The only thing that grew was how
long each waited before being cancelled, which is queue backlog, not progressive
degradation.

That is precisely the original error one level down: **taking measurements from
different subjects and reading a trend across them.** Their first mistake was a
pattern across four repositories; mine was a curve across two commits. In both
cases the numbers were real, the ordering was real, and the subjects were not
comparable — which is invisible unless you ask what each measurement was *of*
before asking what they show together.

Both of us skipped the same one-request instrument, from opposite directions — I
had a hypothesis I preferred, they had an investigation they were enjoying. They
had run a fleet-wide repository sweep, a configuration diff across the boundary, a
four-way comparison and per-attempt job data: vastly more work than the request
that would have ended it. **The cheap instrument is not skipped because it is
expensive. It is skipped because it would end the investigation**, and by then the
investigation has become the thing you are doing.

The operational form is blunter: **run the check that could end the investigation
first, precisely because it might.** Once an investigation has momentum, the cheap
check reads as an interruption to it rather than the point of it.

### Two failures that render identically

My own run list looked like a matching outage — five cancelled, one failed, one
green. It was not. The step count separates them:

- **zero steps** — the job never acquired a runner. The real symptom.
- **many steps** — the job ran and was killed when a newer commit superseded it.
  Self-inflicted, and invisible in the interface.

Both display as "cancelled". I had been pushing commits in quick succession all
afternoon, which is exactly what triggers the second, and I nearly read my own
normal behaviour as corroboration of someone else's outage.

So the check before concluding infrastructure failure is: **does the failing job
show zero executed steps?** If yes, it never started. If no, the annotation is
describing something other than what stopped it.

### A control from before the event

The strongest self-correction came from the team that reported it. Their evidence
included "the repository on the other runner type is green" — and they flagged it
themselves as **not a control, because its last run predated the event by a week.**

A result from before an event cannot testify about the event. Had they leaned on
it, they would have had a false control supporting a conclusion that was itself
unproven — two errors pointing the same way, which is the configuration hardest to
notice.

## An absence asserted from an unfinished search

Asked whether any consumer of a wire format had been missed, I ran a fleet-wide
search, then answered **"nothing else reads this"** and named the count. The
search had not returned when I said it. When it did, there was another consumer.

It was harmless — display-only, unaffected by the change — but the answer was
wrong, and it was wrong in the direction that ends an inquiry rather than
extending one. The person who asked had specifically requested to hear about any
additional consumer *now* rather than later.

The mechanism is worth separating from carelessness: a long-running search that
has not finished looks exactly like one that finished empty. Nothing distinguishes
"still working" from "found nothing" at a glance, and the conclusion arrives with
the confidence of a completed run.

This was committed an hour after writing down the rule it violates. Knowing the
class does not protect against it; only checking that the instrument returned
does.

**Before quoting a search as evidence of absence, confirm it completed.** And
prefer reporting the search's own output over your summary of it — the summary is
where "still running" quietly becomes "none".

## A field name that is real somewhere else

An accessor read an error code from a frame's header. The code lives in the
**body**. But `header["code"]` is a genuine, populated key on a *different* frame
type in the same family — so the lookup was plausible, compiled, and returned nil
for every error frame instead of failing.

A missing key returns "absent", which is indistinguishable from "present and
empty". Had the name been invented outright someone would have noticed; being
real one frame type over is what made it survive review.

The damage went past display. The nil fed a classifier whose first branch reads
`guard kind == "error", let code = errorCode else { return notSettled }` — so
every remote refusal was silently classified as *unsettleable* rather than as a
refusal. **A bug reported as a wrong string was also mis-settling durable
state.**

The fix splits the accessors by frame family rather than repointing one, so
neither field is addressed by literal outside the type. This is the second
instance in the same file: an earlier reader spelled a terminal kind `kind` where
the wire writes `k`, with the same quiet default.

### Four quiet defaults, stacked

Four separate layers each turned "I do not know" into a confident value: an error
type collapsing *refused* into *disconnected*; this accessor's nil; a classifier
inheriting that nil as *not settled*; and an audit column storing an outcome
class while discarding the text. **Not one failed loudly**, which is exactly why
they could stack four deep and make hundreds of failures unreadable end to end.

Each was survivable alone. Survivable is the property that let them accumulate.

## One error value carrying two meanings

A transport client threw the same failure value in two unrelated situations: when
the remote **answered with an error**, and when the session **went away without
answering**. One line converted the first into the second, discarding the
remote's own error code.

The consequence is not cosmetic. A remote refusal and a dropped connection call
for opposite responses — retry the second, do not retry the first — and the caller
can no longer tell them apart. So every refusal displayed as a network problem, a
user-visible lie, while the reason that would have said what to do about it was
thrown away at the conversion.

It survived for weeks because **both stories were individually plausible**. The
server's audit recorded "the module returned an error"; the client displayed
"lost the connection". Either alone reads as an ordinary failure.

### The disagreement is the finding

What exposed it was noticing that **those two accounts cannot both describe the
same event.** Not the volume — 546 failures produced no signal, because a bad
network week explains 546 disconnects perfectly well. The contradiction is what
proved something between the two sources was rewriting the event.

So: **when two sources describe one event in incompatible terms, that is
evidence, not noise.** Neither is necessarily wrong; the transformation between
them is where to look.

The ruling-out mattered equally: neighbouring calls on the *same session, in the
same poll cycle*, succeeded. That proved the session was healthy and only the
failing call was being converted — which is what moved the search from the network
to the error path.

One layer out, the same defect appears in the audit itself: it stores the outcome
**class** and discards the remote's text. Both convert a specific answer into a
generic category, and a category cannot be diagnosed. Worth noting the audit was
still the more truthful of the two sources — had it recorded a generic "failed",
it would have agreed with the client's story and the defect would still be live.

## Sibling arms that disagree on how they match

A lookup mapped credential identifiers to providers through six arms. Four
matched by prefix; two matched exactly. The store issues a family's first
credential under a bare identifier and every additional one under a suffixed
variant — so the two exact-match arms accepted **only the first account**, and any
second account under those two providers silently fell through.

Neither exact-match arm is wrong read alone. **They are wrong beside their
neighbours** — written by different hands months apart, each locally reasonable.
A diff shows one arm, so code review structurally cannot see this; it is only
visible by reading the arms *as a set* and asking whether they agree on their
matching discipline.

The failure was the usual absence: a warning on standard error, the provider
still serving its first account, health reporting fine, the accounting identity
balancing, nothing on the wire. **Capacity nobody mentioned is indistinguishable
from capacity never bought.**

Two things the fix got right:

**The test enumerates the families and asserts the property once**, rather than
adding a case per provider. A per-provider suite passes forever while the seventh
arm reintroduces the defect.

**The mutations included the wrong-fix direction** — widening an arm so one
vendor's identifiers also match another's, which would publish one pool as
another's capacity. Most mutation testing aims at the original bug; the more
valuable aim is **the plausible mistake a future fixer would make**, because that
is the one a green suite blesses.

## When a test is the last witness

A connection defect produced a sixty-second delay, and the client change that
removed the *trigger* shipped before the server fix. So the loud symptom is gone:
nobody will ever observe that delay again on a healthy setup.

Which means **the tests are the only evidence of that defect that will ever
exist.** A test passing against both the broken and the fixed implementation
would be indistinguishable from real coverage, and would quietly retire a finding
that took three people and an external observation to establish.

So the author required the tests written and shown **red against the pre-fix
code** before the fix was written, then re-ran that themselves rather than
accepting the report. The tests measure the bug, not the fix.

The paired control matters as much: a positive case driving the expensive path to
completion with no competitor, asserting it is genuinely used. Without it, an
implementation that always prefers the cheap path satisfies every new test while
breaking the feature entirely.

General form: **when a fix removes the conditions that made a defect observable,
the test becomes the last witness — and a witness that cannot fail is not a
witness.**

## A wall-clock bound is a property of the run

A test asserted an expired deadline returned within a second. It failed exactly
once — during a restart window, on a loaded machine — which is precisely when a
false alarm is most expensive and most likely to be attributed to the change
under way.

A duration bound measures the machine, not the code. The rewrite proves the same
property **by contrast**: a call that ignored its deadline would return the same
result as a generous call, so an error from one and a result from the other can
only mean the deadline was applied. No timing anywhere in the assertion.

It also **stops rather than passing** on a machine where no contrast exists — the
difference between a test that reports "cannot evaluate" and one that reports
success because nothing happened.

The general shape: **prove a mechanism by what it makes impossible, not by how
long it took.**

## Verifying the artifact, never the destination

A binary was placed for a test rig. Every check passed: the digest matched the
value its author published, the signing identifier was correctly pinned, the
build identifier matched, staged and placed were byte-identical, and the copy
used the safe rename. **It went to the wrong directory.**

Two directories, one filename — the production binary directory and the rig's own
— and a rig-named file in the production directory looks entirely reasonable at a
glance. Nothing loads it.

Every instrument that day had been built to make an artifact's *identity*
unforgeable, and **not one of them asks whether the consumer reads from that
path.** An artifact property cannot answer a question about a location.

The author caught it by verifying against the path **the consumer's configuration
names**, rather than the path in the message reporting the placement. When the
values disagreed they searched **by digest** rather than by name — a name search
returns seven plausible candidates across the tree, while a content search finds
the one file that actually moved.

Underneath it was a second floor: **two rigs existed**, with different
configurations and different module sets, and the only launcher entry pointed at
the older one. Both parties had read "the rig config" and read different files,
each truthfully. So the destination question has two parts — *which path*, and
*which consumer reads it* — and answering the first does not touch the second.

The generalisation: after confirming an artifact is right, confirm that the thing
meant to load it **names the place you put it**. Read that from the consumer's
configuration, not from your own record of where you wrote.

### The ambiguity was in the definite article

Two people disagreed about what a configuration said. Neither had misread — there
were **two** configurations, and each had truthfully answered the question they
asked of the file in front of them. The disagreement was not in the reading but
in the phrase *"the config"*.

So: **when two careful people disagree about a fact, check first whether they are
describing the same object.** The mistake will not be in either measurement.

Resolving which one was live is worth copying too. Modification times said
nothing useful — both had been touched. **Written evidence settled it**: one had
1,966 recorded requests and frozen evidence sets, the other had none and its
newest file was a database handle's leftover. *A service that has never recorded
a request has never served one.*

And the contradicting evidence — a launcher entry pointing at the dead one — was
explained rather than dismissed: it was **created and disabled before the live one
existed**. A fossil frozen at the moment it was switched off. **Contradicting
evidence older than the thing it contradicts is not contradicting evidence**, and
saying so is different from waving it away.

## A missing input rendered as a verdict

During a data migration, a verification tool read a baseline file that no longer
existed, got empty strings for the expected counts, and printed **shortfall on
all three fields** — reporting that the migration had lost data. It had not; the
file had been written to a temporary directory that a reboot cleared.

Every other instance of this family renders absence as **success**: a restart
that changed nothing passing its check, grants for a missing subject dropped
without a word, a registry emptied by a rename. This one inverts it, and in an
operational window it is arguably worse: **a false alarm during a migration
invites a rollback.** The correct outcome would have been undone on the strength
of a file that was merely absent.

The rule runs in both directions: **if one side of a comparison is missing, the
result is "cannot compare" — never a verdict.** Not passed, not failed. A
comparison with one side absent has no verdict to give, and inventing either one
is a claim about data nobody has.

The secondary lesson is where the artifact lived. The baseline's entire purpose
is to survive the operation, and it was stored somewhere the operation could
destroy. **Evidence about a change must be kept somewhere the change cannot
reach** — the same reasoning that makes a build-time hash better than one taken
after signing.

## An identifier used as an authority, not a route

A module was renamed and every consumer swept: routing config, module ids,
monitoring, client targets, documentation. A phone lost **all** access to the
fleet anyway.

The missed file was a federation profile granting a paired device a list of
operations, each naming the module it may call. That is the one place the module
id is an **authority reference** rather than a routing one — so nothing that
resolves routes ever reads it, and it survives every sweep driven by "what breaks
if this name is wrong".

The symptom is what makes it dangerous. **The transport connected fine and every
surface came back empty.** The device could not distinguish "you have no access"
from "there is nothing here", and neither could the server: the grants named a
module that no longer existed, so nothing resolved, and nothing objected.

Confirmed at the log: **the daemon drops grants naming a missing module
silently** — no warning across the whole window, verified against a control
showing the search does find warnings. So a rename anywhere in a fleet can revoke
a device's entire access with no error at either end.

Two rules:

**When renaming an identifier, enumerate where it is an authority reference
separately from where it is a routing reference.** Grant lists, allowlists,
policy files, capability manifests. A routing reference fails loudly on the next
call; an authority reference fails as absence.

The same rename produced a second casualty of the same family: a peer registry
keyed by **owner directory** emptied itself when the directory moved. Directory-
keyed and identifier-keyed state are both invisible to a sweep that asks "what
calls this", because nothing calls them — they are looked up *by* the thing that
changed. Worth enumerating explicitly before any rename: what is keyed on this
name, and what is keyed on this path.

**A grant naming an unknown subject should say so.** Fail-loud is arguable —
silence is not, because the failure surfaces as a working connection with no
content, which reads as "nothing to show" rather than "you were denied".

## A check whose passing state is also its null state

During a deployment I restarted a module and verified it: the running process
was executing the file at its deploy path. True, and worthless — **I had never
copied the new binary into place**, so the deploy path still held the old one.
The process and the path agreed because nothing had happened.

That check cannot distinguish "staged and restarted correctly" from "never
staged, restarted the same binary", because **both leave the system internally
consistent**. It compares the system to itself, and doing nothing is the easiest
way to be self-consistent.

The rule: **a check whose passing state is also its null state cannot detect the
null.** Coherence between two parts of a running system is not evidence that a
change reached it. At least one acceptance step must compare against a value
that came from **outside** — a hash the author stated, a count taken before, a
reference artifact rebuilt independently.

What caught it was the owner sending **both the old and the new hash**. The new
one alone would have shown a mismatch and left me guessing; the old one named
exactly which wrong state I was in. Publishing the value you expect to *not* see
is what makes a comparison falsifiable rather than merely hopeful.

The same shape appears wherever an instrument's failure and its success look
alike: a text search returning zero for a control that has shipped for months, a
generated test suite that silently produces fewer cases, a gauge reading zero
because nothing wrote it. Ask what the instrument would report if the operation
had not happened at all — if that is also the passing reading, the instrument is
decorative.

## Uncalled code read as documentation

A transport carries a dial orchestrator with no production callers — fully
tested, never wired. Easy to file as a loose end. It is worse than that, because
the orchestrator is **ahead of the path that actually runs**: it orders three
kinds of route, while the type at the live boundary can only represent two.

So reading it tells you what the transport was *designed* to do, and the wired
type tells you what it *can* do, and those had silently diverged. I read the
orchestrator, saw a route it supports, and told another team to use something
the boundary type has no case for — a comment in that very file says so plainly.
The uncalled code was more specific than any document, so it was read as the
authority.

A second artifact in the same file has the same property: a per-rung timeout that
looks like a guarantee and bounds nothing, because nothing runs the rungs. Two in
one file is not coincidence — **it is what uncalled code decays into**, since
nothing forces it to track the path that ships.

The rule: **dead code is not inert when it is also the most detailed description
of intended behaviour.** Wire it or delete it. Leaving it is a standing source of
wrong conclusions, and the conclusion drawn here was mine.

## A name narrower than it sounds reads as a defect

A backup store showed `entry_count = 2` on every generation, including ones that
demonstrably moved 2219 objects and 1.3 GB. Alongside it, `staged_bytes = 0`
everywhere. Read cold, that is a store recording nothing.

Both are honest. `entry_count` counts **catalog entries** — this account captures
exactly two databases — and the thousands of objects are the chunks those two
decompose into. `staged_bytes` stopped being written when staging changed to hold
only what deduplication missed.

Neither name is wrong to the person who chose it, and both are actively
misleading to everyone else. The cost is not confusion in the moment — that gets
resolved with one question. **The cost is the suspicion it leaves**: a reader who
moves on without asking now believes the store is unreliable, and carries that
into the next incident, where it competes with real evidence.

So: when someone reads a column as broken, the useful reply is not just the
correct interpretation but **why the name misleads**, which is what stops it
being rediscovered. Better still, put it in the schema comment — the reader who
needs it may never ask.

A counter is worth extra suspicion once it **stops being written**. `staged_bytes
= 0` is indistinguishable from "nothing staged", and a column nobody maintains is
better dropped than left reading zero.

## An await inside a select arm stops the other arms

A connection took exactly sixty seconds, three times, and the local network path
it eventually used had been **established one second in**.

The peer loop waits on several things at once: an outbound dial, an inbound
connection, control commands. A control command asked it to open a relayed
connection, and the handler awaited that dial **inside the arm** — so the loop had
left the multi-way wait, and the inbound-connection arm was no longer being
polled. The local connection completed its handshake, queued, and sat there. The
relay wait was bounded by a grant deadline rather than a short timeout, so the
loop returned sixty seconds later, drained the queued connection, and proceeded.

The result reads as "the local path won, slowly", which is why it survived: the
fast path *did* win. It simply could not be served while nothing was looking at
it.

**The API shape invited the mistake.** The client called something that looked
like a constructor — it took values and returned a candidate — and it committed a
remote machine to a sixty-second wait. Nothing at the call site suggested a
remote effect. **A call that obligates another machine should not be
indistinguishable from building a value**, and the durable fix is at the seam:
make side-effecting and pure options different types, so a caller cannot treat
them alike and a racing caller cannot include the wrong one. Documenting the
ordering rule is the weaker version of the same fix.

That also bounds where parallelism is safe. **Racing a side-effecting option is
worse than trying it serially**, because a race *guarantees* the side effect
happens even in the case where it turned out unnecessary. Race only what costs
nothing on failure, anywhere.

Two generalisations worth more than the bug:

**An already-established cheap outcome should pre-empt a speculative expensive
one.** Here the cheapest possible case — the peer is already connected — was
served last. Any loop that races alternatives has to keep racing them for the
whole attempt, not just until one of them starts.

**Setting up an expensive path is not free for the other side.** The client
assembled its full candidate list up front, which minted a grant, which told the
peer to go and wait somewhere. It then connected locally in 180ms and never used
the grant. Preparing an option had a side effect on someone else — so the option
should be prepared only once the cheaper ones have failed.

The diagnosis came from correlating two independent vantage points: the client's
own timings, and a watcher on the *listener* showing when the connection actually
established. Neither alone distinguishes "slow to connect" from "connected and
unattended".

## A correct detector firing on an ambiguous event

A reaper kills background work once its project root is confirmed absent, after a
two-sweep check. The mechanism is right and the check is careful. But **a rename
makes the old path absent while the work keeps running fine** through its open
directory handle at the new location — so the reaper correctly detects absence and
kills healthy work.

This is a third category alongside the two usually considered. Not a **stale**
read, where old data is served as current. Not a **missing** read, which is
visible and self-correcting. This is a **correct detection of a state whose
causes it cannot distinguish**: deleted and moved look identical to anything that
only checks presence.

The question to ask of any detector: does it distinguish the *causes* of the
state it detects, or only the state? If only the state, then every cause sharing
that state inherits the consequence — and the consequence was designed for the
worst cause.

The options are to disambiguate (watch for the move rather than the absence),
soften the consequence, or **accept it and record the constraint beside the
mechanism**. The third is legitimate when disambiguation is expensive, but only
if written where the next reader arrives: at the detection site, saying which
benign cause is being sacrificed and what to avoid doing as a result. Otherwise
it is indistinguishable from an oversight, and the next person to hit it
reasonably files it as a bug.

## A filter that matches nothing, and what happens to the rest

Two people wrote the same check within an hour — compare the configured module
set against the running one — and both filters failed to match. The outcomes were
opposite, and the difference was luck.

The first counted configured modules with a pattern that matched none of them and
reported **zero**, against fourteen actually running. The second filtered
command output for a header token that did not exist, so the header row itself
survived into the data and the check reported **a module named `id`** that does
not exist.

So a filter that fails open does not merely lose rows: **the input's own
furniture — headers, separators, framing — becomes records.** Losing rows tends
toward a false clean; inventing them tends toward a false alarm. Which one you
get is decided by what the framing happens to say, not by anything you control.

The second case was safe by accident. A phantom named `id` is loud enough to
investigate. Had the header token been one the filter *did* match, the identical
defect would have silently dropped a real module and reported agreement.

The fix is to stop hoping the filter still matches: **assert the shape before
parsing it.** Requiring the header to be a specific literal converts a format
change from "produces wrong data" into "fails at the assertion". That is the
difference between a parser and a guess.

A companion worth copying from the same exchange: the procedure runs that check
twice — once expecting agreement, and again after the config edit **expecting a
specific disagreement**. One instrument, two invocations, opposite expected
verdicts. A check that can only ever return agreement is caught by construction,
because the second run demands it say something else.

## Breaking an invariant nothing reads yet

A recovery branch had to skip a sequence number it could not identify — that
unknowability was the whole reason it existed. A side effect: each record links
to its predecessor, and skipping leaves that link chain with a hole.

Nothing reads the link. It is written, signed over, and decoded, and no verifier
walks it, so the hole is unobservable today by construction. No test fails, no
alarm fires, nothing anywhere objects.

That is a distinct hazard from the more familiar one. The usual version is **a
field asserting a property nothing checks** — a writer can record something false
and no reader catches it. This is the mirror: **a writer maintains an invariant
no reader uses**, so a change can silently stop maintaining it and the cost lands
entirely on whoever writes the first reader. They will reasonably assume the
chain is intact, because every line of code that maintains it says so.

Both shapes come from the same root — a promise in the data with no consumer to
keep it honest — and neither is detectable by running anything.

The two shapes fail differently, and that decides where the warning goes. With a
field nothing checks, the danger is that **nobody looks**. With an invariant
nothing reads, the danger is that **looking reassures you** — every line
maintaining it is evidence it holds, and the one place that stopped maintaining
it is precisely where a reader will not think to check, because it is the place
where maintaining it became impossible and therefore reads as an ordinary error
branch.

So: the first shape can live in a design document, whose audience is whoever
defines the field. **The second must live at the site**, because its audience is
whoever writes the first reader, and that person arrives through the code rather
than through the plan. A document is addressed to someone already asking the
question; a comment is addressed to someone who does not know there is one.

When a change breaks an unread invariant the choice is: restore it, delete the
field, or record the constraint where it will be found. Here the constraint was
that a future walk must either tolerate the hole or be built only after the
upstream gap forcing the skip is closed.

Worth noticing what the accumulation was telling us. This was the third
independent symptom of one missing capability — a refusal that withheld a value
the server had already computed, an object orphaned by guessing forward, and now
a chain that only stays intact if clients never have to guess. **Three symptoms
with one cause is a much stronger argument for the fix than any of them alone**,
and none of the three was individually large enough to justify it: the first had
a local workaround, the second cost a few hundred bytes, the third is
unobservable today.

That generalises into a review habit. **When a proposal keeps getting deferred,
check whether its motivations are being priced one at a time.** Each deferral is
individually correct, the aggregate is never evaluated, and no single symptom
ever forces the conversation — which is how a missing capability survives
repeated encounters with people who all did the arithmetic right.

## Verifying properties of the wrong target

A phone could not reach a service on its own network. Two people spent hours on
it and both produced the same class of evidence: the listener is bound to all
interfaces, the host firewall is off, the address holds a live lease, the
routable address accepts connections, both devices ping each other. Every fact
true, and **every fact measured against a different address than the one the
phone was dialing** — the service had moved, and another device had taken the old
address.

Neither of us ever asked what the client was actually targeting. We verified
properties *of the target we assumed*, and a property of the wrong subject cannot
contradict anything.

What broke it open was a diagnostic that **echoed its input**: the client's probe
reported the address it dialed, not merely the outcome. So: a check whose output
omits what it was pointed at cannot expose a wrong target, however thorough it is
about everything else. Print the subject, not just the verdict.

The second tell was one both of us dismissed. Two probes seconds apart returned
*connection refused* and then *no answer*. One host does not usually do both —
but two different devices answering at different moments do. **An inconsistency
that looks like flakiness is sometimes two different answerers**, and the natural
reading is noise, which is what makes it a good signal.

There is a decision lesson too. Before the cause was known, the obvious
improvement was to shorten the timeout the dead path was burning. That would have
made a misconfigured client fail *faster* — the symptom shrinks while the cause
survives, and a smaller symptom is harder to notice. **An optimisation applied
before the cause is known can bury the bug.**

Underneath it: the address was hand-entered once and had no expiry. A fact
someone typed is exactly as fresh as the moment they typed it, and nothing tracks
that. The durable answer is resolving the peer by identity rather than by a
remembered address — which deletes the class instead of detecting it sooner.

## A test double is a set of independent permissions

One simulated cloud service, three separate permissive rules, all found in a
single day and each one certifying a different broken client:

1. It accepted **any** sequence-to-object binding — which certified a retry loop
   that could never converge, because the real service compares the two and
   refuses a mismatch.
2. Its object store **overwrote unconditionally** — which certified a client that
   re-encrypted its payload on retry, because the real store is write-once and
   content-bound and rejects different bytes under a bound identifier.
3. Its error bodies were **shaped differently** from production's — harmless
   against the current substring matcher, and it would certify any future
   matcher that parses the body as structured data.

The sequence is the lesson. Each was independently permissive, and **fixing the
first two taught the author nothing about the third.** Modelling one rule of a
double does not reveal the others; the third surfaced only from deliberately
enumerating what else the double still allowed.

So the rule is not "tighten the double when a bug escapes it" but: **having found
one permissive rule, enumerate the rest.** The count is finite and the list is
writable. What is not discoverable is which of the remaining ones your next
change will depend on.

The general form: **a double must not make a client look correct for a reason
production would not grant.** Every permission it extends beyond the real service
is a green test waiting for a client that leans on it.

Worth noticing that the second defect was caught by *reading the production
service* rather than by any test, and the third by enumeration rather than by a
failure. Neither had a failing test to lead the way, because the double was the
thing preventing one.

Record what remains permissive rather than quietly fixing everything mid-flight.
A written list of known-permissive rules is a map of where the next false green
will come from.

## A control run where the fault cannot occur

An owner wrote an acceptance check for "is the running process executing the
file at its deploy path", and ran three controls against it before use. All
three passed. The check was broken: it took the path that `lsof` reports for
the running program and re-examined that path on disk — comparing the deployed
file against **itself**, which passes unconditionally.

The controls could not have caught it. They ran before anything was staged, when
the running and deployed files were legitimately the same, **so the check passed
for the right reason**. Nothing looked wrong because nothing was wrong yet.

The discriminating state — a binary replaced on disk while the old one keeps
running — did not exist until someone staged a deploy. It surfaced within minutes
of that, when a second tool reported a mismatch about the same process at the
same moment.

So: **a control run in a world where the failure cannot occur proves the check
executes, not that it can detect.** For anything that guards a transition, the
control has to be run *in* the transitional state, which often means during the
operation rather than before it.

The detail that made it undetectable by reading: the path is the **same string**
in the healthy and broken cases. A replaced binary keeps its path while the
running process holds the old file; only the identifier the kernel reports for
the open file distinguishes them. Code that looks correct, uses the right tool,
and compares the right kind of value can still compare one thing to itself.

Auditing my own tooling for the same bug found something worse — the check was
absent entirely, so a staged-but-not-restarted binary read as fully deployed.
Writing it revealed a module that had been serving an eleven-day-old build since
its binary was replaced that morning, invisible to every path-derived instrument
including a version probe.

## A safety check answered by the tool you then repaired

I removed two abandoned working directories after checking they held no
uncommitted work. The check said zero. It said zero *because the tool could not
read them* — their internal pointer referenced a parent directory renamed a
month earlier, so every query returned nothing.

I then fixed the pointer, precisely so the tool could manage them. And I carried
the earlier reading across that repair. Re-running the same query after the fix
reported two modified files in one and one in the other. I had already passed
them to a forced delete.

What was lost was small — uncommitted edits in a directory nothing had touched in
a month, with all committed history intact on a branch that still exists. That is
luck, not diligence.

The shape is general and it is not about git. **A broken instrument reports
"nothing here"; a working one reports what is there.** Those are the same output.
So any safety reading taken before a repair was answered by the broken version,
and the repair is exactly the event that should invalidate it.

Worse, the repair *feels* like progress toward safety — the tool now works, so
surely things are better understood than before. That is what makes the stale
reading easy to keep.

The rule: **after fixing an instrument, re-run every check you made with it.**
And when a check returns an all-clear that would also be returned by a failure to
look, treat the all-clear as unmeasured until something proves the instrument
could have said otherwise — the positive control, applied to a destructive step
rather than to a search.

## An unverified premise that closes a direction

Two people made the same error within hours: each asserted a property of code
they had not read, as the *premise* for an argument rather than as its claim.
The first proposed a design on it. The second used one to kill that design, and
both were wrong.

The two are not equally expensive, and the difference is worth naming because it
inverts the usual intuition about which mistake to fear.

A wrong premise that OPENS a direction gets tested. Somebody builds the thing,
and the build meets the reality the premise misdescribed. The error surfaces,
late and annoyingly, but it surfaces.

A wrong premise that CLOSES a direction is never tested by anyone, because
nothing downstream of it is ever built. There is no artifact to trip over, no
failing test, no incident. The decision simply stands, and the reason it stands
is a sentence nobody re-reads. **The false negative is the expensive one.**

Worse, the closing argument tends to be stated with more force than the opening
one, because refusing feels like the conservative act. "That would create a
permanent oracle for an unauthenticated caller" ends a conversation. What the
source actually said, once read, was that freshness and replay are checked before
dispatch, so the exposure was bounded to a five-minute window rather than
unbounded — a real cost, but one worth a different answer.

So: when a premise is about to close something, hold it to the standard you would
hold a claim. Read the function. And when the correction comes, **replace the
reasoning at the site**, not just the conclusion, or the next person inherits the
belief rather than the reading.

The vacuity guard from the same episode is worth copying. The corrected behaviour
was pinned with tests proving replay is refused and expiry is refused — and a
third proving that a fresh request with a wrong value genuinely REACHES the check
at all. Without the third, the first two are satisfied by a world where nothing
ever reaches that check: identical greens, zero information.

## A guard that checks a proxy for the thing it protects

A data-migration script refused to run while the daemon was up. Under the window
it was written for that was exactly right: the daemon was stopped first, so
daemon-down implied the modules writing those stores were down too. The guard
read the daemon because the daemon was easy to read.

Then the window changed shape. A capability landed that lets modules be retired
and respawned without stopping the daemon, so the new procedure deliberately
leaves it running. The same guard now refuses the safe case.

That much is merely annoying. The dangerous half is the inverse: the script's
process check named the binaries by their *post-rename* names, and under the old
window a full daemon stop covered that gap. Under the new one it would find no
matching process, conclude the coast was clear, and move stores **while the old
modules were still writing them**.

So one guard, unchanged, became wrong in both directions at once — refusing what
is safe and permitting what is not — because the condition it checked stopped
standing for the condition it protected. Nothing in the script changed. The world
around it did, and a proxy has no way to notice.

The repair is to name the real precondition: refuse while the *writers of the
stores being moved* exist, and allow the daemon to be up. That guard cannot drift
with the procedure, because it is about the thing at risk rather than about a
circumstance that used to accompany it.

Proxies are not avoidable — most guards check something observable that stands in
for something abstract. What is avoidable is leaving the substitution unrecorded.
Write down what the check stands for, at the check, so the next person changing
the procedure can see whether the substitution still holds.

Test it in **both** directions. A guard exercised only where it refuses cannot
distinguish correctly-strict from refuses-everything, and this one had by then
been wrong in each direction once.

## A surviving mutant has two readings

The usual reading of a mutation that reddens nothing is that the suite cannot
see the behaviour. There is a second, and it is worse.

A store method was mutated by calling its clear function with `u64::MAX`, meaning
"settle everything". Nothing was deleted, the test passed, and the honest
conclusion recorded was "position-based settlement is unproven". The truth was
that the mutation never ran: SQLite integers are signed, `u64::MAX as i64` is
`-1`, and `seq <= -1` matches no row. **A real bug in the store method was hiding
behind its own failed mutation proof** — a delete that deletes nothing while
returning success.

So when a mutant survives, two questions and not one: can the suite see this
behaviour, and *did the mutation take effect at all*. The second reading is
strictly more alarming. A blind spot in a suite is a gap; a mutation that
silently no-ops is a working defect that has just demonstrated itself and been
filed as absence of evidence.

The cheap guard is to make the mutant prove it ran — assert the mutated path
produced its intended effect before drawing any conclusion from the suite's
silence. In this case: check the delete removed rows.

The sign mismatch is the *carrier*, not the property. `u64::MAX` says "all" in
Rust and arrives as `-1` in SQLite, which says "none" — so any unsigned sentinel
crossing into a signed column can do this. But the module owner swept their
remaining casts and found the real discriminator: **a caller-supplied bound with
neither input validation nor a result check.** A merely *wrong* bound would have
been equally silent; the sign trap only supplied the wrongness.

The control proves it. A sibling range query sits at identical numeric risk and
is safe, because it validates its input (refusing a zero or inverted range) *and*
verifies its output, walking the returned rows demanding contiguity — so a
wrapped bound produces an empty result the completeness check catches. Either
mechanism suffices; the defective one had neither. Sixteen casts narrowed to four
bounds narrowed to one defect, and it is the safe sibling at equal risk that
turns this from a description of one bug into a test you can apply.

So the usable form: **a delete or range query that asserts nothing about what it
touched cannot distinguish success from a no-op.** That covers the wrapped bound,
the wrong bound, the empty table, and the mutation that never ran.

Companion rule already in this file: when a mutation *does* redden something,
read which test died. Survival can mean the mutation missed; death can mean it
hit something other than the subject.

## A mechanism that is specified only in its triggering state

Two findings landed a round apart, in the same design, from the same blind spot.
Both were mine to have caught, and I passed both.

The first: a latch that fires when a producer's authority counter regresses. It
refuses new admissions and lets established sessions age out on their last known
good policy. I checked what it prevents and what it deliberately permits, and
approved it. What nobody asked was WHAT LEGITIMATE OPERATIONS IT BLOCKS — and a
latched org cannot install new policy, *including policy that reduces access*. A
security-tightening action, blocked by a security mechanism. The fail-closed
state prevents further closing.

The second: the probe that decides whether a stale-looking artifact is delivery
reordering or a genuine regression. Correct, and specified entirely for the
period BEFORE it answers. After the latch is confirmed the probe has nothing left
to distinguish, so every subsequent stale artifact starts another one — work with
no possible outcome, aimed at a system already known to be in trouble.

The common shape: **a mechanism defined by its behaviour in the state that
triggers it, silent about its behaviour in the state it creates.** Review
naturally follows the trigger, because that is where the reasoning lives and
where the tests are. The post-state has no such gravity: nothing in the design
document points at it, and by the time anyone is in it they are debugging
something else.

So the question to ask of any mechanism with a durable effect: after this fires,
what does it do next time, what does it now block, and how does it end. A latch
needs its release path; a probe needs its stopping condition; a fence needs the
list of operations it must still permit.

A useful sibling from the same round, on how the terminal case is encoded: an
absent value meaning "never" is indistinguishable from "not yet set", and on a
security state those have opposite consequences — one holds a fence forever, the
other drops it the first time something reads the field expecting a value. Branch
on an explicit severity or state marker and let the absent value be *required* to
accompany it, so a mismatch between the two is detectable corruption rather than
a silent default.

## A gap that never closed leaves no diff

A cross-language conformance suite read its fixtures from a directory the other
language owns. It carried the non-vacuity guard you would want: list the
directory, assert the list is non-empty, assert a known member is present. That
closes the case where a bad path silently resolves to nothing, and it is the
reason the suite looked trustworthy.

It consumed seven of twenty fixtures.

The guard proves the directory is REACHABLE. Nothing in it speaks to coverage OF
the directory, and the two read alike at a glance. So a fixture lands on the
producing side, nothing on the consuming side reads it, and the suite stays green
forever.

This is worse than the erosion case it resembles. When coverage is LOST, a file
was deleted and that deletion appears in somebody's review; there is a
before-state to compare against. When coverage NEVER EXISTED, there is no event
at any point: no deletion, no failure, no moment at which anything changed.
Nothing looks wrong because nothing happened.

The instrument is a set difference — which of the producer's artifacts does
nothing of mine consume — and it must be **reported, not asserted**. Pinning an
expected fixture count would fail the consuming side for a legitimate addition on
the producing side, which is the typed-in-number class one section above; the
directory is observed, not owned. Naming the unconsumed members is a coverage
fact, and coverage facts belong where a person reads them rather than in a
pass/fail.

One caution when the check comes back clean, because a null from a
just-written instrument is the weakest reading available: plant an unreferenced
artifact, confirm the check names it, remove it. A clean set difference is worth
something only after the instrument has been seen to find one.

## A check that compares a measurement to a typed-in number

A precondition in a migration runbook read "expect 1" and "expect 3" against two
probes of a deployed binary. Both numbers were correct when written and both are
properties of how one build happened to be laid out — not of the binary carrying
the feature the check exists to confirm. A later release that changes either
count fails the precondition during the window.

That timing is what makes it worse than an ordinary stale value. The natural
reading at that moment is "the deployment is wrong", not "the check was too
tight", because the runbook is the thing telling the operator what to suspect and
is therefore the last thing suspected. A check that can raise a false alarm under
maximum pressure does not merely fail to inform — it misinforms with authority.

The author had written that control deliberately, because a zero from a probe is
ambiguous without one. Then pinned the control to a number that could go stale by
itself. Presence was the proposition all along: one probe must find the feature,
the other must find anything at all, and the second is what makes a zero on the
first mean something.

The general form, checked against two independent codebases the same morning:
**the class bites where a human types the expected value.** Anything comparing
one live measurement against another is immune by construction — modules
reporting against modules configured, files changed against files in a diff.
Anything comparing a measurement against a remembered number is exposed. Prose
runbooks are almost entirely the second kind, which is where to look first.

Note what this does *not* say. Asserting an exact count is sometimes exactly
right: a data-driven suite that generates its cases from files on disk must
assert how many it generated, or a silently missing file removes cases with no
failure. The discriminator is whether the number describes something the check
OWNS — its own case count — or something it merely OBSERVED, which is free to
change without the check being wrong.

## A count without its breakdown invites the reader to supply an attribution

I reported "96 markdown files" from a branch and let the recipient infer which
subtree they came from. They inferred the one that fit the fix they were already
designing -- generated task outputs -- and sized the remedy to it.

The breakdown was 54 hand-authored prompt files, 26 generated transcripts, and 18
task outputs. So the proposed fix would have removed 18 of 99 while reading as
though it addressed the whole finding, AND it would have kept force-adding the
category that actually dominated.

MY NUMBER WAS TRUE AND MY REPORT WAS ONE FIELD SHORT OF USEFUL. A bare count is
an invitation: the reader needs a composition to act on it, and if you do not
supply one they will construct the most plausible one available to them -- which
is the one consistent with what they already believe.

SO WHEN REPORTING A COUNT THAT SOMEONE WILL SIZE A FIX TO, REPORT THE
PARTITION. `sort | uniq -c` over the grouping dimension costs one pipe.

AND VERIFY CLAIMS MADE ABOUT EVIDENCE YOU SUPPLIED. The recipient's attribution
was about MY specimen, which I could check and they could not. Where two seats
discuss one artifact, the one holding it owes the check -- an assertion about
someone else's evidence is the least-defended claim in any exchange.

## A quiet lane is unarmed, not stuck

Every defect corrected across one evening's cross-seat arc had been WRITTEN by a
seat working alone, and each was found only once another seat's finding put a new
instrument in the author's hands. Nobody in the chain was being more careful than
usual; each was holding a tool sharpened somewhere else an hour earlier.

SO THE SEATS ARE NOT CORRECTING EACH OTHER -- THE INSTRUMENTS ARE CIRCULATING AND
THE SEATS ARE THEIR CARRIERS. That is a more precise account of what a review
partnership does than "a second pair of eyes", and it predicts where the method
fails: A SEAT WORKING ALONE ON A NOVEL PROBLEM HAS NO CIRCULATING INSTRUMENT TO
HOLD AGAINST IT. It is exactly the condition under which the original defects
were written.

OPERATIONAL FORM: when a lane has been quiet for a long stretch, ROUTE AN
INSTRUMENT INTO IT DELIBERATELY -- a blind gate, a cross-seat census, a
report-only detector aimed at its artifacts -- rather than reading the quiet as
health. The quiet lane is not stuck; it is unarmed.

IT PAID ON THE FIRST APPLICATION, AND THE FAILURE IT CORRECTED WAS MINE. Four
hours before running the instrument, I had checked the same quiet seat by hand
and concluded idle-and-fine: clean tree, module healthy in `ck health`, recent
commit. All three TRUE, none of them the question -- A SUPERVISED MODULE'S HEALTH
SAYS NOTHING ABOUT WHETHER THE CODE THAT PRODUCES IT PASSES CI, because the
running binary is an older build. The instrument found a red tip on the seat's
last commit, a windows-only regression five hours unread, with the five preceding
commits green.

SO THE RULE HAS A SHARPER EDGE THAN "CHECK ON QUIET SEATS": I DID CHECK, WITH THE
CHECKS I HAPPENED TO HAVE. Reaching for available signals -- health, tree state,
commit recency -- is the rung-below substitution wearing a diligence costume. The
instrument matters more than the intent to look.

THIS COMPOSES WITH THE SINGLE-READER RULE. "Check whatever has exactly one
reader" targets ARTIFACTS; this targets SEATS, by the same logic -- the artifact
nobody else has read and the seat nobody else has audited are the same exposure
at two scales. Recency of cross-seat contact is a selection signal alongside the
usual suspects.

## Re-running a red: experiment or retry, and the difference is a prediction

Re-running a failed job usually converts an unexplained failure into an
UNEXAMINED PASS. It is the rigorous move only under a specific condition: YOU CAN
STATE, BEFORE RUNNING IT, WHAT EACH OUTCOME WILL MEAN.

LEGITIMATE CASE (SYNAPSE, 2026-07-26): a windows job failed at a `choco install
llvm` setup step, BEFORE any code from the commit was touched, and the diff was
structurally incapable of causing it (python + Swift + markdown under a bench
directory no Rust target builds). With a named suspect and an a-priori argument,
the re-run is a controlled experiment on one variable -- same sha, different CDN
draw. Green means environment; red means the structural argument is wrong and the
diff is back in scope. THE PREDICTION REGISTERED IN ADVANCE IS WHAT MAKES IT AN
EXPERIMENT RATHER THAN A HOPE.

THE WEAK BRANCH IS THE ONE THAT FEELS CONCLUSIVE: A SINGLE GREEN RE-RUN IS
CONSISTENT WITH A FLAKE AT ANY RATE ABOVE ZERO, INCLUDING 50%. It does not
establish "environmental", it fails to refute it. So the comment that follows must
name the flake CLASS and not assert a RATE nobody measured -- "this step has
failed on a CDN draw before" is defensible; "rare, transient" is a number nobody
took.

AND THE FIX RESTORES MEANING RATHER THAN REMOVING NOISE. Before a retry pin, a
red on that step is uninformative -- real break or bad draw, indistinguishable
without redoing the whole investigation. After, a red means the failure survived
N attempts. Same shape as logging a static refusal once instead of every sweep:
THE VALUE IS NOT REMOVING THE NOISE, IT IS RESTORING THE SIGNAL'S MEANING.

RECORD THE EVIDENCE, NOT A CHARACTERISATION. The comment that lands with the pin
should say "red on <sha>, green on same-sha rerun" rather than "rare, transient".
EVIDENCE STAYS TRUE; A RATE CLAIM ROTS THE FIRST TIME SOMEONE MEASURES IT, and
nobody measured this one.

THE LIMIT THE STORY WILL FORGET: A RETRY PIN BUYS ROBUSTNESS, NOT DIAGNOSIS. If
the underlying failures are a flake, N attempts is plenty. If they are a RISING
TREND -- an image change, a package deprecation, a tightening rate limit -- the
pin CONVERTS A VISIBLE RED INTO AN INVISIBLE SLOW-DOWN, and the first signal is
the day N attempts stop being enough. The pin's own success rate is a measurement
nobody is taking; if the step starts needing all N, that is data rather than
noise.

## "Pushed" is not a terminal state

Two seats made the same substitution within one evening, in different objects. I
read a module's HEALTH as evidence about its CODE. They read a LOCAL gate
(fmt/clippy/tests) as evidence about the REMOTE head, pushed, and stopped. Both
signals were true, adjacent, and about a different object -- and in both cases
THE CHECK WE RAN COULD NOT HAVE FAILED FOR THE REASON WE NEEDED IT TO.

They had banked the tip-was-queued-vs-tip-is-green rule TWO DAYS EARLIER and
forgotten it at exactly the moment it applied. That is not a memory failure: A
RULE WITH NO MECHANICAL TRIGGER DECAYS TO ZERO PRECISELY WHEN IT IS RELEVANT,
because relevance arrives at the end of a work session when attention is lowest.

SO THE DURABLE FIX IS A TRIGGER, NOT MORE DISCIPLINE: the terminal state of a
change is A CONCLUSION ON THAT SHA, not the push. Anything that ends a session at
"pushed" leaves a claim unverified -- and the claim is the one everyone downstream
will assume was checked.

AND THE TRIGGER MUST BE ARMED BY THE SAME ACT AS THE PUSH, NOT AFTERWARDS. The
seat that had just been bitten wired their next push's watch BEFORE sending the
message reporting it. That ordering is the entire rule: BEFORE means the trigger
exists independent of your attention; AFTER means you are relying on the same
attention that already failed once. A reminder you have to remember to set is the
defect wearing a solution's costume.

## A cleanup whose trigger dies with the activity it cleans up after

AFT's callgraph temp-file cleanup was PER-ROOT AND RAN ONLY WHEN A BUILD FIRED
FOR THAT ROOT. The active store kept building, so cleanup kept firing and it
stayed clean. THE LEGACY STORE STOPPED RECEIVING BUILDS AT A MIGRATION -- and
every orphan that existed at that instant became permanent. 15.4 GB, frozen at
the moment the activity moved elsewhere.

THE CONTRACT GAP, IN THEIR WORDS: the migration DID clean what it was told to;
its contract was per-root-on-build, and NOBODY OWNED "STORE STOPPED BEING
WRITTEN". Not negligence -- an unowned state transition.

GENERAL FORM: ANY CLEANUP TRIGGERED BY THE ACTIVITY IT CLEANS UP AFTER STOPS
EXACTLY WHEN ITS BACKLOG BECOMES PERMANENT. Garbage collection on write, cache
eviction on access, log rotation on emit, orphan reaping on spawn -- each is
fine while the subsystem is live and each freezes its debris the moment the
subsystem goes quiet. The failure is silent because the component looks idle
rather than broken.

THE TELL IS AN ASYMMETRY BETWEEN TWO INSTANCES OF THE SAME MECHANISM. One store
accumulated and its sibling did not, same code, same operation. THAT DIFFERENCE
WAS THE DIAGNOSIS -- reading the code it pointed at took minutes. Where two
deployments of one mechanism diverge, the divergence names the variable.

AND THE FIX'S PREDICATE MATTERS: age, not liveness (see below). A retirement
sweep keyed on "is the creating process alive" reads false-positive on precisely
the oldest files, which are the ones worth deleting.

## Process liveness is not ownership on a long enough timescale

A reaper that spares a resource because "the owning pid is still alive" is
trusting a number the OS recycles. Sweeping 88 orphaned temporary files whose
owning pid is embedded in the filename: 85 pids dead, 2 ALIVE -- and the live
ones were `PasswordBreachAgent` and a similar system daemon, on files last
written TWO WEEKS EARLIER.

SO A PID-LIVENESS REAPER WOULD HAVE SPARED THOSE TWO FOREVER, for a reason
unrelated to the code that created them. The predicate is not merely weak; it is
WRONG IN A DIRECTION THAT LOOKS LIKE CAUTION, so nobody investigates the
survivors.

CROSS PID-LIVENESS WITH MTIME, ALWAYS. A two-week-old file whose pid is alive is
telling you about the pid table, not about the file. Same family as a
self-matching process selector (`pgrep -f <name>` matching the probe's own
command line) -- both are identity checks on a namespace that is not stable
across time.

AND CHECK WHAT THE LIVE PID ACTUALLY IS. Counting it as live is the error;
resolving it to a name takes one command and turns "2 possibly-active writers"
into "2 instances of pid reuse".

## Durable state in an ephemeral directory is a near-miss waiting for a cleanup

Sweeping for unbounded growth, I found a 2.9 GB directory I did not recognise
inside the daemon's RUNTIME dir -- the one holding the connection file, the
start-lock and the log I was at that moment scoping ROTATION for. It turned out
to be another module's write-ahead log and durable index: every session
transcript in the fleet, every unexported metering fact, and THE ONE ARTIFACT
WITH NO UPSTREAM (the SQLite store rebuilds from the WAL; nothing rebuilds the
WAL).

THE NATURAL SHAPE OF THE WORK I WAS SCOPING -- "manage the contents of the
runtime directory" -- WOULD HAVE DESTROYED IT. What stopped me was not care: it
was that the sweep surfaced a directory I could not identify and I asked its
owner instead of assuming. THAT IS NOT A REPEATABLE SAFETY PROPERTY.

SOURCE OF THE HAZARD, in the owner's words: a BOOTSTRAP DEFAULT derived from the
connection file's parent, with a comment saying a managed descriptor was a later
refinement. Nobody decided it; it was never revisited. AND "COSMETIC
INCONSISTENCY" AND "ONE COMMAND FROM DESTROYING THE DURABILITY SUBSTRATE" ARE THE
SAME ITEM AT TWO PRICES -- the placement had been parked as low-priority
housekeeping for weeks.

TWO RULES:
· BEFORE WRITING ANY CLEANUP, ENUMERATE WHAT IS ACTUALLY IN THE DIRECTORY AND
  RESOLVE EVERY ENTRY TO AN OWNER. A directory's NAME is a claim about its
  contents (see: a name is a count without a breakdown) and "run" claims
  ephemerality it may not have.
· AN ACCEPTED HAZARD NEEDS ITS EXCLUSION AND ITS REASON AT THE LINE, not in a
  commit message. The next person to write a cleanup may not think to ask, and
  asking was the only thing that worked this time.

AND WHEN THE MIGRATION COMES: verify a moved WAL by REPLAY OR CHECKSUM, never by
directory size. A partially-copied 2.9 GB directory looks almost exactly like a
correctly-copied one, and the failure stays silent until someone resumes a
session.

THE MIGRATION-GATE SHAPE, worth reusing whole:
· RUN THE VERIFIER AGAINST THE SOURCE FIRST. A replay script that silently
  exercises nothing -- wrong path, zero sessions enumerated, a swallowed
  exception -- returns the same "all good" as a correct one. The baseline turns a
  clean post-move result from an assumption into a COMPARISON.
· RECORD COUNTS FROM THE BASELINE, NOT JUST PASS/FAIL. A post-move replay passing
  over FEWER sessions is exactly what a truncated copy produces, and a boolean
  gate cannot see it. Same as a data-driven suite that silently shrinks: the
  count is the check.
· MOVE, DO NOT COPY-THEN-SWITCH, AND NEVER SYMLINK. A single-writer guard that
  compares file size against its own last-write mark can trip MID-RUN through
  symlink indirection -- and a failure arriving at an arbitrary later moment is
  worse than one at boot, because it detaches the symptom from the change.
· PUT THE ALL-OR-NOTHING CONSTRAINT IN THE SCRIPT, NOT THE PLAN. Where several
  stores cross-reference each other, moving one and exiting nonzero leaves a
  CONSISTENT-LOOKING TREE WITH BROKEN REFERENCES, and the operator's instinct is
  to re-run. All or none removes the class.
· RE-MINT THE BASELINE AGAINST THE STOPPED TREE, not the live one. A baseline
  taken while the service is still writing is stale the moment it is written, so
  a clean post-move check proves "no loss relative to a point BEFORE the move"
  rather than "no loss". The blind spot is exactly as wide as the gap between
  minting and stopping. Costs one command at the top of the window.
· CHECK THAT YOUR FIXTURE CAN CONSTRUCT THE DEFECT. Reproducing a truncation
  gate, I picked a WAL by size filter and got a ZERO-BYTE file: baseline recorded
  frames 0, truncation could not fall short of zero, and the check passed. The
  gate was correct and my input could not exercise it -- and the failure pointed
  the dangerous way, saying the gate does not fire. Corollary for count-based
  gates: an empty file contributes to a FILE count and to nothing else, so
  dropping the file count as "redundant" makes empty files invisible to the gate.
· SHIP THE READ-SIDE CHANGE AHEAD OF THE MOVE, with the fallback arm VERBATIM the
  old expression, so the new binary computes the identical path until someone
  sets the variable. Verify that verbatim-ness at source rather than trusting it;
  it is the whole reason the change is safe to land early. Guard the empty string
  explicitly -- an empty env var is the classic way to relocate durable state to a
  relative path.

## An arrival rate is not an accumulation

I reported a directory as growing -- "4,415 files in three days, newest written
tonight" -- and it was a RING AT EQUILIBRIUM: a 512 MB cap with a 7-day age
bound, measured at 521 MB with zero files older than 3 days. The eviction rate
equalled the arrival rate.

I HAD MEASURED ARRIVAL AND INFERRED ACCUMULATION. A newest-file timestamp tells
you the writer is live; it says NOTHING about whether anything is leaving. The
missing measurement was one command: the age of the OLDEST file. Where that is
much younger than the directory's lifetime, retention is working.

SO FOR ANY "THIS IS GROWING" CLAIM, MEASURE BOTH ENDS. Newest tells you it is
alive; oldest tells you whether it is bounded. Reporting only the first is the
alarming half of a true picture.

## Sweep the class, and expect the plausible culprit to be wrong

Having found one unbounded file (an unrotated 1.34 GB daemon log), asking WHAT
ELSE HAS NO BOUND found a 187 GB tree where that log was 0.7% of the problem --
56 GB of build output in agent worktrees, 15.4 GB of orphaned database
temporaries, both invisible because NO SINGLE COMPONENT OWNS THE TOTAL.

AND THE FIRST HYPOTHESIS WAS WRONG BY TWO ORDERS OF MAGNITUDE. I attributed 24 GB
to my own tooling's exhaust -- the plausible culprit, and the one I was primed to
find because it was mine. Measured: 117 MB. The real consumer was a neighbouring
directory I had not considered.

THE HABIT: when a sweep turns up a big number, MEASURE THE ATTRIBUTION SEPARATELY
FROM THE TOTAL. A plausible owner adjacent to the real one is the easiest wrong
answer to publish, because both the number and the story are true -- they are
just about different objects.

## Retention outlives evidence: the oldest item is the one you can least adjudicate

A cleanup predicate that consults a record to decide safety has an expiry the
designer rarely models: THE RECORD IS ALSO SUBJECT TO RETENTION, and it usually
ages out faster than the thing it describes.

PROOF CASE: a prune predicate needed a task's record to distinguish "this commit
deletes files because the task was to delete them" from "this commit photographed
a worktree mid-teardown". Searching all 3,734 store rows for the oldest candidate
returned ZERO -- the branch outlived its own ledger row. So THE BRANCHES MOST
WORTH PRUNING, being oldest, ARE EXACTLY THE ONES WHOSE ADJUDICATING EVIDENCE NO
LONGER EXISTS.

THE ANTI-PATTERN THIS FORBIDS: treating a missing record as the benign case. An
absent row is not evidence of teardown-photography, nor of anything else -- it is
the same absent-vs-unknown discriminator that has appeared all week, arriving
through a retention policy. Add a REPORTED-BUT-NEVER-AUTO-ACTED class rather than
letting absence fall into either branch.

DESIGN CONSEQUENCE: when a predicate's safety depends on a lookup, compare the
retention horizon of the LOOKUP TABLE against the lifetime of the thing being
judged. If the table is shorter-lived, the predicate is strongest exactly where
it is least needed and weakest exactly where it is.

## A name is a count without a breakdown

Sorting artifacts by their directory name is supplying the plausible attribution
rather than measuring the composition -- the same error as reporting a bare count
and letting the reader infer what it contains, one level down.

PROOF CASE: ten unclassified subtrees needed sorting into authored (preserve) and
generated (discard). I sorted by name and got SEVEN OF TEN. Opening two files per
directory took ninety seconds and flipped three:
· `work/` -- sounds generated, contains hand-written analysis sections.
· `patches/` -- sounds authored, contains generated artifacts whose durable copy
  lives on the source branch.
· `harness-rounds/` -- unclassifiable by name, contains generated iteration logs.
A NAME IS A CLAIM MADE BY WHOEVER CREATED THE DIRECTORY, and it was never
required to stay true.

THIS ALSO STRENGTHENS THE POLARITY ARGUMENT BELOW more than the original specimen
did: a denylist author sorting by name would have gotten three entries wrong IN
THE SILENT DIRECTION -- generated artifacts included forever, with nothing
failing. Under an allowlist the same three mistakes surface as something missing.

## A denylist over a growing tree fails silently; an allowlist fails visibly

The same exchange produced a shape worth generalising past its instance. A
reclaim safety net force-added an entire subtree MINUS a denylist of generated
directories. That denylist was already one entry short, and nobody noticed until
a specimen forced the question.

A DENYLIST OVER A TREE THAT GROWS IS A TRANSCRIBED LIST WITH ALL THE USUAL
PROPERTIES: it agrees with reality on the day it is written, and the first new
subtree defaults to INCLUDED with nothing failing.

INVERTING IT CHANGES THE FAILURE DIRECTION, WHICH IS THE WHOLE ARGUMENT. Under an
allowlist a new subtree defaults to EXCLUDED: worst case something is missed, and
THAT FAILURE IS VISIBLE (the thing you wanted is not there). Under a denylist the
failure is invisible (twelve thousand lines of noise you will not read).

GENERAL FORM: WHEN A LIST GOVERNS A GROWING POPULATION, CHOOSE THE POLARITY
WHOSE DEFAULT FAILS TOWARD SOMETHING A HUMAN WILL NOTICE.

## An impossible number is a gift; a merely wrong one is not

`git ls-files '*.rs' | xargs wc -l | tail -1` reports the LAST BATCH's total, not
the grand total: xargs splits its input across multiple invocations and each one
emits its own "total" line. On a repo small enough for a single batch it is
correct, which is how the idiom survives.

I caught mine only because it printed 0 LINES ACROSS 63 FILES -- impossible on
its face, so it could not be believed. Had the split landed differently it would
have under-reported by some plausible fraction, and I would have published a
confident wrong ratio in a comparison I was using to size a risk. THE VERSION
THAT IS EASY TO CATCH AND THE VERSION THAT IS DANGEROUS ARE THE SAME BUG; which
one you get is set by the input size.

SO DO NOT TREAT "THE NUMBER LOOKED SANE" AS A CHECK. It only rules out the
self-evidently broken instance. Where a count feeds a decision, prefer an
accumulator you can read (`while read; do ...; done | awk '{s+=$1} END'`) over a
pipeline whose batching is invisible, or cross-check one value against a second
method.

SAME SHAPE, DIFFERENT TOOL: a pipeline reporting the last command's exit code,
and `tail -1` reporting the last batch's total. In both, an intermediary quietly
narrows what you are measuring, and the output still looks like the answer to
the question you asked.

## A fallback can substitute a weaker operation, not just a weaker verdict

The swallowed-status family has a member that is harder to see, because nothing
visibly discards a status. `strict-thing || loose-thing` does not suppress a
verdict -- it RUNS SOMETHING ELSE and reports that instead.

PROOF CASE: `bun install --frozen-lockfile || bun install`, in both a CI lane
and a RELEASE lane. The strict form fails in exactly one situation -- the
committed lockfile disagrees with package.json -- and that is precisely the
situation both lanes must refuse. The fallback resolved fresh versions, rewrote
the lock inside the runner, ran typecheck and tests against those unreviewed
versions, and in the release lane PUBLISHED the result. Every step green.

WHAT MAKES IT HARD TO SPOT: read as ordinary robustness ("try the strict thing,
fall back if it does not work"), it looks careful. The question that exposes it
is not "what does the fallback do" but WHEN DOES THE FIRST COMMAND FAIL -- and
if the answer is "exactly in the situation this gate exists to catch", the
fallback is not resilience, it is the bypass.

MEASURE BEFORE REMOVING. Run the strict form yourself: if it succeeds today the
fallback is dead weight and deleting it costs nothing, and if it fails you have
found a real drift that the fallback has been hiding. Either way you learn
something; guessing gets you a red pipeline and no explanation.

AND PROVE THE STRICT FORM ACTUALLY FENCES. Add a dependency to package.json
without touching the lock, confirm the strict form refuses it, restore. Without
that, "strict" is an assumption about a flag's name.

## Distinguish the benign failure by state, not by its wording

When a command's failure is sometimes benign, the tempting test is a substring
match on the error text. That reads as precise and is not: ERROR WORDING IS NOT
A CONTRACT. It changes across tool versions, across API versions, and between a
human-readable renderer and a machine one.

I wrote exactly this while FIXING a swallowed-status defect in a release
workflow, and my own case sweep caught it before it landed: I matched
"already exists" where the tool now emits "already_exists". The consequence is
the bad direction -- a legitimate retag routes to the failure branch and the
release stops, and it stops for a reason the log describes wrongly.

ASK THE STATE QUESTION INSTEAD. Not "did the error say the release exists" but
"does the release exist now". A state query cannot drift with wording, it is
true for the right reason, and it stays correct if the tool starts failing for a
new benign reason nobody enumerated.

THE SAME REASONING RETIRES THE OTHER HALF OF THE PATTERN. `... 2>/dev/null ||
true` on a create-if-absent step is right for the one benign case and wrong for
every other failure -- permissions, network, a malformed argument. Worse than
silence: the next step then fails with a MISLEADING SYMPTOM ("release not
found"), so the reader is sent after a missing artifact rather than the cause
that actually stopped the run.

AND PROVE THE BRANCHES WITHOUT SPENDING THE REAL OPERATION. A release workflow
cannot be exercised by pushing tags. Simulating the command's outcomes -- each
exit status crossed with each state -- costs one shell function and is what
caught the wording defect above.

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

## A label is scoped too, and its scope is the one nobody checks

We treat scope carefully in queries, filters and controls. A LABEL has a scope
too -- what the name refers to -- and it is checked less than any of them,
because a name is not obviously a claim.

My deploy screen read "subconscious is 48h behind master" while the daemon had
been rebuilt an hour earlier. The gap and the changed-file set were computed
correctly PER BINARY; only the label was per repo. So the number was true of the
MCP shim and the name invited reading it as the daemon -- and the reader who
acts on it investigates the artifact that is fine.

WHY IT SURVIVED: thirteen of the fourteen fleet repos ship exactly one binary.
Repo and artifact are the same thing almost everywhere, so the ambiguity is
invisible until the one repo where they differ -- and there, the wrong reading is
the alarming one. This is the sampling trap operating on VOCABULARY rather than
on data: naming habits get trained on a population where the ambiguity cannot
bite, and then carry into the case where it can.

The check: for any name in an output an operator acts on, ask what population
trained it, and whether every member of that population maps one-to-one onto
what the name denotes. Where it does not, name the narrower thing. A label that
is unambiguous only because the general case has not arrived yet is a defect
waiting on a second binary, a second cluster, or a second account.

The operational corollary is worse than the reporting one. An instruction
carrying an ambiguous name gets RESOLVED BY GUESSING, and the guess fails in
whatever way that system fails -- which is not always loudly. A seat pointed at
the wrong module id gets `unknown_module`, which the client RETRIES IN PLACE, so
a typo presents as a hang rather than an error and gets debugged as one.

## A permissive harness route certifies both answers as correct

A test double that never refuses is not merely failing to test refusal. It is
ACTIVELY CERTIFYING that authenticated and unauthenticated callers are the same
thing -- so every test written against it passes, forever, in BOTH directions.

ENGRAM's instance: a simulated cloud answered 200 to any caller on five routes,
including one with no Authorization header at all. Production 401s on all five.
The real defect it hid was an ordering bug -- a preflight that READ before it
MINTED, so the read fell back to an expired token and returned before reaching
the mint that would have fixed it. With a permissive route, read-before-mint and
mint-before-read are indistinguishable.

THE PART THAT MAKES THIS ITS OWN SECTION: after adding the gate, the mutation
reddened TWO tests rather than one. The second was an EXISTING test written
explicitly to prove that the retire path works on a minted token without a valid
account JWT. It could never have observed that, because with a permissive route
there is nothing to distinguish a minted bearer from no bearer. It had been
asserting a property it could not see since the day it was written.

So fixing a permissive double does not only ADD coverage -- it RESTORES coverage
that was already claimed, and the claim is what stopped anyone looking. A test
named for the exact property is the strongest possible signal that the property
is covered, which is why this shape survives review indefinitely.

THE CHECK: for every route or method on a test double, ask what it REFUSES. One
that refuses nothing cannot support any test whose subject is a precondition --
authentication, ordering, admission, capability. Sweep the whole double when you
find one, because the permissive route you found is rarely the only one, and
patching the single route that exposed the defect leaves the identical hole for
the next caller.

A THIRD MECHANISM, AND THE WORST OF THE FAMILY: A REJECTION PRIMITIVE THAT IS
ALL-OR-NOTHING. ENGRAM's harness could make the next request fail, whatever it
was -- so no test could place a failure AFTER an earlier operation had already
mutated server state. That interleaving was structurally unreachable, and it is
not exotic: IT IS WHAT EVERY PARTIAL FAILURE LOOKS LIKE. The double could express
total success and total failure and nothing in between, which is exactly where
the interesting bugs live. The fix is per-operation rejection, so one call can
fail while the rest succeed.

SO WHEN AUDITING A DOUBLE, ASK NOT ONLY "WHAT DOES IT REFUSE" BUT "CAN IT REFUSE
SELECTIVELY". A double that fails everything at once tests recovery from a dead
dependency and nothing about recovery from a HALF-COMPLETED one.

THE CADENCE THIS EARNS: WHEN A DOUBLE IS CAUGHT LYING ONCE, SWEEP EVERY ROUTE IT
SERVES BEFORE LEAVING. ENGRAM found two in one day by pulling a single thread --
first five routes answering success without credentials, then a route answering
`200 {}` to every object read, which meant a cloud-primary manifest path had zero
coverage while looking covered. A double is authored once, by one person, under
one set of assumptions; a permissive route is evidence about the AUTHORING, not
about that route.

AND A SECOND BLIND THAT STACKS ON THE FIRST: a test flag that suppresses
construction of the very subsystem under test. Their cloud arm passed a
capture-only flag that skips building the cloud target entirely, so the arm was
not exercising a degraded cloud -- IT HAD NO CLOUD AT ALL. Two independent blinds
on one arm, either sufficient. When a test configures the system differently from
production, the difference is a claim that the difference does not matter, and
that claim is rarely checked.

THE ASSERTION SHAPE THAT KEEPS SUCH AN ARM HONEST: assert the SPECIFIC failure
that must not occur, not a blanket success. Their cloud arm legitimately ends in
an unrelated downstream error, so `is_ok()` would have been a lie and `is_err()`
would pass even if the subject never ran. "Not THIS error" is the honest
assertion when the run has legitimate failures downstream of what is under test.

## A marker that looks like a marker but predates the change

A deploy differential -- prove the new binary is really running by finding a
string the old one lacks -- has a well-known failure and a less-known one.

The known one: the probe cannot read at all, so every string reads absent and the
result looks like a clean "old binary". A POSITIVE CONTROL fixes it: a string
present in BOTH binaries, which must be found in both.

The less-known one is the inverse and it fails in the dangerous direction. THE
MARKER ITSELF DOES NOT DISCRIMINATE. ENGRAM's case: the obvious marker for a
credential-ordering fix was `"worker token minting failed"` -- topical, specific,
sounds new -- and it reads 3 in BOTH binaries, because the fix REUSES an existing
string. A probe built on it reports the marker present on the OLD binary, and the
reader concludes the swap succeeded without anything having swapped.

FIRST, THOUGH: VERIFY THE MARKER ON THE ARTIFACT YOU JUST BUILT, BEFORE COMPARING
AGAINST ANYTHING. If a string does not appear in a binary compiled from source
containing it, the marker is unusable -- stop there. One command, and it converts
an unfalsifiable comparison into a falsifiable one before it can mislead anyone.

THE REASON THAT PRE-CHECK IS NOT OPTIONAL: "use a string the change introduces,
because those survive into the binary" is simply FALSE FOR SOME LITERALS. rustc
can compare `&str` match arms by length-and-bytes without emitting a contiguous
constant, and then no `strings`-based check can ever see one.

BUT DO NOT LEARN THAT AS "MATCH ARMS DO NOT SURVIVE" -- THALAMUS checked their own
artifact and found one shipped match arm absent and ANOTHER, TWO FILES AWAY IN
THE SAME SYNTACTIC POSITION, PRESENT. Same construct, opposite outcome, and
neither of us can say which optimisation decides it.

SO THE PREDICATE IS NOT ABOUT SYNTAX AT ALL: SURVIVAL IS NOT DECIDABLE FROM THE
SOURCE. That distinction is load-bearing rather than pedantic -- a reader who
learns the match-arm version will CLASSIFY A LITERAL BY LOOKING AT IT, conclude
"this one is a format string, it is fine", and be wrong in the other direction
for reasons nobody has enumerated. THE ONLY AUTHORITY IS A BUILD KNOWN TO CONTAIN
THE STRING.

AND THE FAILURE DIRECTION IS THE DANGEROUS ONE. A marker reading zero in BOTH
images looks exactly like "the deploy did not take", so it produces ACTION rather
than complacency: a redeploy of an artifact that was already correct, which also
appears to fail, which invites escalation.

WHAT TO USE INSTEAD WHEN THE CHANGE IS STRUCTURAL: A MONOMORPHIZATION IS CODE, SO
IT NECESSARILY EMITS A SYMBOL. A change that alters a data structure or
introduces a generic instantiation leaves a symbol that cannot be optimized into
a comparison -- check it with `nm`, not `strings`. Pair it with a reach-control: a
different instantiation of the SAME type present in both images, proving the grep
reaches that corner of the symbol table, so a zero on the marker means absence.

ONE TRAP THERE: mangled names embed a per-build crate disambiguator, so raw
symbol names DIFFER BETWEEN BUILDS EVEN FOR IDENTICAL CODE. Write the pattern to
skip it; a tightened literal match reads zero for a reason unrelated to the
change.

AND A REACH-CONTROL HAS ITS OWN BLIND SPOT, found by THALAMUS inside the very
script they wrote to implement this check. Their control was a string present in
every build, so a broken extractor could not report a false absence. Sound --
until they pointed the script at a Cargo.toml as a negative case and THE CONTROL
PASSED, because the manifest declares the binary's name and the control string
was in the text file too. The script then declared the marker unusable: a false
verdict, in the action-causing direction, produced by the guard added to prevent
exactly that.

A REACH-CONTROL PROVES THE EXTRACTOR CAN FIND THINGS. IT DOES NOT PROVE YOU ARE
LOOKING AT THE RIGHT KIND OF THING. If the input's TYPE matters -- an executable
rather than any file that happens to contain text -- verify the type first;
content-based controls cannot distinguish a binary from a manifest that mentions
it.

So a differential needs THREE strings, not two:
  - one present in both (proves the probe can read),
  - one absent-then-present (the marker),
  - and a REJECTED marker candidate, stated alongside, showing the marker was
    CHOSEN rather than assumed.

The third is the discipline worth adopting. Recording what you rejected is the
only evidence that you checked; a single accepted marker is indistinguishable
from the first string that came to mind.

AND THE PROPOSITION A DIFFERENTIAL PROVES IS NARROWER THAN THE ONE YOU WANT.
Inode and symbol checks answer "did the deploy happen". They cannot answer "did
the deploy fix the thing" -- for that you need a functional probe that exercises
the repaired path end to end. On a box where the fix is needed, only the second
fails before the deploy, which is what makes it the real gate.

THE PATTERN UNDERNEATH, and it recurred three times in two days: THE AVAILABLE
AND EASY CHECK SITS ONE RUNG BELOW THE ONE THAT MATTERS. A health route returning
200 (the service is up) below whether the mutation path works. A `--version`
probe (the binary runs) below whether it contains the fix. An inode match (the
image swapped) below whether the swap repaired anything. Each lower rung is
cheap, scriptable, and produces a green line, which is exactly why it gets
substituted for the rung above without anyone deciding to substitute it.

The question that separates them: WOULD THIS CHECK HAVE FAILED BEFORE THE FIX? If
the answer is no -- if it was already green on the broken box -- it is measuring
something other than what you are trying to establish.

PREDICT THE CONFUSING SIGNAL BEFORE THE WINDOW. If a known-benign anomaly is
expected (a stale error on the first tick, a warning from an unrelated path), say
so IN ADVANCE and say what would distinguish it from a real failure. Named
beforehand, it is a prediction that either holds or does not; discovered live, it
gets rationalised in whichever direction the operator already expects. And the
standing instruction should be to report the confusing signal rather than the
tidy one -- a smoothed-over anomaly is a defect that has been given a reason to
stay.

## A capability table is reviewed one row at a time and defeated by a pair

A deny table lists operations individually, so it gets reviewed individually --
and a privilege boundary can be crossed by a COMPOSITION that no single row
contains.

ENGRAM's case: a slice widened a worker-token deny range from one operation to
seven in a single keystroke, sweeping in every newly added retention op. Fixing
it meant deciding each op, and two of them looked alike: a retention SWEEP and a
retention POLICY SET. Both arrived in the same slice, both are retention, neither
is destructive alone.

But policy is the INPUT that decides what the sweep destroys. Admit both and one
credential can set the bound and then run the operation that honours it. The
sweep's safety argument -- signed plan, lease, grace window, gateway
revalidation -- holds only while the policy those mechanisms evaluate AGAINST is
not writable by the same credential. Admitting the pair makes every one of those
checks evaluate a bound the caller just chose.

THE DISCRIMINATOR: admit an operation the unattended path MUST call to do its
job; deny an operation that SETS THE BOUNDS that path operates within. A sweep is
not a configuration act, even when both live in the same feature and were written
in the same week.

THE REVIEW HABIT: for any capability grant, ask what the granted set can do IN
COMBINATION, not one row at a time. Rows enter a table one at a time and are read
back one at a time, so a composition has no natural moment at which anyone looks
at it.

AND THE TEST SHAPE THAT HID IT: the guard test computed its expected value with a
CHARACTER-FOR-CHARACTER COPY of the implementation's match expression -- the same
expression evaluated twice. It iterated every operation byte, so it LOOKED
exhaustive; the enumeration was real and the oracle was empty. A capability table
must be tested by an explicit enumeration written from the POLICY, each entry
named with its reason, and mutation-proved in BOTH directions -- admitting a
denied op must redden it, and denying an admitted op must too. A one-directional
proof leaves the fail-open half unmeasured, and on a capability table fail-open is
the half that matters.

## A fence whose recovery path is gated by the fence

Two defects in one day, same repo, same shape at two depths -- worth separating,
because the second is much worse and the fix for the first does not touch it.

SHALLOW: an ORDERING error inside one function. A preflight read authenticated
with a stale credential and returned BEFORE the mint that would have refreshed
it, so the fix for the failure was minted on the line the failure skipped. It ran
269 consecutive times with zero successes. Unrecoverable, but only until someone
swaps two statements.

DEEP: a DURABLE STATE DIVERGENCE between two machines. An operation advanced the
server's chain, failed at a later step, and returned without reconciling -- so
the local head went stale. Every subsequent credential mint is fenced on head
equality, and the recovery read that would learn the true head is itself
credential-gated. THE SIDE THAT IS BEHIND CANNOT LEARN IT IS BEHIND WITHOUT A
CREDENTIAL IT CAN ONLY OBTAIN BY PROVING IT IS NOT BEHIND. No code change
resolves the CURRENT instance; only an outside credential does.

THE INVARIANT, which is what to write down rather than the remedy: ANY OPERATION
THAT MUTATES DURABLE SERVER STATE MUST RECONCILE LOCAL STATE BEFORE RETURNING, ON
EVERY PATH INCLUDING ERROR PATHS. Stated as a remedy ("sync after the append"),
the next person who adds a server-mutating step ahead of a fallible one
reintroduces it; stated as an invariant, they have something to check new code
against. And the test must assert the RECONCILIATION -- force the later step to
fail, assert local state advanced -- not the absence of the symptom, which can be
satisfied by making the later step stop failing.

BUT RECONCILIATION CLOSES ONE PRODUCER, NOT THE CLASS. A crash between the two
steps, a network drop mid-reconcile, or a killed process re-enters the identical
deadlock. THE CLASS-KILLING MOVE IS TO MAKE THE REFUSAL CARRY WHAT RECOVERY
NEEDS: at the moment the fence refuses, the server is HOLDING the value the
client lacks, and throwing it away is what makes the deadlock reachable. A
refusal that returns the current head lets the client reconcile from the refusal
itself -- no second credential, no human. Check the security question honestly
(does echoing it enable a replay against the fence it protects?), but ask it,
because a fence that hoards the recovery datum is a fence with a deadlock built
in.

AND RAISE IT WHILE IT STILL HURTS. Once the outside credential lands the symptom
vanishes, and a class that stops hurting stops getting fixed.

TWO THINGS THE SHIPPED FIX DID THAT THE INVARIANT DID NOT SAY, both worth
copying. HOIST THE RECONCILIATION OUT OF THE BRANCH rather than duplicating it
into the error arm: run it before the match, so a future author adding a third
outcome arm CANNOT forget it -- there is nothing arm-local to forget. And CHECK
WHAT ELSE CARRIED THE STALE VALUE: the cleanup call released a lease against the
pre-failure head, which would have failed the same equality fence and stranded
the lease on top of the credential deadlock -- a second failure hiding behind the
first, invisible until a timeout much later. When a stale datum causes one
failure, grep for every other consumer of that datum on the same path.

AND THE TEST WHOSE FAILURE OUTPUT IS THE INCIDENT: theirs reproduces the
production error string and interleaving exactly, in 0.35s. A test that covers
the branch proves the branch runs; a test whose failure output IS the outage
proves the model is right, and hands the next person the incident instead of a
puzzle.

THE INVARIANT PAID OUT WITHIN THE HOUR, and this is the argument for writing
invariants rather than remedies. Phrased as the remedy ("sync after the
retirement append") the question stops at retirement. Phrased as the invariant,
the next question is MECHANICAL -- which other paths mutate durable server state
before a fallible step? Three call sites existed. ALL THREE HAD THE DEFECT.

ONE OF THE THREE WAS THE CRASH-RECOVERY PATH, and that one deserves its own
note: A RECOVERY PATH WITH THIS DEFECT DOES NOT MERELY FAIL TO RECOVER, IT
CONVERTS A RECOVERABLE CRASH INTO AN UNRECOVERABLE DEADLOCK. The mechanism that
exists to reduce blast radius enlarges it, and it fires exactly when the system
is already degraded and nobody has spare attention. RECOVERY PATHS DESERVE A
HEAVIER STANDARD THAN THE PATHS THEY RECOVER, and they routinely get a lighter
one because they are rare and hard to exercise.

THE SUBTLE CALL THERE: reconcile even when the mutating call REPORTS FAILURE. A
lost response is indistinguishable from a refusal, and the record may be durable
either way -- the client's knowledge of a remote mutation is strictly weaker than
the mutation's existence, so any outcome other than proven-not-applied is a
possible mutation. Treating a reported failure as evidence of non-mutation is the
same error as treating an absence in a log as evidence of non-arrival: a claim
about your instrument dressed as a claim about the world.

WHERE TO PUT THE PROTECTION when sibling call sites share a shape: not one test
per site. Write the invariant AT THE SHARED FUNCTION, as a precondition of using
it, so the next person adding a fourth call site reads it. That is cheaper than a
test and it covers sites that do not exist yet.

WRITE THE COMMENT BEFORE YOU ARE SURE THE CHANGE IS FINISHED. ENGRAM found a
second latent failure -- a cleanup call releasing a lease against the stale head,
which would have failed the same fence -- not while making the change but while
EXPLAINING it. Explaining a change is a different cognitive act from making one
and catches things the making does not. Same effect as a checklist auditing the
prose it was built from.

## A protection that emerges from the order of two unrelated checks

The most dangerous safety property is the one nobody chose.

ENGRAM's gateway refuses a stale control head BEFORE checking roster membership.
That ordering was chosen to avoid a membership oracle. It ALSO happens to be the
only thing preventing a replayed envelope from becoming a permanent oracle for
the account's control head -- because the freshness and nonce checks that would
catch a replay live in a DIFFERENT function, called by each handler AFTER the
verifier returns. Swap the two lines and the replay defence disappears. NO TEST
ANYWHERE WOULD FAIL.

I proposed that swap, twice. First to let the refusal carry the head (killed:
the caller is not authenticated at that point at all -- the signature is checked
against the key the request itself carries). Then a membership-first variant to
fix that (killed harder: membership passing proves the envelope was ONCE signed
by an enrolled key, not that the caller possesses it, and with freshness sitting
behind the refusing check the replay window is unbounded).

THE LESSON IS NOT "BE MORE CAREFUL". IT IS THAT A SECURITY ORDERING CANNOT BE
REASONED ABOUT FROM THE ORDERING ALONE. The checks that made possession mean
anything were in another file, in another function, invoked by every caller
rather than by the verifier -- so nothing at the site tells you they exist.

TWO THINGS TO DO WHEN YOU FIND ONE:
· WRITE THE PROPERTY AT THE ORDERING, naming what breaks if reversed. An
  ordering that encodes a security decision and does not say so is
  indistinguishable from an accident, and accidents get tidied for readability.
· TEST THE EMERGENT PROPERTY DIRECTLY, precisely BECAUSE it is incidental.
  Deliberate protections attract tests; emergent ones have none by construction.

AND A NAMING TRAP WORTH ITS OWN LINE: a function called `verify_req` that
verifies SOME of the request. Freshness and replay protection were the CALLER'S
obligation, so a handler that forgets them has a silently unauthenticated path
with a reassuring verifier call right above it. When a verifier does not verify
everything its name implies, say so IN the verifier -- the missing half is
invisible at every call site.

## A self-digesting file makes mutation testing report the opposite of the truth

CEREB nearly recorded a false positive that would have PASSED a mutation gate
while proving nothing.

Their module `include_bytes!`s ITSELF and pins the SHA-256 in a checked-in
artifact. So ANY edit to that file reddens the digest tests -- regardless of what
the edit was. Their first mutation run showed 8 failures and looked like a clean
catch. It was the tamper-evidence firing, not the fence. With the digest repinned
so the signal could be attributed, deleting the fence produced ZERO failures: the
clause was entirely unfenced.

GENERAL FORM: ANY MECHANISM THAT REACTS TO THE FILE CHANGING RATHER THAN TO THE
BEHAVIOUR CHANGING WILL FIRE ON YOUR MUTATION AND LOOK LIKE THE TEST YOU WANTED.
Self-digests, snapshot tests over source, checksum manifests, generated-file
drift checks. NEUTRALISE THEM FIRST, then mutate, then READ WHICH TEST DIED --
the name has to be the one whose subject you broke.

This is the strongest reason to check WHICH test reddened rather than THAT
something did. A count of failures is satisfied by any mechanism sensitive to the
edit; only the name tells you the mutation reached the behaviour.

AND THE FAILURE IS ASYMMETRIC IN THE WORST DIRECTION. The false POSITIVE -- the
digest fires, the mutation proved nothing -- leaves you believing a guard is
fenced when it is not, which is exactly the state the mutation gate exists to
detect. Worse, it LOOKS RIGHT: eight tests red on a security-fence deletion is
the shape of a clean catch, so nothing prompts a second look.

NEUTRALISING IS NOT FREE, EITHER. Repinning a digest is itself an edit to the
artifact, so the restore must return BOTH the source and the artifact. A
half-restored mutation is worse than none: the tree looks clean while carrying a
stale claim.

AND CONFIRM THE MUTATION ACTUALLY APPLIED, which is the cheapest check in the
whole family and the one I skipped. Probing my own tree for this class, my first
substitution MATCHED NOTHING -- the file was unchanged, every test passed, and I
nearly recorded "no hazard here". A MUTATION THAT DID NOT APPLY IS INDISTINGUISHABLE
FROM A GUARD THAT WAS NEVER REACHED: both leave a green suite and an unedited
intuition. Count the occurrences before and after, or diff the file, before
reading anything into the result.

The proof, once the mutation did apply, is worth stating because it is the class
at its purest: inserting a COMMENT containing the watched string -- compiled
away, zero semantic content -- took the guard from green to red.

A CONTRACT QUESTION HAS THREE ANSWERS, NOT TWO. "No contradiction", "we have a
problem", and TRUE IN THE CODE BUT UNFENCED IN THE SUITE. The third is invisible
to memory -- REMEMBERING THE DESIGN CORRECTLY IS EXACTLY WHAT PRODUCES THE
CONFIDENT WRONG ANSWER -- and is only reachable by reading the source AND
breaking it. So when you send a contract clarification, ask for the check rather
than the answer; a seat answering honestly from memory returns a clean "no
contradiction" over an unfenced clause.

AND THE DEFECT UNDERNEATH IT was the outcome-homogeneity class in its purest
form: a validator with SEVEN typed refusals whose entire suite built well-formed
input and asserted success. Every refusing branch was unreachable from the tests,
so the fence the whole design rested on was correct in the code and unfenced in
the suite. The fix shape to copy: one test per clause, each spoiling exactly ONE
field of a shared valid fixture and asserting the EXACT error variant -- so a
refusal is attributable to the clause under test rather than to an incidentally
malformed input -- plus a paired positive vector, since a validator that refused
everything would otherwise satisfy the whole rejection set.

## A mutant that hangs is a result, not a failed experiment

Running both constants against subc's reserved-module HELLO gate: the
constant-ADMIT arm died correctly and immediately, naming three tests. The
constant-REJECT arm NEVER FINISHED -- it exhausted a 30-minute budget, twice,
including with the suite narrowed to the relevant module.

That is not an inconclusive run. Refusing every HELLO means no module ever
registers, so every test that waits for a module to come up waits forever. THE
SUITE'S RESPONSE TO A TOTAL-REFUSAL MUTANT IS A HANG, and a hang carries the same
information as a red: the behaviour is load-bearing, reached, and depended upon.

BUT IT IS A DIFFERENT SHAPE OF EVIDENCE AND MUST BE READ AS ONE. A timeout is
also what an infinite loop, a deadlock, or a broken harness produces -- so a
hanging mutant is only informative when you can say WHY it hangs. Here the reason
is structural and predictable in advance: registration is a precondition of
almost every integration fixture. If you cannot name the mechanism, a timeout is
an absent measurement, not a passed one.

THE OPERATIONAL HAZARD IS THE TREE, NOT THE RESULT. A timed-out mutation run
leaves the mutant IN PLACE -- the restore line never executes, because it sat
after the command that timed out. I checked and found `M supervise.rs` with the
mutant still present. RESTORE, THEN VERIFY THE RESTORE, THEN VERIFY GREEN. The
same half-restored-mutation trap as a repinned digest, arriving by a different
route: the tree looks like work in progress rather than like a live sabotage, and
nothing about it announces itself.

PRACTICAL: put the restore in a trap/finally, or budget the mutation run
explicitly and treat exhaustion as an outcome you will have to clean up after,
not as an error path you can ignore.

## A clean apply proves the patch applied, not that it was the patch you reviewed

BROCA cherry-picked a fix using the sha from the delivery record. It was the sha
of the FIRST delivery, not the revision they had reviewed and asked for. THE PICK
APPLIED CLEANLY, because both versions apply to the same base -- so nothing
failed, nothing warned, and the operation reported exactly what they hoped.

What caught it: comparing the picked `--stat` against the diff they had just
read. 27 insertions where the revision had 50 insertions and 16 deletions.

THE PRESCRIPTION: after any revision round, pick from the WORKTREE HEAD rather
than a recorded sha, and verify with an IDENTIFIER THAT EXISTS ONLY IN THE
REVISED VERSION. The identifier check is the stronger half -- it is a positive
fact rather than an arithmetic coincidence.

GENERALISES PAST CHERRY-PICKS to anything addressed by a recorded reference: a
sha, a tag, a build id, an artifact URL. Success proves the reference resolved,
never that it resolved to what you meant. And the danger is specific to the case
where BOTH candidates are valid: if the wrong sha did not apply you would learn
immediately, so THE FAILURE IS ONLY POSSIBLE WHERE IT IS ALSO SILENT.

## Checking a table against the prose it summarises

The checklist above is a summary of the body, so it can drift from it in two
directions: a row naming a check the body never states, and a body section with
no row. I have hit both.

WHAT DOES NOT WORK, and I tried it: phrase-matching a row's wording against the
body. A body section states its idea IN ITS OWN WORDS -- that is what makes it
prose rather than a restatement -- so the row's phrasing appears exactly once, in
the row. My probe reported zero matches for four rows and I was one step from
"four phantom rows" when the POSITIVE CONTROL came back 1 instead of the >1 I had
predicted. THE CONTROL DID NOT VALIDATE THE INSTRUMENT, IT REFUTED THE METHOD.

Two instrument defects on the way there, both of the kind that return a confident
number: markdown emphasis (`*file*`) breaking a literal phrase match, and mixing
`grep -c` with alternation syntax that needs `-E`. Each produced a zero that
looked exactly like a real absence.

WHAT WORKS: read the section HEADINGS and match SUBJECTS by hand. There are tens
of them, not hundreds, and a heading is written to name its subject. The check is
cheap in the only currency that matters here -- attention on the right thing --
and it cannot be automated by matching text, because the relationship between a
row and its section is semantic rather than lexical.

GENERAL FORM: WHEN A SUMMARY AND ITS SOURCE ARE WRITTEN IN DIFFERENT WORDS BY
DESIGN, NO TEXTUAL COMPARISON CAN RELATE THEM. Reaching for one produces a
number, and the number is about the wording rather than the coverage.

AND NOT EVERY SECTION EARNS A ROW. Two here deliberately have none -- the one on
label scope and the one on emergent protections -- because they are review
postures rather than steps you can take at a moment of work, and A CHECKLIST THAT
GROWS TO COVER EVERY SECTION STOPS BEING A CHECKLIST. The omission is recorded
here so a later reader does not read the gap as drift and 'fix' it: an
unexplained absence and a deliberate one look identical, which is the same reason
a deliberate refusal in code needs its justification written at the refusal.

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

## A check that goes quiet exactly when it matters

A silently skipped check is bad because a clean result covers an arbitrary subset.
It is far worse when the condition that silences the check is CORRELATED WITH THE
CHECK FAILING -- because then the runs least likely to have demonstrated the
property are the runs most likely to report success.

Two instances from one harness, both found by asking the question rather than
waiting for a failure. A protection check ran only when the wire carried enough
items for any to be eligible; too few items means the drive barely exercised the
mechanism, so a WEAK DRIVE silenced the check AND produced a pass. And a reply-form
check needed a decision log, which is exactly the artefact absent when the feature
is misconfigured -- so a misconfigured run announced success over the checks that
survived.

So the question at each skip is not only "can this be skipped" but "WHAT MAKES IT
SKIP, and does that condition make the property MORE likely to be violated?" If it
does, the skip is not a gap in coverage, it is a filter that removes the failing
cases.

A check that could not be attempted neither holds nor fails, so it must not be
absorbed into either. Count it separately, print it with its reason, and report the
run as INCOMPLETE rather than as a pass. And write the control that proves the skip
cannot launder a real violation -- a genuine failure must still fail rather than
reporting as skipped -- because that is the control everyone omits and the only one
separating an honest skip from a silent pass.

## A check that affirms by measuring nothing

THE THIRD SHAPE IS NOT A SKIP AT ALL, AND IT IS THE WORST: A FALSE PASS FROM
SELF-COMPARISON. A check picked the last qualifying item as its comparison subject;
when nothing qualified, the fallback was THE BASELINE ITSELF, so it compared one
thing with itself and AFFIRMED the property. Full green run, no SKIP, no missing
count, exit 0 -- on a drive where the condition never recovered. The anti-
correlation at its most extreme: the run where recovery FAILED is the run that
produces the confident recovery pass.

So the family has three members, increasingly bad: a check that says it skipped; a
check that vanishes and shrinks the denominator with it; and a check that AFFIRMS
BY MEASURING NOTHING. Only the first is visible in output.

WHERE TO LOOK FOR SELF-COMPARISON: any selection with a fallback, and any
before/after comparison against stored state. For a selection, ask what the
fallback resolves to when nothing qualifies -- if it can resolve to the baseline,
the comparison is trivially true. For stored state, CHECK THE ORDER OF READ AND
WRITE: writing the current value before reading the previous one makes every cycle
compare a value with itself and report "unchanged" forever, which is the reassuring
direction. Both of mine read before writing, verified at source rather than
assumed.

## A guard against one empty case can hide another

A VACUITY GUARD PROTECTS AGAINST ONE VACUITY AND CAN HIDE ANOTHER. The second
self-comparison found by this rule sat behind a minimum-length guard, written by
someone who had already worried about trivial passes. It fired correctly -- the
prefix genuinely was long -- and its PASS line made the tautological run look MORE
carefully verified than a bare comparison would have. So the guard did not merely
fail to help; its success was part of the disguise. Ask of any check carrying a
vacuity guard: WHICH vacuity, and does its passing imply anything about the others?
Usually nothing.

AND WHEN THE INPUTS MAKE THE QUESTION UNANSWERABLE, REFUSE RATHER THAN ANSWER. Two
identical arguments where two distinct ones were meant has no correct comparison
available, so producing one is the error. Exit distinctly and say why.

PUT THAT REFUSAL WHERE THE ARGUMENTS ARE READ, NOT WHERE THE MISSING INPUT IS USED.
A guard placed at the point of use sits AFTER whatever setup precedes it -- a store
query, a network call -- so it cannot fire when that setup fails, and the run dies
in a traceback instead of refusing. IF THE ARGUMENTS ALONE SETTLE IT, THE REFUSAL
OWES NOTHING TO ANY MEASUREMENT and belongs before the first one.

## Rank the empty paths by how easy they are to reach

RANK VACUOUS PATHS BY HOW MUCH EFFORT THEY TAKE TO REACH. A tautology needing a
wrong invocation is a hazard; one needing an OMITTED OPTIONAL ARGUMENT is the
default behaviour for anyone running the command for the first time. Worst instance
found: a delta check whose baseline was optional, so without it the comparison
measured from zero and the "movement" attributed to a run was the process's LIFETIME
TOTAL -- asserting "this process has ever done X" while reading as "this run did X",
with identical output either way. THE LONGER THE PROCESS HAS BEEN UP, THE MORE
CERTAINLY IT PASSES. The check goes quieter as UPTIME grows rather than as the
property worsens -- a second way for a check to be silent exactly where it matters.

THE WORST VARIANT NEEDS NO SECOND CONDITION, ONLY TIME. A search that scans
newest-first for anything matching, with the target optional, is correct on a fresh
instance and becomes wrong as history accumulates -- and capture directories,
ledgers and logs only grow. So it passes every test run while the tool is being
built and starts lying once the tool is trusted. Worst instance found: a compaction
assertion with no exchange given matched YESTERDAY's correctly-handled event and
reported six of six checks passed for a run that produced no compaction at all. Not
one check was wrong; every one was true of a different event. THE RUN DESCRIBED
SOMETHING OTHER THAN THE THING BEING ASSERTED, fluently.

A LIFETIME RATIO IS THE NUMERICAL FORM OF THE SAME DEFECT. It is not incorrect, it
answers a question nobody asked while sitting where a window would go -- and it
improves while the fault is unchanged, so it makes the problem look smaller more
convincingly every day the problem persists.

GENERAL FORM OF THE PLACEMENT RULE: A REFUSAL BELONGS AT THE EARLIEST POINT WHERE
THE ANSWER IS ALREADY DETERMINED, NOT AT THE POINT WHERE THE MISSING INPUT IS USED.
Those are the same place only when nothing happens in between, and something always
happens in between.

## Enumerate the space, not the neighbours

ENUMERATE THE SPACE, NOT THE NEIGHBOURS. Having just applied a rule to one neighbour
is its own certainty, one notch smaller than having just understood the rule -- and
it stops you at the same place. Two seats independently fixed a help-fallthrough by
handling the sibling flag and stopping there, leaving every OTHER argument falling
through into the side effect: a typo, a flag copied from another tool, an option that
no longer exists, each silently starting a daemon or a gateway. The boundary that
felt complete was the FLAG boundary; the real one was the ARGUMENT SPACE.

AND A DEFENCE THAT HOLDS FOR THE WRONG REASON GUARDS THE ENTRANCE TO THIS ONE. An
unrecognised flag ALONE often errors -- for a reason unrelated to the flag, such as a
required argument being absent -- so a careless check reads clean. The case that matters is AN UNRECOGNISED
FLAG ALONGSIDE A VALID INVOCATION, which nobody constructs without having seen the
shape before.

## Reading a name loosely: harmless for a monitor, dangerous for an authorizer

A NAME-KEYED READ IS A LIABILITY WHEN IT GATES OBSERVATION AND A FEATURE WHEN IT
GATES AUTHORITY. Same mechanism, opposite correct response, and the discriminator is
THE COST OF READING THE NAME TOO LOOSELY.

A MONITOR reading a name loosely costs nothing -- try the new id, fall back to the
old, and a rename needs no edit. An AUTHORIZER reading a name loosely WIDENS WHAT CAN
ARM THE THING IT PROTECTS, which is the opposite of its purpose: a suffix or prefix
match on a tool name that authorizes a destructive path lets a neighbouring name
authorize it too. For an authorizer the correct answer is EXACT MATCH PLUS A
COORDINATED CHANGE, documented where the constant lives so the coordination is
discoverable at the moment someone renames -- not tolerance.

THIS IS THE FAILURE MODE OF RECEIVING A SHAPE WITHOUT KNOWING WHAT A WRONG ANSWER
COSTS HERE. A seat that
ported the monitor fix directly onto an authorizer would have written a real security
regression while believing they were applying a lesson. So a shape must travel with BOTH the
invocation that exposes it AND what reading it wrongly costs in the recipient's code,
or it arrives as a rule with no idea which way to point.

CHECKED IN MY OWN AUTHORITY PATH: subc's principal grant compares the module id by
exact map lookup and the nonce in constant time, with an empty presented nonce
refused outright. No prefix, suffix or contains anywhere on that path -- so the
rename must add the new id deliberately, which is the coordinated change rather than
a fallback. Right by construction, and now stated so nobody "improves" it into
symmetry with the monitors.

A SHAPE ONLY TRANSFERS IF IT CARRIES THE INVOCATION THAT EXPOSES IT. "Help must not
run the command" alone would have had the recipient test an unknown flag IN ISOLATION,
watch it error for an unrelated reason, and file the binary clean. What made it
transferable was the accompanying detail that the bad flag must sit ALONGSIDE a valid
invocation. A SHAPE WITHOUT ITS TRAP IS AN INVITATION TO A FALSE NEGATIVE, and a false
negative from a shape you were told about is WORSE than never having heard it --
because you now believe you checked, and the area is closed rather than merely
unexamined.

So when sending a shape, send the invocation that exposes it, not only the property.
The property is what makes it interesting; the invocation is what makes it findable.

A CITATION IS A CLAIM ABOUT ANOTHER FILE AND AGES LIKE ONE. A comment saying "covered
by X" is worse than no comment when X does not cover it, because it tells the next
reader the coverage exists and they stop looking. Check the citation before writing
it, and check it again when the test moves -- nothing makes a stale citation fail.

A NOTE ON THE WORDS IN THIS DOCUMENT. Two seats working a class together invent
shorthand fast, and the shorthand feels like understanding. It is not portable: a
reader who was not in the exchange meets a coined phrase and stops, or worse, guesses.
The same hazard applies to CODE COMMENTS written during a sweep -- a comment saying
"the positive half is covered" is meaningless to the next maintainer. Prefer the plain
statement of the mechanism over the name we gave it, and if a name is worth keeping,
define it where it first appears rather than assuming the reader has the conversation.

A DESCRIPTION OF A SHAPE TRAVELS FURTHER THAN A REVIEW. Across four consecutive finds
in two codebases, NOT ONE WAS FOUND BY THE AUTHOR OF THE CODE, and none came from
reading the other's diff. Each came from someone describing a shape they had just hit
and the recipient applying it to different code. Review requires reading what someone
wrote; this requires only knowing what to ask -- which is why it crosses repository
boundaries that review cannot, and why the finding worth sending is the SHAPE rather
than the patch.

## Knowing the pattern is what makes you skip the sibling

KNOWING THE PATTERN IS WHAT MAKES YOU SKIP THE SIBLING. My seats section already
printed MISSING against an expected roster; the modules section one screen away
counted only what answered, and I read its first line every thirty minutes for a
weekend without seeing it. The lesson existed in the same file and did not transfer.
Same shape as two sibling subcommands sharing an idiom where one gets fixed and the
other is never opened. So the mechanical enumeration is LEAST optional exactly when
you are most confident you understand the pattern -- confidence is what replaces the
second look.

A CHECK CAN GO QUIET AS THE FAULT GETS WORSE, AND THERE ARE THREE WAYS IT HAPPENS.
By the PROPERTY: a weak drive silences the very check a weak drive would fail. By
ELAPSED TIME: a lifetime counter passes more certainly the longer a process runs.
And by SEVERITY, which is the worst. A persistence detector comparing
rendered lines calls a fault PERSISTENT when its detail is stable and NEW when the
detail changes -- so a stuck condition whose detail carries a RISING count reads as
new every cycle and never once as persistent. The detector is quietest exactly where
the signal is strongest. Ask of any change-detector: what does WORSENING look like
to it, and does worsening alter the thing it compares?

A DETECTOR KEYED ON THINGS BEING THE SAME IS NOT YET A DEFECT -- IT BECOMES ONE WHEN
ITS DOCUMENTATION PROMISES DETECTION. Compare two fields with the identical blind spot. A
degrade streak documented as "the substrate for a persistent-degrade alarm" makes a
CAPABILITY claim and says nothing about scope -- a reader builds the alarm on it and
finds it silent during the widest outage. A field named `last_error`, documented as
"reason for the MOST RECENT failure", has the same one-slot shape and cannot be
misread, because the NAME STATES THE SCOPE and the doc repeats it. Only the first is
a defect.

SO THE CHEAPEST PROPHYLACTIC IS A NAME THAT STATES ITS OWN SCOPE: a doc then cannot
overclaim without contradicting the name, and a contradiction is something a reader
notices where a silence is not.

BUT CHECK WHICH LEVEL CARRIES THE CLAIM, because they can differ within one
declaration. `CircuitBreaker { identical_failures }` states its scope at the FIELD
and overclaims at the TYPE: the type name promises general protection against
runaway failure, and the type name is what travels -- into design docs, into prose,
into conversation -- while the field name stays at the construction site. A reader
asking "does this task have a circuit breaker?" never sees the qualifier. When the
name cannot be changed without a wire break, the doc has to carry the whole scope,
which is the more expensive form of the same fix.

THE EXPOSURE IS STRUCTURAL AND PREDICTABLE WITHOUT REDOING THE REASONING. A check
that ITERATES EVENTS gets LOUDER as a fault worsens -- more bad events means more
assertions, so severity cannot suppress it. A check that COMPARES A STATE TO ITS OWN
PREVIOUS RENDERING can be suppressed, because worsening alters the compared value
itself.

NARROW IT ONE STEP FURTHER, WHICH THE PAIR IN MY OWN SCRIPT SETTLES: the exposed set
is cross-cycle comparisons WHERE SAMENESS IS THE SIGNAL. My persistence detector
treats unchanged-as-meaningful, so a rising count destroys the signal. My swap-delta
treats changed-as-meaningful, so a rising number IS the report. Same structure,
opposite exposure, decided entirely by which side of the comparison carries the
meaning. So: ANY DETECTOR WHOSE ALARM CONDITION IS "THIS LOOKS LIKE LAST TIME" IS
EXPOSED TO SEVERITY; one whose alarm condition is "this differs from last time" is
not.

DISTINGUISH BEING COVERED FROM BEING DESIGNED. Auditing four text comparisons, three
were safe -- one shadowed by an equality check directly above it, two sitting under
POSITIVE assertions that fail loudly when their literal is renamed. None of that was
intentional; the asymmetry did the work. That is luck WITH A MECHANISM BEHIND IT,
which is worth more than luck and less than design: the mechanism predicts where the
next one will be safe, but nothing stops someone deleting the shadowing check while
tidying. Record which safety is load-bearing and which is incidental, or the next
reader cannot tell them apart.

THE FINDS GENERATE THE CRITERIA, NOT THE REVERSE. Across thirteen defects in one
sweep, NOT ONE was found by the criterion that opened it. Each new criterion arrived
because a defect's SHAPE did not fit the question that had found its predecessor --
silenced checks, then effort ranking, then subject binding, then set size, then
negative assertions, then predicate shape. None came from thinking harder about the
previous question.

So the method is not "have better criteria", it is LET A FIND THAT DOES NOT FIT
REWRITE THE QUESTION. That only works if you examine the SHAPE of what you caught
rather than filing it under the category you were hunting -- filing it is what ends
the sequence, because a defect recorded as another instance of a known class teaches
nothing and the next criterion never arrives.

FIXING THE SAMPLE IS NOT FIXING THE POPULATION. A remedy aimed at the observed
failure narrows the entrance it watched and can leave a wider one open -- in one case
the survivor was reachable by doing LESS than the closed path required. After writing
a fix, ask what OTHER inputs reach the same wrong output, not merely whether the
observed one is now blocked.

## Classify your own fixes: comparison or case

CLASSIFY YOUR OWN FIXES BY THE SHAPE OF THE CONDITION THEY ADD. A fix stated as a
COMPARISON covers causes its author never enumerated; a fix stated as a CASE handles
the case. THE TELL IS WHETHER THE PREDICATE NAMES THE PROPERTY OR NAMES A
CIRCUMSTANCE THAT USUALLY IMPLIES IT.

Worked pair from one sequence. "Compare the count against what SHOULD exist" is
indifferent to whether a module died or an upstream glyph was renamed -- it covered
a second cause with no second edit. "Is this match the NEWEST one" is a proxy for
recency that holds only while the directory keeps growing; the same sequence's own
remedy carried it, and a run producing NO captures made a day-old event trivially
newest, so the original defect survived its own fix by an even lower-effort path
than the one that had been closed. The comparison form was available the whole time
-- the records carry a timestamp, so "does this belong to the run being asserted"
can be asked of the clock directly.

USUALLY IS WHERE THESE LIVE. When a predicate is true because of a circumstance that
ordinarily accompanies the property, write down which circumstance -- and then ask
what makes it stop accompanying.

## A negative assertion passes when the field it names disappears

A NEGATIVE ASSERTION OVER A NAMED FIELD IS SATISFIED BY THE NAME'S ABSENCE. Reading
a counter that is not on the surface yields zero, so "no failures" passes when the
failure counter has been renamed away -- with real failures sitting in a counter
nobody reads. The run gets QUIETER as the surface drifts away from it.

THE ASYMMETRY IS THE AUDIT KEY: positive assertions are self-defending, because
"count > 0" fails loudly the moment its field vanishes. Only NEGATIVE assertions
convert a missing name into a pass. So in any field-driven check, the ones asserting
that something DID NOT happen are the ones exposed to upstream renames -- a much
smaller set than "all checks", and mechanically identifiable by their comparison.
The fix is a comparison, not a maintained list: surfaces enumerate their own fields,
so requiring the named ones costs one check.

WHEN A CHECK RUNS OVER A NAMED RANGE, VERIFY THE RANGE WAS COVERED. Assertions over
rows a store happens to hold are silent about rows it does not: a range half covered
reports the same clean success as one covered completely, and THE FEWER ROWS SURVIVE
THE FEWER CHECKS RUN AND THE QUIETER THE RESULT. The authority is usually already in
hand -- an independent record of what was served, a config the daemon spawns from --
so this needs a comparison rather than a maintained roster.

MAKE PROBES BRITTLE WHILE YOU ARE STILL ESTABLISHING WHAT THEY SAY. Chasing one
count I got 0, then '?', then a crash -- three wrong answers, and ONLY THE CRASH WAS
HONEST. The 0 reads as "none configured" and the '?' as "unknown"; both are
reportable as findings. A probe that fails loudly is cheaper than one that fails
plausibly, and tolerance added early is what makes a broken instrument look like a
result.

## Ask where a tool decides WHICH thing it is talking about

ENUMERATE WHERE THE SUBJECT IS CHOSEN, NOT WHERE CHECKS MIGHT NOT RUN. The five
earlier finds came from asking which checks can be silenced; that key was exhausted.
Asking instead "where does this decide WHICH EVENT it is talking about" found the
purest instance in one pass: a command that scans with NO SUBJECT ARGUMENT AT ALL,
so there is nothing to omit and nothing to get wrong -- the vacuous path is the ONLY
path, correct on a fresh instance and wrong from the moment a second run lands in the
same directory.

THE THIRD REMEDY IS TO QUALIFY THE CLAIM, when the operation is legitimate but its
scope is wider than a reader will assume. A survey that reports "a placeholder
exists" is doing its job; the defect is that an unqualified finding is read as a
finding about the run just performed. "...in any capture on disk" is still true,
still unscoped, and now unmistakable. THE LABEL IS THE BINDING, as far as anyone
reading the output is concerned -- which is why a lifetime ratio marked all-time is
fixed while the same number unmarked is not.

AND NOTE WHAT NOT TO DO: requiring a scope bound on a genuinely unscoped survey is
as wrong as leaving it unlabelled. Refusal and qualification are not interchangeable
-- refusal denies an answer that exists.

AND CHOOSE REFUSAL OR SKIP BY WHETHER A LEGITIMATE READING EXISTS. Two identical
arguments where two distinct ones were meant has no correct answer, so refuse.
A newest-first match that lands on an older event has one -- the run may genuinely
be describing something else -- so record it as a skip that names what it found.

WHEN COUNTING INSTANCES, SEPARATE THE SHAPE FROM THE COUNT. Five instances of one
pattern in one file is a fact about that file -- five subcommands sharing one
counter-and-compare idiom -- not a competence difference between the people who
wrote it and the people who did not. The shape generalises; the count does not.

RELATED, AND THE REASON TO ENUMERATE RATHER THAN TRUST: fixing one call site says
nothing about its siblings. A correct new mechanism used in one place leaves the
other place untouched and still wrong. The remedy is not more care at the moment of
fixing; it is listing every site mechanically afterwards and reading what follows
each one. Nineteen sites, two of them checks that had quietly stopped being checks.

## Pre-commit the stop condition

Write it down before the round runs. A stop rule authored after the result is a
rationalisation with a timestamp.

State it as a property of the *result*, not a round count: "if this returns a
speculative hardening proposal rather than a triggerable defect, stop." A good
one binds in both directions — that rule would have ended the loop on a weak
round, and it also refused to allow stopping on a real defect when stopping
would have been the tidier ending.

## A key narrower than the statement it indexes

Two different questions get asked of a storage key, and answering one of them
well reads as answering both.

The first is about mutability: does the key contain anything that changes? An
epoch, a reason, a freshness marker, a generation counter — key on one of those
and the lookup misses at exactly the moment the value moves. That rule is easy
to state and easy to check.

The second is about scope, and it is invisible from the first. A key can be
perfectly stable in every component and still be *narrower than the statement
stored under it*. The case: membership standing is authored about an account,
while device binding is authored about a device. Storing the composed record
under a key containing the device means a revocation indexes one device — so a
sibling device, or one enrolled afterwards, presents an older assertion and is
not fenced by it. Every component of that key was stable. A mutability review
passes it without comment.

So the companion check is: **the key must be no narrower than the broadest
statement stored under it.** Ask what the record *asserts about*, not what it is
filed beside.

A related precedence rule falls out of the same case, and it is the mechanism
rather than the principle. Where records carry both a class and an ordering
value, the class must be evaluated *before* the ordering. Otherwise a record
with sufficient ordering weight wins numerically and clears a state its class
had no authority over. The same shape appears in a backup descriptor whose
entries carry both a class and a mechanism: class is evaluated first, so
guidance phrased purely in terms of mechanism misleads — it describes a test
that never runs for the entries an earlier test already excluded.

The generalisation worth carrying: where a decision has an authority dimension
and a recency dimension, recency must never be able to overrule authority. A
remedy may only be applied by a party at least as strong as the one that
imposed what it undoes.

## Name the field a checklist step will read

A runbook step that has never been traced to a real surface is prose. It reads
exactly like a step that was, and the difference only shows under the pressure
of actually running it.

The case: a migration runbook said to confirm, after the move, that the module
appeared in the backup service's module set with its declared entries — a
deliberate positive check, written because absence of an error cannot separate
"enrolled and capturing" from "enrolled and inert." The reasoning was right and
two people agreed on it. Nobody looked at the reply an operator would read. It
reported a count of *planned* entries, so the one module whose data was never
captured rendered identically to one capturing gigabytes. The step would have
been performed, its expected output seen, and the window closed with more
confidence than it opened — about a backup that did not exist.

So the check would not have failed to confirm. It would have confirmed the wrong
thing, convincingly, which is worse than having no step at all: an operator who
finds an adjacent signal reads it as the check and stops looking.

Two things follow. Name the surface and the field for every step *before* the
window, because it is cheap in advance and cannot be done honestly mid-procedure
with a daemon stopped. And when a field cannot answer, prefer deleting the step
and saying so plainly over qualifying it — a qualified step still gets run, and
the qualification is the first thing that drops under time pressure.

The underlying shape is broader than runbooks: a signal that is *correct for the
question it answers* and read as answering a neighbouring one. Three instances
landed in a single evening — a rule stated in terms of one column when an
earlier column decided the outcome, a descriptor entry read as coverage when it
only ever meant discovery, and this count. In none of them was anything broken
or lying. The reading sat adjacent to the meaning, and every layer reported
success.
