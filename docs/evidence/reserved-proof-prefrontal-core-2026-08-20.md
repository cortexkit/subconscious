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
