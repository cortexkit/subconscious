# cortexkit-release

`cortexkit-release` is the Rust package for the `ck-release` binary. It drives
declared release trains through validation, planning, approval, durable replay,
and verified staging. Repository declarations are data in
`.cortexkit/release.jsonc`; they never provide executable release code.

## Machine boundary

Production durable state has one ruled root:

```text
~/.local/share/cortexkit/release/<repository-id>/<train>-<intended-commit>.journal
```

The associated append-only intent stream, approval record, and leases live
below that same root. `ck-release` never writes its journal into the release
repository and never reads or emits provider credentials.

The first public trigger is an explicit approval boundary. The machine renders
the complete approval subject—repository, train, intended commit, declaration
digest, artifact identities and digests, release version or run ID, and the
ordered public-effect list—before it can admit a public effect. A material
change requires a new approval. `plan --dry-run` and `status` do not acquire
release credentials or invoke providers.

There is deliberately no `place` command and no `place` phase. A successful
train ends at verified staged artifacts and emits placement instructions for
the separate operator-owned fleet ceremony. Placement, restart, inode checks,
and behavioural acceptance remain outside `ck-release`.

## Commands and machine output

Every command supports `--json`. Its output is a versioned envelope with a
stable refusal code and exit status `2` for an unsafe or invalid request.
Automation must branch on `error.code`, not diagnostic text.

```text
ck-release declare [--repo PATH]
ck-release validate [--repo PATH] [--train TRAIN]
ck-release plan --train TRAIN --dry-run --artifact ID=PATH [--repo PATH]
ck-release execute --train TRAIN --artifact ID=PATH [--confirm-first-public-trigger]
ck-release resume --train TRAIN --artifact ID=PATH
ck-release status --train TRAIN [--repo PATH]
ck-release abandon TRAIN [--repo PATH]
ck-release rebind TRAIN [--repo PATH]
ck-release rebind TRAIN --confirm DIGEST [--repo PATH]
```

`rebind` first displays a structural declaration diff. Supplying the exact
replacement digest on a later invocation is the explicit confirmation that
re-pins the declaration and invalidates earlier approval state. A digest
mismatch on `resume` instead refuses and names `abandon` and `rebind` as the
available operator ceremonies.

The `--synthetic-provider` and `--interrupt-after-effect` options are for the
hermetic synthetic train exercised by this package's tests. They are not a
production provider implementation. The synthetic interruption makes the fake
effect durable, returns before the completion append, and then proves `resume`
uses the done-probe to append completion without a second executor call.

## Acceptance-baseline walkthrough mapping

The normative acceptance baseline records one manual sequence: publish an
asset, verify it, stage it with the intended revision, then hand it to the
separate placement ceremony. The following is a 1:1 mapping of those manual
steps to machine phases and output boundaries.

| Manual baseline step | Machine phase or boundary | Machine evidence |
| --- | --- | --- |
| Publish the finalized asset | `publish` or `assets` | Write-ahead intent, approval subject, and done-probe evidence |
| Verify the published asset | `verify_readback` | Phase completion plus identity-matching probe conclusion |
| Stage the verified asset | `stage` | Verified staged-artifact terminal state |
| Confirm the intended revision | Artifact identity channel and the train's `intended_commit` | Identity evidence in the plan, probe, and approval subject |
| Hand off for live-fleet placement | Terminal placement-instructions boundary | `placement_instructions`; no machine-owned action follows |

The final row intentionally maps to an emitted instruction rather than a
phase: the manual placement, restart, inode verification, and behavioural
acceptance are operator work and are not reported as machine completion.
