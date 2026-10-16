# The contribution gate

Every repository we own runs the same contribution gate, so a contributor who
has learned the rule in one of them has learned it in all of them. The gate has
one canonical home — this repository — and every other repository calls it with
`uses:`. Nobody copies it; two copies drift.

> Behaviour changes need an approved issue first. Bypass is the
> maintainer-applied `trivial` label for contributor PRs; the repository's own
> configured bot PRs are exempt. A path glob is a judgement
> about effect made from a filename, and the filename is not where the effect
> lives — a test-only PR can delete the test that guarded a behaviour.

## What the gate does

A pull request must link an issue carrying a maintainer-applied
`design-approved` label. If it does not, the gate comments once, converts the
pull request back to draft, and publishes a failing `design-gate` check on the
head commit.

Applying `design-approved` to the issue later turns the check green on every
open pull request linking it, with no push required, and flips back to ready
only the drafts the gate itself converted. The gate records each conversion as a
second hidden marker (`<!-- design-gate:converted -->`) in its own comment; the
marker stays while the pull request remains a draft and is dropped once any run
sees it out of draft or approval releases it. A draft without that record, such
as one the author opened as a draft to ask for design guidance, is the author's
own decision and stays a draft. If the gate cannot read the comments it marks
nothing ready.

Bypasses are a maintainer-only `trivial` label on the pull request, and the
`ci/**`, `train/**` and `alfonso/**` branch prefixes. The prefixes count only
when the head branch is in the repository itself, so a fork cannot buy a bypass
by naming its branch after a maintainer one. They are path prefixes rather than
substrings: a branch simply named `train` is an ordinary branch.

The repository's own configured bot authors are also exempt on same-repository
branches, regardless of branch name or labels. Bot PRs that do not meet those
conditions and contributor PRs still need an approved issue unless one of the
existing bypasses applies.

It runs on `pull_request_target`, so it has the base repository's context and
its secrets, though the gate reads none. Every write uses the calling job's
own `GITHUB_TOKEN`: the comment, the conversion back to draft, and the
`design-gate` check run, the one output branch protection actually reads.
Under `pull_request_target` that token can write even on a fork pull request.

## What a repository adds

### 1. The caller workflow

`.github/workflows/design-gate.yml`, in the adopting repository:

```yaml
name: Design gate

on:
  pull_request_target:
    types: [opened, edited, reopened, synchronize, ready_for_review, labeled, unlabeled]
  issues:
    types: [labeled]

jobs:
  design-gate:
    permissions:
      contents: read # fetch the gate action
      issues: read # read the linked issue's labels
      pull-requests: write # gate comment, draft conversion, ready for review
      checks: write # publish the design-gate check run
    uses: cortexkit/subconscious/.github/workflows/design-gate.yml@master
    # Optional: this repository's own bot accounts, as login:id pairs.
    with:
      owner-bots: "example-app[bot]:123456789"
```

The triggers live in the caller because a reusable workflow cannot add any.
Both trigger blocks are required: `pull_request_target` evaluates pull
requests, and `issues: labeled` is what releases the drafts waiting on an issue
when a maintainer approves it. A caller that declares only the first gets a
gate that blocks and never unblocks.

`owner-bots` is optional (empty by default): a whitespace- or comma-separated
list of `login:id` pairs for the repository owner's own bots. It exempts only
authors whose GitHub type is `Bot` and whose head branch is in the base
repository. Both login and numeric id must match, so a reused login or a
different account cannot inherit an exemption. Under `pull_request_target`,
the caller workflow and this configuration are read from the **base branch**;
a pull request cannot grant itself an exemption. Find a bot's numeric id with
`gh api '/users/<login>'` and read the `.id` field. Each login must end in
`[bot]` and contain no colon, and each id must contain only digits; malformed
entries fail the run and name the bad entry rather than being ignored.

`labeled` and `unlabeled` are in the pull request types so that a maintainer
applying `trivial` to a failing pull request (or removing it) re-runs the gate
and the check follows the label. The payload of a label event carries the pull
request's current labels, so the gate simply re-evaluates; a label change never
converts a pull request back to draft — only `opened` and `ready_for_review`
do.

The `permissions:` block is an obligation rather than a knob, and `checks:
write` is the half worth understanding. The gate publishes the `design-gate`
check run with the calling job's own `GITHUB_TOKEN` — but a called workflow can only
narrow the scopes its caller grants, never widen them, and a repository whose
default workflow permissions are read-only grants none of them by default.
Withhold `checks: write` and the publish is refused; the gate fails closed, so
every pull request stays blocked. Copy all four lines: naming any permission
sets every unnamed one to `none`.

Apart from `owner-bots`, nothing else is configurable, and that is deliberate.
The decision of which event arm runs lives inside the reusable workflow rather
than in an `if:` in the caller, so a repository cannot switch the gate off by
leaving a condition out.

### 2. No secrets

