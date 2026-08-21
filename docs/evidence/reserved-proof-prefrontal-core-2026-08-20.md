# prefrontal-core reservation: live three-arm proof

Recorded 2026-08-20T09:28:00Z against the production daemon (127.0.0.1:8757, ck-subc 0.6.1).
Harness: clients/subc-client/tests/reserved-proof.ts (production auth + envelope code).

## Enforcement timing (CKCRED's ambiguity, resolved empirically)

Enforcement went live AT RESCAN, before any bounce: the negative arms below
were first run ~40 minutes BEFORE prefrontal-core's restart and already read
reserved_module. Mechanism: rescan's changed-pending-reload path calls
apply_identity_configuration (supervise.rs), which populates the HELLO gate
from the CURRENT spawn nonce without waiting for a respawn.

## The ladder (re-run post-bounce, codes verbatim)

```
# reserved-proof for module_id=prefrontal-core at 2026-08-20T09:28:00.352Z
# daemon: /Users/ufukaltinok/.local/share/cortexkit/run/subc-connection.json (127.0.0.1:8757, ver=0.6.1)
ARM 1 forged-nonce: Error code=reserved_module message="module_id 'prefrontal-core' is reserved; HELLO without a valid launch nonce is rejected"
ARM 2 absent-nonce: Error code=reserved_module message="module_id 'prefrontal-core' is reserved; HELLO without a valid launch nonce is rejected"
# ARM 3 positive: recorded separately from catalog.list (the real module's own registration)
```

## ARM 3, positive control: the real module through the same gate

prefrontal-core re-registered through the reserved gate during ALF's deploy
bounce (their binary inode 893541087, attempt 1), HELLO passing with the
daemon-minted nonce echoed by subc-client-rs (lib.rs:1211). Catalog state:
```
id                  state     enabled  live   health  
aft                 running   true     true   ok      
astrocyte           running   true     true   ok      
prefrontal-core     running   true     true   ok      
```

## Check-order property (what makes the negatives non-vacuous)

The reserved gate precedes the duplicate-id check in control.rs, so a forged
probe against the OCCUPIED name reads reserved_module, never duplicate_module_id.
The codes above show it: both arms fired while the real module held the name.

## Known limitation at recording time (fix in flight)

A reserved id that has NEVER been spawned (e.g. enabled:false) had no gate
entry and admitted any HELLO — found by probing the reservation-probe canary,
which REGISTERED with a forged nonce (registration died with its socket;
verified absent from catalog after). Fixed in subc-core (reserved names with
no legitimate holder now refuse ALL HELLOs); serves at the next daemon bounce.
Every SPAWNED reserved module (claustrum, cerebellum, condition-runner,
prefrontal-core) was protected throughout.

## Post-deploy acceptance, 2026-08-21 (daemon 0.7.0 @ bb0a64b5, pid 6467)

Live production run of `clients/subc-client/tests/reserved-proof.ts` after the
bounce that activated cdb90283 (a reserved name with no legitimate holder
refuses every HELLO).

Spawned reserved id (`prefrontal-core`):

    ARM 1 forged-nonce: Error code=reserved_module message="module_id 'prefrontal-core' is reserved; HELLO without a valid launch nonce is rejected"
    ARM 2 absent-nonce: Error code=reserved_module message="module_id 'prefrontal-core' is reserved; HELLO without a valid launch nonce is rejected"
    ARM 3 positive: prefrontal-core running/ok in `ck module list` (its own registration through the same gate)

Never-spawned reserved id (`reservation-probe`, `enabled: false` canary):

    ARM 1 forged-nonce: Error code=reserved_module message="module_id 'reservation-probe' is reserved; HELLO without a valid launch nonce is rejected"
    ARM 2 absent-nonce: Error code=reserved_module message="module_id 'reservation-probe' is reserved; HELLO without a valid launch nonce is rejected"

The canary arms are the lazily-populated-gate close: before cdb90283 a
reserved-but-never-spawned id had no gate entry and admitted any claimant.

Instrument note, recorded because the artifact looked exactly like the defect:
the harness takes a POSITIONAL module id; a first canary run passed
`--module-id reservation-probe` and probed the literal string `--module-id`
(unreserved, correctly admitted). The header line printing the probed subject
is what caught it. Verdict lines above are from the corrected positional run.

## Nonce forgery surface (the claim consumers cite, stated once)

What a spawn nonce is: minted per-spawn from the daemon's CSPRNG, injected into
the child's environment (`SUBC_LAUNCH_NONCE`), never logged, never on the wire
outside the HELLO that redeems it. The reserved-name gate holds it only while
the process it authorizes is the live holder; a reserved id with no live
holder holds `None` and refuses every claimant (the canary arms above).

The boundary, verified live by CKCRED on this host (`ps eww` against the
running vault process, same uid): **supervisor attestation is trustworthy
against remote and cross-account claimants; against same-uid local processes
it is an integrity signal, not a secret.** Same-uid readability is a property
of process environments on this platform, not of the minting. A same-uid
impostor additionally needs a window in which the reserved module is NOT the
live holder, because the duplicate-id gate refuses a second registration while
it is — which is what makes principal-scoped grants strictly stronger than
on-disk bearer handles for this attacker class: the file asks for one read,
the impersonation asks for a read plus a window that does not normally exist.

Named non-goal: no read-plane mechanism on this host defends against a
same-uid adversary. Designs consuming this attestation should state that as a
non-goal rather than leaving it for an auditor to discover.
