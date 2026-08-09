# Vendored corpora: where these bytes came from

These files are **copies**. They are minted by `cortexkit-account` and vendored
here so this repo's conformance work has fixed bytes to check against.

Recording the source commit is the one provenance check a consumer *can* run. A
producer can regenerate an artifact from its generator and require byte-identity;
a consumer holds only the artifact, so the strongest available assertion is
"these bytes are the ones published at a named commit". Without that, a hand-edit
that keeps every module id live passes every check in `scripts/fleet/check-vendored-module-ids.sh` —
that script asserts the ids are *live*, never that the bytes are *authentic*.

Before this file existed, the source commit lived only in the peer message that
requested the re-vendor. That message scrolls away; the corpora do not.

## Current vendoring

Source: `cortexkit-account` commit `52188788fafc0ab4684282d5d88e58e9a234ffe7`

| file | sha256 (first 16) |
| --- | --- |
| `fixtures/room2-ckcred-fixtures.json` | `951428d63b8f2dff` |
| `appendices/room2-ckcred-fixtures.json` | `951428d63b8f2dff` |
| `conformance/vectors/ckcred/room1-contract-samples.json` | `39e6b2d4e78e1c8f` |
| `fixtures/room1-contract-samples.mirror.json` | `39e6b2d4e78e1c8f` |

The two identical pairs are deliberate — each corpus is vendored to two
locations, and they must move together. A digest mismatch *within* a pair means
one copy was re-vendored and the other was not, which is worse than both being
stale: two of our own mirrors disagreeing reads as a contract dispute, and the
first place anyone looks is the contract rather than the copy.

## Re-vendoring

1. Re-vendor **every** file in the table, not only the ones a producer names.
   A producer can only notify about copies it knows exist; enumerating our own
   copies is the half that finds all of them. The room1 pair was found this way
   after a notice that named only the room2 pair.
2. Update the commit and the digests above in the same change.
3. Run `scripts/fleet/check-vendored-module-ids.sh` and confirm it reports a
   non-zero **recognised** count. A zero-unknown result alone cannot distinguish
   a clean corpus from a scan that silently read nothing.

Re-minting changes every signature, so a re-vendor is a deliberate amendment at
a named commit rather than a silent refresh.

## Why the provenance lives here and not inside the corpora

Embedding the producing commit *in* each corpus was proposed and withdrawn after
being tested rather than argued: the producer verifies its corpora by
regenerating them and requiring byte-identity, and a commit hash is not knowable
until after the bytes are written, so an artifact carrying its own commit
permanently disagrees with its own regeneration. Measured on a throwaway branch —
verification came back stale both at the mint commit and after an unrelated one.

The general form is worth keeping, because two seats hit it the same day from
different directions: **no value derived from a commit can be embedded in that
commit's own artifacts** — commit hash, tag, tree signature, build UUID alike.
It is a fixed point rather than a tooling limitation, so no amount of care makes
it work.

Referencing the *parent* commit escapes the fixed point and is confusing forever
after, since the recorded value never matches the commit a reader is standing on.
Provenance recorded outside the artifact has no fixed point by construction,
which is why this file can name a commit and a corpus cannot.

## Checking authenticity against the producer

The digests above answer "do these bytes match what we recorded", which is not
the same as "are these the bytes the producer published". When the producer repo
is present, compare directly at the recorded commit:

    git -C <account-repo> show <commit>:<path> | shasum -a 256

List the producer's paths with `git show --name-only --format=`. Do **not** use
`--stat` for this: it truncates long paths with an ellipsis to fit a terminal,
and a truncated path fed to another git command reports the file as absent from
the repo — which reads as "this vendoring points at nothing" rather than as a
formatting artifact. A display-oriented command's output parsed by a machine is
its own defect class; prefer the machine-readable form even when the human form
looks identical on the examples in front of you.

## The producer's own check, verified at source

The producer does not merely store these corpora — it REGENERATES them and
requires byte-identity, which is what makes "the bytes published at commit X" a
meaningful claim rather than a label. Verified in `cortexkit-account` at
`workers/account/test/fixtures/verify-contract-fixtures.mjs`:

    const first  = regenerate(generator);
    const second = regenerate(generator);
    assert.equal(second, first, `${generator} must be deterministic`);
    assert.equal(readFileSync(join(fixtureDir, artifact), "utf8"), first,
                 `${artifact} is stale`);

Both assertions matter and neither substitutes: the first proves the generator
is deterministic across two runs, the second proves the stored artifact is that
output. A generator that varied per run would satisfy the second alone on the
run that wrote the file.

It is ENFORCED rather than available — `workers/account/package.json` chains it
ahead of the test suite (`fixtures:verify && … && vitest run`), so a stale
artifact fails at the `&&` rather than depending on someone remembering to run
it. And the generator list is DERIVED by enumerating `mint-*.mjs` rather than
transcribed, with an explicit refusal when the enumeration comes back empty —
so a corpus cannot be silently excluded by being omitted from a list. Both
corpora vendored here have generators (`mint-room1-contract-samples.mjs`,
`mint-room2-contract-samples.mjs`), so both are covered.

Recorded because subconscious commit `8c3994e9` asserts this property in its
message, and a pushed commit body cannot be amended. The claim was relayed when
written and confirmed afterwards at the citation above — noted so a later reader
finds a confirmation with a location rather than an unsourced assertion.

Finding it is harder than it looks: the check is a standalone node script
chained in a package script, NOT a case in the vitest suite. Searching the test
suite for it — the obvious move — structurally cannot find it.