The gate holds no credential beyond the calling job's own `GITHUB_TOKEN`, and
the caller passes no secrets. Under `pull_request_target` that token carries
the write scopes the caller grants even on pull requests from forks (the
`pull_request` trigger is the one whose token is read-only there), so it can
post the gate comment, convert a pull request to draft and publish the check
run. Keep it that way: the gate runs in public repositories, and an App private
key reachable from a `pull_request_target` job there is one bug away from
exposing every repository that App can read.

The reusable workflow declares no secrets, so a caller that still passes
`CK_CI_APP_ID` or `CK_CI_APP_PRIVATE_KEY` is refused when its workflow is
parsed. Delete its `secrets:` block.

### 3. The two labels

| Label | Lives on | Applied by |
| --- | --- | --- |
| `design-approved` | issues | a maintainer, once the design is agreed |
| `trivial` | pull requests | a maintainer, when there is no design to agree |

Create both labels by hand once when adopting the gate. The gate tries to
create them on pull request runs if they are missing, but the caller's
`issues: read` grant cannot create labels, so the attempt is refused: the gate
then logs a warning, notes it in the gate comment, and continues evaluating the
pull request. Existing labels are left as they are.

### 4. The pull request template

Copy `.github/pull_request_template.md` from this repository. The line that
matters is the first one under its heading:

```markdown
Approved issue: #
```

It sits in the place a contributor actually reads, and the gate's parser is
pinned against it: `scripts/design-gate.test.mjs` asserts that an unfilled
template does not satisfy the gate and that filling in the number does.

The gate accepts `Approved issue: #N`, `Refs #N`, `Part of #N`, and GitHub's
closing keywords (`Closes #N` and friends), each also as `owner/repo#N` or an
issue URL in the same repository. It never asks for a closing keyword: GitHub
closes the issue the moment the pull request merges, before the release that
carries the change has shipped, and a pull request that is one step of an issue
would have to claim a closure it does not intend. Maintainers close issues when
the fix ships.

### 5. Branch protection

Make **`design-gate`** a required status check.

Do **not** require `design-gate / design-gate`. That name is real — GitHub
names a job called from a reusable workflow `<caller job id> / <called job
id>`, and the prefix cannot be suppressed because `uses:` takes no expressions
— but half of it is the calling repository's own job id. Requiring it would
write a name the repository can rename into that repository's branch-protection
contract.

`design-gate` is published on the head commit by the gate script itself, from
both event arms, so the name has exactly one author and no caller edit can
change it. The composed `design-gate / design-gate` job status is
informational: read it when you want to know whether the run itself failed.

## The trap

> A required check that a sha never produces blocks that sha forever. When a
> repo makes this gate a required status on a public branch, the job name is
> part of the contract and the triggers must be unfiltered — the first renamed
> job or paths-filter blocks main permanently, with nothing in the run list to
> say why.

Both halves of that bite, and they are answered differently.

The job-name half is answered by construction: the required check is published
by the script under a name the script owns, so renaming the caller's job
changes only the informational status. This is worth knowing rather than
trusting — it is the one part of the contract that a repository cannot break by
editing its own workflow, and the reason the reusable workflow contains no
checkout is the same reason: remove the argument rather than trust it.

The trigger half is not answered by construction and is entirely on the
adopting repository. **Do not add a `paths:` or `paths-ignore:` filter to the
caller's `on:` block, and do not narrow the `types:` lists.** A filtered
trigger means a docs-only pull request produces no run, no run produces no
`design-gate` check, and a required check that is never produced leaves the
pull request unmergeable with an empty check list. Copy the `on:` block above
as it stands.

The `types:` list matters for the same reason in the other direction: leave
`labeled` and `unlabeled` out and applying `trivial` to an already-failing pull
request does not re-run the gate, so the check stays red until someone re-runs
it from the checks tab or pushes.

## Why there is no checkout

`pull_request_target` runs in the base repository's context with access to its
secrets, so contributor code from the pull request head must never be checked
out or executed. The reusable workflow therefore contains no `actions/checkout`
step and no `git` invocation at all. The gate script is packaged as a composite
action, `.github/actions/design-gate`, which the runner fetches itself — so
there is no ref for a caller to supply and nothing a caller can pass that
reaches a checkout.

That is stronger than checking out the base ref correctly, which is what the
gate did before: correctness by omitted argument survives only until someone
adds the argument back. `scripts/design-gate.test.mjs` asserts the absence
against both files that actually run the gate.

## Verifying a lift

```
node --test scripts/design-gate.test.mjs
```

**The suite contains 149 test cases.** That number is pinned here on purpose: a lift that
reports anything other than `# tests 149` has a setup defect before it has a
gate. The security arms read the workflow and the action from disk, and a
missing file reports as a named failure — `workflow file missing at <path>` —
rather than as test cases that quietly fail to register.

`actionlint` covers the reusable workflow.

## Changing the gate

Changes land here and every caller picks them up, so a change to the gate is a
change to every repository we own. The callers pin `@master`; pin a tag instead
if a repository needs to hold a version, and note that the reusable workflow
references its composite action by an explicit ref of its own, so the two move
together only on `master`.
