---
task_id: bg_2fa3e54b
agent: council-member
session_id: ses_0ce3c5869ffeUH26KwGZ6NCtyE
parent_session_id: ses_0ce3e0e75ffed3gUYBr4lZyfSt
status: completed
completed_at: 2026-07-05T10:14:20Z
---

[user] 2026-07-05T10:12:14Z
## Solo Analysis Mode
You MUST do ALL exploration yourself using your available read/search tools.
- Do NOT use task or any delegation tool under any circumstances
- Do NOT delegate to explore, librarian, or any other subagent
- Do NOT spawn background tasks
- Search the codebase directly — you have full read-only access to every file
- This mode produces the most thorough analysis because you see every result firsthand


## Analysis Intent: AUDIT

You are conducting an **audit** — your goal is to find discrete issues, risks, or violations.

**Focus:**
- Search for problems, anti-patterns, security risks, correctness issues, or violations of stated requirements
- Each finding must be a distinct, actionable item with concrete evidence
- Severity determines priority: critical (blocks/breaks), high (significant risk), medium (should fix), low (nice to fix)
- For each finding, provide the specific location (reference, section, or component where it occurs)
- State your confidence: high (clear evidence), medium (likely but needs verification), low (suspicion, investigate further)
- **This is a broad sweep, not a targeted trace.**

**Analytical standards:** Support claims with concrete evidence. State confidence (high/medium/low) for key assertions. Note caveats and limitations.

**Structure your response as:**
```
<COUNCIL_MEMBER_RESPONSE>
## Finding 1: [Title]
- **Severity**: critical/high/medium/low
- **Location**: [specific reference — e.g. component, section, endpoint, rule]
- **Confidence**: high/medium/low
- **Issue**: [what is wrong and why it matters]
- **Evidence**: [concrete reference, snippet, or observation that proves the issue]
- **Suggested Fix**: [actionable recommendation]

## Finding 2: [Title]
...

## Summary
[Total findings by severity. Overall risk assessment with confidence levels.]
</COUNCIL_MEMBER_RESPONSE>
```

## Analysis Question

You are an adversarial reviewer performing a v3 RE-GATE of the subc federation design. Your job is to VERIFY that v3's deltas actually close the prior v2 re-gate findings, and to HUNT NEW gaps introduced by the deltas themselves. Be skeptical, precise, and source-grounded. Do NOT rubber-stamp.

## READ FIRST (in full)
The design doc: `docs/subc-federation-design.md` (v3). Read the whole file. Pay special attention to §2.6 (two subc-core primitives P1/P2), §5.3 (TOFU), §5.4 (identity split), §6.1 (at-most-once mechanics), §6.2 (partition/liveness).

## SOURCE GROUNDING (already verified by the orchestrator — you may re-open any file to confirm; cite file:line where you rely on source)
Key subc-core facts relevant to the primitives:

1. **Concurrency / flow-control coupling (critical for P1).**
   - `Registry::register_with_control_ops` (crates/subc-core/src/registry.rs:65-89): rejects duplicate `module_id` (line 74-75); stores `control_ops` taken from the HELLO; bumps `generation`.
   - control.rs:584 — `control_ops` are computed from the HELLO's advertised ops (`effective_module_control_ops(hello.control_ops)`), i.e. control_ops are a HELLO-time property.
   - control.rs:619-625 — at registration, `concurrency = manifest_concurrency(&registration.manifest)` is read from the manifest and passed into `forwarding.register_module_connection(...)`.
   - forwarding.rs:271-308 — `register_module_connection` takes `concurrency: Concurrency`, and (line 280-282) EVICTS any prior endpoint for that connection_id, then (line 284) bumps `next_generation`, creating a NEW `ModuleEndpointId { connection_id, generation }`. All existing route bindings key on the OLD `ModuleEndpointId` (forwarding.rs:41-46, 55). So re-registering a connection orphans its existing route bindings.
   - forwarding.rs:18-22 — the per-channel request-credit WINDOW is derived from concurrency: `DEFAULT_MODULE_MANAGED_WINDOW = 32` (ModuleManaged), `STATELESS_PARALLEL_WINDOW = 1024` (StatelessParallel). So concurrency in the manifest DIRECTLY sets the flow-control window. control.rs:1852-1862 `manifest_concurrency` — only `ToolProvider` carries explicit concurrency; other roles fall back to ModuleManaged (32).
   - IMPLICATION TO SCRUTINIZE: P1 `catalog.update` proposes to REPLACE a module's manifest IN PLACE without re-registering the connection. But concurrency AND control_ops are captured at register time from the manifest/HELLO. Does P1 change them? If the replacement manifest changes concurrency, does the live flow-control window change, and is that safe on in-flight routes with outstanding credits? If it does NOT change them, is the catalog now inconsistent with the flow window? Can catalog.update change advertised control_ops (which the health prober reads)? Is there any atomic transition that guarantees a route is NEVER bound to a tool the registry no longer knows?

2. **Reserved-nonce squatting protection (critical for P2).**
   - supervise.rs:344-395 — `SupervisorHandle` holds `reserved_nonces: HashMap<String,String>` (EXACT module_id → nonce) and `spawn_nonces`. `reserved_hello_authorized(module_id, presented)` (line 384-395): if there is NO reserved_nonces entry for the EXACT module_id, returns `true` (unconditionally authorized). Only an exact-id match triggers nonce checking.
   - IMPLICATION: P2 wants to extend this from exact ids to PREFIXES (e.g. reserve `fed:`). The gate today is a HashMap exact lookup returning `true` on miss. Prefix matching is a real semantics change. SCRUTINIZE: collision semantics between an exact reserved id and a reserved prefix (does `fed:` prefix-reserve also cover an exact id `fed`? what about `fedx:`?); what stops the fed-module ITSELF from registering `fed:<peerA>:tool` when policy meant a different module owns peerA; how is "connection owned by that module's attested process" KEYED, given the fed-module opens N per-peer loopback connections all from ONE attested process (one launch nonce) — is ownership keyed by nonce, by pid, by connection? Can a co-resident local key-holder open a connection and present the fed-module's launch nonce (is the nonce a shared secret visible to other local processes)?

3. **At-most-once client taxonomy (critical for §6.1).**
   - crates/subc-client-rs/src/consumer.rs:581-593 — `CallError` variants are EXACTLY: `NotSent` (request not accepted by writer path / route.open failed before data send), `OutcomeUnknown` (accepted by writer path but no terminal response observed), `Module(ErrorBody)` (handler error frame), `SubscriptionBackpressure`. Real-daemon tests (crates/subc-client-rs/tests/real_daemon.rs:294-303, 421-422, 524-525, 801) assert: accepted mid-call → OutcomeUnknown; OutcomeUnknown is NEVER auto-retried; bounded target absence → NotSent; route-gone after accept → OutcomeUnknown.
   - IMPLICATION: §6.1 says the fed-module reports `accepted` to the local consumer only after `intent` is durable, and an ambiguous WAN outcome surfaces as `OutcomeUnknown`. SCRUTINIZE: is "accepted-after-intent-durable" compatible with the existing NotSent/OutcomeUnknown taxonomy at the ORIGIN consumer (which is a plain subc client that only knows these 4 variants)? Once the fed-module has reported `accepted`/written the body, the origin client can ONLY see OutcomeUnknown on ambiguity — is that the intended and safe classification, and does anything the origin consumer does with OutcomeUnknown (never retry) compose correctly with the fed-module's own dedup/re-send? Also: llm-runner (the cited intent-log precedent) is NOT in this repo — treat that precedent as unverified/external.

4. Client taxonomy has no separate "durably-recorded-remote-intent" state; the fed-module must MAP its richer internal state machine onto these 4 variants for the origin consumer.

## PRIORITY FOCUS AREAS (address each explicitly)
1. **P1 catalog.update**: safe interaction with in-flight routes; the flow-control window when the replacement changes concurrency; route bindings to REMOVED tools; the catalog generation/staleness model; the health prober's advertised control_ops (can catalog.update change control_ops?). Is the daemon-side state transition atomic enough to NEVER leave a route bound to a tool the registry no longer knows?
2. **P2 prefix reservation**: collision semantics between exact reserved ids and prefixes; what stops the fed-module registering an id under a DIFFERENT module's reserved prefix; interaction with the connection-ownership check across the fed-module's N per-peer connections (all owned by one attested process — how is ownership keyed? is the nonce a shared secret?).
3. **§6.1 mechanics**: is the serving-side dedup ledger retention window soundly definable? What happens when the ledger row is evicted but the origin legitimately re-sends? Cross-restart durability of the monotonic seq in effect_id (a reset seq after origin db loss would collide with prior effect_ids)? Is reporting accepted-after-intent-durable compatible with the existing NotSent/OutcomeUnknown taxonomy at the ORIGIN consumer?
4. **Identity split §5.4**: is the profile-authored local BindIdentity sufficient for AFT-class providers that validate harness against an allowlist? (A `fed:<peer>` harness marker would be rejected by AFT today if AFT allowlists harness values — does the design need a harness-registration story? Check whether AFT/providers validate harness/BindIdentity against an allowlist.)
5. **TOFU §5.3**: first-contact window (TOFU trusts the first key blindly — what if the cloud is malicious at FIRST contact, before any pin exists?); key rotation legitimate-vs-attack disambiguation (how does a user tell a legit key rotation from an attack-substitution? both present as "changed key"); what the verification code binds EXACTLY (does it bind the session, the directory entry, both endpoints' long-term keys?).

Also sanity-check ANYTHING else that strikes you as unsound (e.g. §6.2 partition classification, §6.4 ClientHello device-identity transport addition, §6.5 cross-version negotiation, the one-connection-per-peer multi-registration claim vs forwarding.rs eviction behavior, relay DoS).

## OUTPUT REQUIREMENTS
- For EACH finding: give it a short title, a VERDICT of **BLOCKER** / **SHOULD-FIX** / **NOTE**, the evidence (cite file:line where you rely on source), WHY it matters, and a CONCRETE resolution proposal.
- Explicitly state, per v2-re-gate delta (P1, P2, §6.1, §5.4, §5.3), whether the v3 delta ACTUALLY CLOSES the prior finding or leaves residue.
- Separate "the delta closes the old finding" from "the delta introduces a NEW gap."
- END with an overall verdict: **GO** / **GO-WITH-CHANGES** / **NO-GO** for proceeding to phase 0 (building the two subc-core primitives P1 and P2), with a one-paragraph justification. If GO-WITH-CHANGES, list the must-fix-before-phase-0 items vs the can-fix-during items.
- Be concrete. Prefer "this specific line/mechanism is unsound because X; fix by Y" over generalities. Distinguish genuine BLOCKERs from nitpicks — calibrate severity honestly.

[assistant] 2026-07-05T10:12:14Z
The user wants me to perform an adversarial review (v3 RE-GATE) of the subc federation design. I need to:

1. Read the full design doc: docs/subc-federation-design.md
2. Verify against subc-core source code for P1, P2, at-most-once, identity, TOFU
3. Produce findings with BLOCKER/SHOULD-FIX/NOTE verdicts
4. Assess whether v3 deltas close v2 findings or introduce new gaps
5. End with GO / GO-WITH-CHANGES / NO-GO verdict

Let me start by reading the design doc and relevant source files in parallel.
Reading the v3 design doc and verifying it against subc-core and subc-client sources.

[assistant] 2026-07-05T10:12:17Z
I need more deep dives:
1. catalog.update semantics - does it exist? (probably not yet - it's proposed P1)
2. manifest_concurrency, control ops at register time
3. supervise reserved_hello_authorized full logic
4. BindIdentity / harness / AFT allowlist
5. Fork Cat inconsistency in §8 vs §2.6
6. route bindings on eviction
7. health prober control_ops

Let me read more files.

[assistant] 2026-07-05T10:12:19Z
Let me read more source files for P1/P2 details - supervise.rs reserved_hello_authorized, control registration path, forwarding route bindings, AFT harness validation, and whether catalog.update exists.

[assistant] 2026-07-05T10:12:21Z
I need more details on:
1. P1 - is catalog.update specified anywhere in code or only design? What about generation, route bindings
2. forwarding route bindings and removed tools
3. MCP harness validation
4. registry generation bump on register vs update
5. health prober control_ops
6. §8 fork Cat contradiction with P1

Let me search for registry generation, route goodbye, remove tools, and read subc-mcp harness validation

[assistant] 2026-07-05T10:12:24Z
I need more on:
- AFT harness validation
- How connection ownership could work for P2
- Registry deregister and route handling
- target_has_required_role / catalog lookup
- consumer accepted semantics

Let me search AFT for harness validation and read registry removal, route open path

[assistant] 2026-07-05T10:12:26Z
Let me read fake-aft-stub for harness validation, health prober, launch_nonce injection, and how multiple modules per connection works

[assistant] 2026-07-05T10:12:31Z
Key findings:
- harness is cosmetic per subc-principal.md for AFT trust - so fed:<peer> might NOT be a blocker for AFT allowlist - but design says profile maps to fed:<peer> harness marker - need to check if AFT or tools use harness for policy
- health prober reads control_ops from registry - if catalog.update doesn't update control_ops, health probing wrong module capabilities
- Registry has no update method - only register and deregister
- spawn_nonces vs reserved_nonces - P2 needs connection ownership - likely spawn_nonce on connection, but HELLO on peer connections uses fed module's nonce from one process - all N connections same process same nonce when registering fed:peerX:tool modules

For P2: federation module opens N loopback connections - each HELLO registers different module_id under fed: prefix. Each needs:
- prefix reservation check
- connection owned by fed module process

Today: each HELLO is on a connection_id. spawn_nonces keyed by module_id of the HELLO manifest - not by connection. When fed module registers `fed:peerA:mcp`, the launch_nonce in HELLO is the fed-module's spawn nonce.

Another local process with vault key could open loopback and HELLO as `fed:peerA:evil` - without P2, passes reserved_hello_authorized (no exact match for fed:peerA:evil in reserved_nonces if only `federation` module id is reserved).

P2 says reserve prefix `fed:` for named spawn-attested module - the federation module's config module_id might be `federation` not `fed:`.

Connection ownership: "unless it arrives on a connection owned by that module's attested process" - need to tie connection to spawn nonce. Is launch nonce secret? SUBC_LAUNCH_NONCE_ENV in child only - but local attacker with connection file could be different threat model - same-host key holder can read connection files and impersonate clients; for HELLO they need to present nonce for reserved ids. For non-reserved they don't need nonce.

Squatter with key: can register any non-reserved module_id including fed:peerA:x without P2.

For P2 implementation gap: spawn_nonces is per module_id of the registering HELLO, not per reserved prefix owner. Fed module registers many module_ids on many connections - each HELLO uses same launch nonce from federation process.

Co-resident key-holder: could they read SUBC_LAUNCH_NONCE from federation process env? Same-host threat model includes local key holder - typically they have user's keys. If they can spawn their own process, they don't get inject nonce unless daemon spawns them. If they attach to loopback with stolen connection file - consumer path. For provider HELLO they open new connection to subc - need module connection file from federation module or create own connection.

Actually federation module holds module connections per peer - each is daemon-supervised spawn? One federation process, multiple TCP connections to subc - same process, same launch nonce on each HELLO?

When one process opens multiple module connections, each HELLO has same launch_nonce. P2 connection ownership = connection_id must be registered only by connections from process that owns the prefix reservation.

Need: map connection_id -> authorized prefix at first HELLO from that connection?

Also exact vs prefix: module_id `fed` vs prefix `fed:` - if both exist, longest match rules undefined in doc.

§8 Fork Cat still says "v1 is coarse re-HELLO per peer" contradicting §2.5 P1 - internal doc inconsistency.

catalog.update spec gaps:
- Doesn't say update ModuleRegistration.control_ops
- Doesn't say update forwarding ModuleConnection.concurrency / credit window
- Doesn't say atomicity with route.open validation
- Removed tools: "normal route-GOODBYE" - who triggers? proactive scan on catalog update?

Read handle_route_open for registry check

[assistant] 2026-07-05T10:12:42Z
Important finding: `SUBC_LAUNCH_NONCE_ENV` is in the child process environment - any co-resident process that can read `/proc/<pid>/environ` or the fed-module's memory could get it. Same-host threat model - key holders can already do a lot.

For P2 connection ownership: design says "connection owned by that module's attested process" - need to check if there's connection→pid mapping. ConnectionId is assigned at accept time - likely not tied to nonce at connection level for HELLO except the nonce in HELLO frame.

Multiple module_ids on ONE connection: can one connection register multiple modules? registry is module_id keyed; duplicate on same connection? Only one module_id per HELLO per connection typically. Federation uses one connection per peer with one module_id per peer catalog.

P2 prefix on fed:<peer>:<module> - each HELLO has full module_id. Prefix reserve fed: means all ids starting with fed: need fed-module's nonce.

Collision: exact reserve for module "fed" vs prefix "fed:" - if module_id is "fed" exact match in HashMap; prefix "fed:" might match "fed:xxx" but not "fed" unless prefix rules say trailing colon required.

What about "fed" without colon squatting? 

Spawn nonces are per supervised module_id (fed-module's config id e.g. "federation") not per peer registration id.

HELLO for fed:peerB:tool presents launch_nonce from federation process - good.

Another local module spawned by user with stolen nonce - if they can read env from federation child they can impersonate.

handle_route_open checks registration.manifest for role - tool in manifest. If catalog.update removes tool from manifest but route still bound - need GOODBYE logic.

Search for stale route / manifest tool validation on data path

[assistant] 2026-07-05T10:12:48Z
Key findings for route.open after catalog update - it checks registry manifest for role at open time. If tool removed from manifest, new route.open would fail at target_has_required_role. But existing bindings have module_id and route_channel - need to see if removed tools still get GOODBYE.

Doc says "routes to tools that vanish get the normal route-GOODBYE" - P1 spec doesn't exist in code yet so this is a design requirement that needs to be specified in P1 impl.

P1 gaps:
- No in-place update API exists
- control_ops frozen at HELLO (registry.rs:83, control.rs:584-589)
- concurrency frozen at register_module_connection (control.rs:619-625, forwarding.rs:276-304)
- Registry generation bumps on register AND close - P1 needs to bump on update - staleness consumers need to watch
- Health prober uses control_ops from registration - catalog.list returns registration.control_ops (control.rs:754)
- If concurrency changes in manifest but not in ModuleConnection - inconsistency and wrong credit window

For multi-registration per connection - doc says one connection per peer with multiple module_ids? Actually "one loopback connection per peer, each HELLO-registering that peer's namespaced catalog" - wait, one connection can only register ONE module_id per HELLO because duplicate rejects. So they must mean: multiple module ids on same connection? That can't work with current registry - one HELLO = one module_id per connection.

Re-read §2.5: "one loopback connection to the local subc per remote peer, each HELLO-registering that peer's namespaced catalog"

Actually catalog might be ONE module_id per peer connection with many tools in manifest, not many module_ids. §4.1: "re-registers it into A's local subc under the fed:<B-pubkey-fingerprint>: namespace" - tools namespaced in catalog, likely one provider module_id per peer like `fed:abc123:...` or multiple tool entries under one module.

§3.1: "one loopback connection per peer" that re-exports peer's exposed catalog - probably one ModuleManifest per peer connection with tools prefixed in names.

P2: connection ownership - spawn_nonces keyed by configured module_id (fed module), not per connection. All loopback connections from fed-module process share same SUBC_LAUNCH_NONCE env var on spawn - actually each connection is same process so same nonce. Prefix reservation must check connection_id maps to process that owns the reserved prefix - need connection_id -> spawning module_id mapping.

Can another local process open loopback and present fed-module's nonce? Only if they read SUBC_LAUNCH_NONCE from fed-module's environment - secret in process env, other users on same host might read via /proc on Linux. Same-host threat model may allow key-holder already.

co-resident key-holder registering fed:peer:tool without nonce - P2 should reject. Key-holder with disk key can open consumer connection but HELLO as provider needs launch_nonce for reserved exact id; for prefix need extension.

Exact vs prefix: reserved_hello_authorized only exact match. For `fed:peer:mod` id, prefix `fed:` would need new check before or instead of exact.

Collision: exact reserve `fed` vs prefix `fed:` - must define longest match or exact takes precedence.

§6.1:
- accepted after intent durable - origin consumer is calling through fed-module as provider. When does consumer see NotSent vs OutcomeUnknown? Fed-module is the "module" from subc's view on origin - the client's writer path acceptance is when subc accepts request on route to fed-module. Design says fed reports accepted to local consumer only after intent fsync - that implies fed-module delays acknowledging at subc level OR uses internal semantics. Actually "reports accepted to the local consumer" might mean the point at which CallError taxonomy splits - if that's after intent, then route.open and first body byte accepted by subc → OutcomeUnknown territory. Need to check when consumer classifies NotSent vs OutcomeUnknown.

consumer.rs - classification happens on writer path acceptance.

If origin agent calls tool on `home:foo` via fed provider on VPS, flow is: consumer opens route to fed-module, sends body - when is that "accepted"? If intent fsync happens BEFORE network send but AFTER subc accepted the frame, we're in OutcomeUnknown on disconnect - design aligns. If intent fsync is BEFORE subc accepts from client perspective - might need fed-module to not read body until intent done - complex.

Seq reset: effect_id (origin_device_pubkey, monotonic_seq) - if origin db lost, seq resets → duplicate effect_ids possible → serving dedup could return wrong cached outcome - BLOCKER unless seq is durable cross-restart with fencing token.

Dedup ledger eviction before origin retry → double execution - need window spec tied to max retry horizon.

§5.4 harness fed:<peer> - subc-principal says harness cosmetic for AFT trust. But cache tiers and storage key off harness - fed:peer would partition cache/WAL. MCP validates harness for mcp class. Need check AFT route.bind for harness allowlist in aft module repo - not in subconscious maybe. Design says profile-authored harness marker - if AFT doesn't allowlist, forced-restrict or config_divergence?

Search aft module for harness validation

[assistant] 2026-07-05T10:13:04Z
Let me read consumer.rs around accepted/writer path for route.open timing
And check ToolProvider target - does it check tool name in manifest?

[assistant] 2026-07-05T10:13:06Z
Need to understand multi module per connection - registry allows one module_id per registration but can multiple modules register on same connection_id?

From registry deregister - filter by connection_id - so multiple modules CAN be on same connection.

But register_module_connection EVICTS per connection_id - only ONE module endpoint per connection!

So one connection = one forwarding module endpoint, but registry can have multiple module_ids with same connection_id?

Let me read register flow again - each HELLO calls register_module_connection with same connection_id - that EVICTS old endpoint!

So second HELLO on same connection would evict first module's forwarding state - disaster for multi-registration per connection.

v3 says one loopback connection per peer with ONE peer catalog per connection - each HELLO is one module_id per connection. Good.

For fed module with N peers = N connections each with one namespaced module_id. Good.

P2: each connection owned by fed-module process - all N connections from same process, same launch nonce in env when process started once - but each connection is separate TCP to subc - does each HELLO present same launch_nonce? Yes from same env var in fed process.

Connection ownership: need connection_id -> owning supervised module. Today only spawn_nonces keyed by module_id for consumer identity, not per connection for HELLO.

P2 says "connection owned by that module's attested process" - need mapping connection_id to process that opened it - likely first frame on connection proves nonce.

Key-holder could connect as client and HELLO with fed:peerA:tool if they steal nonce from /proc/environ of fed module - local attacker model.

Also key-holder could register fed:peerA without prefix protection today.

Prefix collision: exact reserve "fed" vs prefix "fed:" - id "fed" doesn't match prefix "fed:" unless prefix rule is careless.

Id "fedx:evil" shouldn't match "fed:" prefix if longest-prefix or boundary rule is `fed:` only.

§8 Fork Cat still says coarse re-HELLO - contradicts v3 §2.5 - internal doc inconsistency.

For P1 - need spec for removed tools: target_has_required_role only checks module has ToolProvider role, NOT specific tool name in manifest. So route stays bound to module after catalog update removes tool - provider may reject at bind or runtime?

Route already committed - catalog update doesn't re-validate tool in manifest for existing routes.

Finding: P1 "routes to tools that vanish get route-GOODBYE" - not automatic from registry alone - needs explicit mechanism in P1 spec.

Health prober - control_ops from registration at HELLO; catalog.update if doesn't update registry.control_ops, health probe advertising catalog.update might be stale.

effect_id monotonic_seq - origin db loss resets seq -> collision if serving ledger still has old effect_ids - BLOCKER for §6.1

Dedup ledger eviction before origin retry -> duplicate mutation - need tied retention formula

accepted after intent durable -> origin consumer sees OutcomeUnknown only after accepting writer path - fed-module is provider on loopback side - the VPS agent calls fed:home:tool - the consumer is agent, fed-module is provider. Wait path is Agent -> subc -> fed-module (provider leg). The "accepted" is on consumer to subc when body enters writer. Fed-module receives as provider.

Actually §6.1 says origin fed-module mints effect_id - on VPS calling home tool, VPS fed-module is origin. Local consumer on VPS is agent. Fed-module reports accepted to local consumer after intent fsync - so agent's call is "accepted" (writer path) only after intent durable. That maps to OutcomeUnknown on ambiguity - good.

But if intent durable before WAN send, consumer classification is "accepted" - any failure = OutcomeUnknown, never retry - correct for mutating.

Serving side: home fed-module receives WAN, dedup, calls local tool.

Lost-ack: origin retries, serving dedup returns recorded outcome - good IF ledger retained.

§6.2 silent drop - still depends on partition handling - design says GOODBYE-on-partition - need verification subc does this for WAN path bridged through fed module.

TOFU first contact - malicious cloud at first pairing - no pin yet - BLOCKER for cloud path unless manual pairing only for first trust.

Verification code - what exactly is hashed - design doesn't pin algorithm/bindings - SHOULD-FIX

Identity §5.4 fed:<peer> harness - mcp validates harness; AFT docs say harness cosmetic - but MC cache tiers key on harness - federation cross-peer may need profile-specified harness allowlist extension for AFT if any validation exists at bind.

Search aft-bridge for harness validation

[assistant] 2026-07-05T10:13:18Z
Let me check if multiple modules can register on same connection - registry allows one module per module_id, but can same connection register multiple different module_ids? Each HELLO register_once - duplicate module_id rejected but different module_ids on same connection?

Actually each connection can only do one HELLO as module? Let me check - if federation opens N connections that's fine. The design says one connection per peer with one module_id per connection.

For P2: prefix on `fed:` - HELLO for `fed:peer:module` - need prefix match reserved for fed-module AND connection ownership.

spawn_nonces keyed by module_id of the SPAWNED module (e.g. "federation" or "subc-federation"), not by the HELLO module_id `fed:...`.

So P2 must check: presenting launch_nonce matches spawn nonce for the reserved PREFIX OWNER module_id from config, AND connection_id matches the connection that registered that owner OR any connection owned by same process?

Actually design says "connection owned by that module's attested process" - all N loopback connections from fed-module process share ONE launch nonce in env var when spawned once. Each connection might be separate TCP to subc - does each connection get same nonce from env? Yes, same process reads same SUBC_LAUNCH_NONCE.

But spawn_nonces is keyed by supervised module_id (fed module's config id), not per-connection. HELLO for `fed:abc:tool` would need to verify nonce against federation module's spawn nonce AND verify connection is from federation module's process.

How does subc know connection is owned by fed process? Today: only via matching launch_nonce on HELLO for exact reserved id. For prefix ids, they'd present same launch_nonce - but ANY process with the nonce can register. Nonce is in child env only - but can other local processes read /proc/environ on Linux? Yes - local attacker with same uid can read environment of sibling processes. So launch nonce is NOT a secret from co-resident same-user processes.

That's a BLOCKER for P2 if threat model is same-host key-holder - they already have key. The v2 finding was key-holder could HELLO without being fed module. Key-holder doesn't get spawn nonce unless they spawn their own module. They could try to guess nonce - 256 bit. 

But co-resident process that CAN read fed-module's environ gets the nonce and can open connection presenting fed:peer:tool. That's a real gap.

Document should say: connection ownership = TLS/UDS peer creds or only connections from PID that supervisor registered? Need to check if subc tracks peer pid.

Also §8 line 214 contradicts §2.5/§4.1 on Fork Cat - internal inconsistency NOTE.

For tool removal: route.open checks manifest roles at open time but not specific tool name in ToolProvider target - need RouteTarget structure.

[assistant] 2026-07-05T10:13:26Z
For §6.1 - the fed-module is itself a subc module/provider. When an agent calls `fed:peer:tool`, the consumer is likely a module consumer or direct client. The fed-module receives via route.bind on its provider connection. The question is: does the fed-module's acceptance to its upstream local client happen only after intent durable?

The design says fed-module reports accepted only after intent durable. The origin consumer taxonomy: once body is accepted on writer path → OutcomeUnknown on ambiguity. That's correct mapping.

Issue: if fed-module delays accepting the route/body until after intent fsync, does that keep the call in NotSent until intent durable? That would be GOOD - remote leg not started before durable intent, and local client hasn't "accepted" yet.

But if the design means: route.open succeeds and body is written to fed-module, THEN fed-module fsyncs intent - the consumer has already "accepted" the body at that point (writer path accepted). So ordering must be: fed-module must not read/accept the request bytes from subc until... actually the flow is consumer sends body on route after route.open. The acceptance is when body is enqueued to writer.

So the fed-module needs to either:
1. Not complete route handling until intent durable (might mean subc hasn't forwarded body yet - complex)
2. Or accept body but not forward to WAN until intent durable - local consumer already accepted

The design says "reports accepted to local consumer only after intent durable" - need to check if that means route.open + body send from consumer perspective. Likely the fed-module is the handler - consumer's "accepted" is when subc accepted the frame toward fed-module. So there's a gap: design language vs actual taxonomy where acceptance is writer-path to module, not "fed-module ack".

I'll flag: §6.1 barrier wording may not align with subc-client's definition of "accepted" (consumer.rs:582-584) unless fed-module uses a two-phase pattern or consumer waits on streaming.

Also lost-ack: origin NotSent retry with dedup ledger - good if horizon matches.

effect_id seq reset after db loss - collision risk BLOCKER

Dedup ledger eviction - need explicit horizon tied to origin retry policy

For §8 Fork Cat contradiction - doc line 214 says "v1 is coarse re-HELLO" but v3 says P1 - internal inconsistency SHOULD-FIX

§5.4 - subc-mcp validates harness allowlist for MCP facade; AFT docs say harness cosmetic. Design proposes fed:<peer> - if any provider validates harness string (subc-mcp does for shim path), federation needs profile mapping to allowed harness tokens OR AFT/module registration story. docs/subc-principal says harness cosmetic for AFT trust - but subc-mcp main.rs:1150-1156 validates harness for MCP binds to tools through facade. Cross-machine calls go direct to tools with profile-authored bind - AFT may reject unknown harness in route.bind if it validates.

Search AFT route.bind harness validation in fake-aft or docs

[assistant] 2026-07-05T10:13:35Z
RouteTarget::ToolProvider only has module_id - tool selection is in the body. So when catalog removes a tool, route.open might still succeed if module provides ToolProvider role - the specific tool failure happens at module handler level.

For P1 "routes to tools that vanish get route-GOODBYE" - the design claims this but registry only stores manifest at module level - tool list is in manifest. Need to check if subc does per-tool route validation at open time or only module level.

handle_route_open only checks target_has_required_role for ToolProvider generically, not specific tool name.

So if catalog.update removes a tool from manifest, existing routes bound to module_id still work - the module might reject unknown tool on invoke. Design says "route-GOODBYE" for removed tools - that would need NEW behavior in P1 not existing in codebase.

Important finding: P1 spec overclaims vs current subc behavior which only validates module_id at route.open, not per-tool.

For federation, namespaced module per peer means route target is `fed:peer:module` - tool in body.

Let me verify tool routing in manifest tools list.

[assistant] 2026-07-05T10:13:39Z
I have enough to write a thorough audit. Let me also quickly check if multiple modules per connection is tested in forwarding tests.

[assistant] 2026-07-05T10:13:42Z
Important: One connection cannot be both consumer and module (invalid_hello). So federation module with N peers = N loopback connections, each with one HELLO one module_id.

For P2: each connection from fed module process - they share SUBC_LAUNCH_NONCE_ENV from their single process... Actually each connection is from same process so same nonce in env when they connect. Prefix reservation on connection ownership keyed by connection_id + verifying launch_nonce on HELLO for each fed:peer:module id.

Key insight from prompt: "Can a co-resident local key-holder open a connection and present the fed-module's launch nonce" - the nonce is in SUBC_LAUNCH_NONCE_ENV only for the spawned child process, not readable by arbitrary key-holders unless they can read the child's environ or intercept. A malicious local client with connection file could HELLO as fed:fake:tool WITHOUT nonce if NOT reserved - with P2 prefix reserved, HELLO for fed:* needs connection owned by fed module - connection ownership means the TCP/UDS connection was accepted from the fed module's process... Need to verify how connection is attributed to process.

Actually launch nonce is secret to spawned process - key holder with own connection doesn't have nonce unless they stole from fed module process memory/env.

For prefix collision: module_id "fed" exact reserve vs prefix "fed:" - longest match rules need spec.

Fork Cat inconsistency is a doc bug BLOCKER for phase 0 clarity.

P1 unspecified API - need to list what must be in primitive spec.

Tool-level routing: RouteTarget only has module_id - multiple tools same module - if tool removed from manifest, does route still work? Provider receives tool name in body probably - subc doesn't validate tool in manifest at route.open time necessarily.

Let me check route.open for tool validation

[assistant] 2026-07-05T10:13:47Z
One more thing: route.open only checks target_has_required_role at module level - NOT individual tool names in manifest. So if catalog.update removes a tool from manifest but route is still bound to module_id, calls to removed tools might still route to module and fail at module handler - not necessarily route-GOODBYE until something triggers refresh.

Design says "routes to tools that vanish get the normal route-GOODBYE" - need mechanism. Possibly P1 must enumerate removed tools and send GOODBYE per route channel targeting that tool - but route binding stores module_id not tool name. So how does subc know which routes are for removed tools?

RouteTarget is ToolProvider { module_id } only - tool might be in the body not in binding. So catalog.update removing a tool from manifest does NOT automatically invalidate routes - routes are per-module not per-tool. This is a significant gap unless tools are separate route targets (they're not - same module_id for all tools on that provider).

Actually for ToolProvider, the target is module_id only; the tool name is in the request body. So "route-GOODBYE for removed tools" can't be per-tool at forwarding layer without parsing bodies (violates opaque body). The module must reject unknown tools. Design claim "routes to tools that vanish get route-GOODBYE" may be wrong - they'd get module errors or stale behavior until catalog generation checked elsewhere.

This is a BLOCKER for P1 semantics.

Also health probe - uses registration.control_ops from registry - catalog.update would need to update control_ops if health advertised in manifest - design silent.

Let me also verify: one connection per peer with multiple module_ids - registry allows multiple module_ids per connection_id (deregister removes all for connection). register doesn't check one-module-per-connection. So multiple HELLOs on different connections each with different module_id - yes. Same connection duplicate module_id rejected. Different module_ids on SAME connection - can you HELLO twice on same connection? First HELLO registers; second HELLO with different module_id on same connection - would need second handle_control hello - is that allowed? connection already has module endpoint from first HELLO. Second registration different module_id - register_with_control_ops would add second entry with same connection_id. But register_module_connection on SAME connection_id EVICTS previous endpoint (forwarding.rs:280-282). So second HELLO on same connection breaks first module's forwarding!

That's critical for fed-module N per-peer connections - each connection one module_id. Design says one connection per peer with one namespaced catalog per peer - one module_id per connection. Good.

But "multi-registration-per-process" in phase 1 - one process opens N loopback connections (N sockets), each HELLO one peer module_id. Not N HELLOs on one connection.

I'll write the full response now.
<COUNCIL_MEMBER_RESPONSE>

## Finding 1: P1 `catalog.update` is unspecified for concurrency, `control_ops`, and removed-tool routes
- **Severity**: critical  
- **Location**: 2.6 P1; `crates/subc-core/src/control.rs:619-626`, `584-589`; `crates/subc-core/src/forwarding.rs:271-306`, `18-22`; `crates/subc-protocol/src/lib.rs:441-443`  
- **Confidence**: high  
- **Issue**: v3 withdraws evict-and-re-HELLO but does not define how P1 updates state that today is fixed at HELLO. `register_module_connection` sets `concurrency` from the manifest once (`control.rs:619-625`); the credit window is derived from that (`forwarding.rs:18-22`, default 32 vs 1024). `control_ops` are taken from HELLO (`control.rs:584-589`) and stored on `ModuleRegistration` (`registry.rs:83`). `catalog.list` exposes those `control_ops` (`control.rs:754`). If P1 replaces the manifest in the registry but leaves the forwarding `ModuleConnection` unchanged, a catalog that changes `ToolProvider` concurrency leaves **live flow-control inconsistent with the advertised catalog**. If P1 also bumps forwarding concurrency on live routes, **in-flight credits** may violate the new window with no transition spec. Separately, `RouteTarget::ToolProvider` is **module_id only** (no tool name in the route key); bindings store `module_id` (`forwarding.rs:54-55`). `handle_route_open` checks module-level role only (`control.rs:836-841`, `1780-1792`), not manifest tool names. So “removed tools get route-GOODBYE” cannot be implemented at the forwarding layer without parsing opaque bodies; stale routes to a live module can keep calling until the provider rejects in-body.  
- **Evidence**: HELLO path couples manifest → registry + forwarding; tool routing is per-`module_id`; no `catalog.update` exists in tree.  
- **Suggested Fix**: Normative P1 spec: (1) whether `concurrency` / `control_ops` may change via P1; if yes, define atomic credit reconciliation or forbid changes while routes exist; if no, reject P1 payloads that differ from HELLO-time values. (2) For removed tools: either document module-side rejection + catalog generation on `route.open`, or add a tool-aware invalidation mechanism (without body parsing at subc-core). (3) Single atomic transition: registry generation bump + optional selective drain before exposing new catalog.

**v2 delta (P1):** Closes “re-HELLO kills all in-flight calls on every catalog change” **only if** P1 is implemented as claimed; **residue + NEW gaps**: concurrency/`control_ops`/removed-tool semantics are still open — prior finding partially closed, implementation spec is not.

---

## Finding 2: P2 prefix reservation + “connection owned by attested process” is underspecified and conflicts with launch-nonce exposure
- **Severity**: critical  
- **Location**: 2.6 P2, 5.2; `crates/subc-core/src/supervise.rs:384-395`, `2026-2033`, `401-411`; `crates/subc-core/src/control.rs:556-568`  
- **Confidence**: high  
- **Issue**: Today `reserved_hello_authorized` is **exact** `module_id` → nonce map; miss ⇒ authorized (`supervise.rs:389-390`). P2 needs longest-prefix vs exact-id precedence (`fed` vs `fed:` vs `fedx:`), and whether an exact reserved id shadows a prefix. “Connection owned by that module’s attested process” is not keyed in subc-core: HELLO only checks nonce for **reserved exact id** (`control.rs:556-568`), not `connection_id` ↔ spawn record. The fed-module opens **N loopback connections** (one per peer); all share one supervised process and one `SUBC_LAUNCH_NONCE_ENV` (`supervise.rs:2033`). Any **local process that can read that env** (or the connection file) can present the same nonce on its own connection and HELLO-register `fed:<victim-peer>:…` unless P2 binds **connection_id** (or first HELLO on that socket) to the supervisor’s spawn nonce for the **federation module_id only**. `spawned_consumer_authorized` keys on consumer’s claimed `module_id` + nonce (`supervise.rs:401-411`), not on provider HELLO under a prefix. A co-resident attacker is not blocked by P2 as described.  
- **Evidence**: Prefix matching and per-connection ownership are absent; nonce is injected into child env (readable by same-user local attackers).  
- **Suggested Fix**: P2 design must specify: prefix match algorithm + exact-over-prefix rules; HELLO gate = `(module_id matches reserved prefix) ⇒ connection_id registered on first successful HELLO from spawn-attested fed-module AND nonce matches fed-module reserved entry`; reject prefix ids on other connections even with stolen nonce unless connection is the one bound at spawn. Consider not reusing env nonce across multiple outbound connections or use per-connection server-issued tokens.

**v2 delta (P2):** Identifies real exact-id squatting gap; v3 **does not close** it until prefix + **connection binding** semantics are specified — **residue**; **NEW gap**: stolen launch nonce + missing connection ownership.

---

## Finding 3: 6.1 “accepted after intent durable” misaligns with origin `CallError` taxonomy and client retry behavior
- **Severity**: high  
- **Location**: 6.1; `crates/subc-client-rs/src/consumer.rs:581-593`, `295-296`, `369-373`  
- **Confidence**: high  
- **Issue**: Origin consumer only has `NotSent` / `OutcomeUnknown` / `Module` / `SubscriptionBackpressure`. `NotSent` = not accepted by writer or `route.open` before body send (`consumer.rs:582-583`). If the fed-module delays “acceptance” to the origin until after WAN `intent` fsync, failures **before** that are `NotSent` (origin may retry per `consumer.rs:369-373` for `NotSent`). After intent is durable, the fed-module must forward bytes on the loopback route; that is **writer-path accepted** ⇒ ambiguity ⇒ **`OutcomeUnknown`** only (`real_daemon.rs:294-303`). That matches 6.1’s WAN `OutcomeUnknown` story **if** the fed-module is the origin’s direct subc peer. Risk: calling this “reports accepted” is misleading — the taxonomy has no “intent recorded” state; operators/agents must treat post-send ambiguity as **never auto-retry** while origin may still **legitimately** re-send mutators with the **same** `effect_id` (dedup ledger). 6.1 does not pin when the origin consumer’s `call()` returns relative to fed-module send-log barriers (response only after outcome fsync is stated; intermediate states are not).  
- **Evidence**: Four-variant enum; auto-retry on `NotSent` only.  
- **Suggested Fix**: Map explicit fed-module states → consumer-visible outcomes in 6.1: pre-intent failures = `NotSent`; post-intent without terminal response = `OutcomeUnknown`; document that origin **must not** auto-retry `OutcomeUnknown` but **may** issue a new call with same `effect_id` per federation policy. Add phase-0 test vectors mirroring `real_daemon.rs`.

**v2 delta (6.1):** Closes “no mechanics” at design level; **residue**: barrier ↔ `CallError` mapping and origin retry interaction; llm-runner precedent **unverified** in-repo.

---

## Finding 4: Dedup ledger retention vs `effect_id` seq reset after origin DB loss
- **Severity**: high  
- **Location**: 6.1 (`effect_id`, dedup ledger window)  
- **Confidence**: medium  
- **Issue**: Serving ledger returns cached outcomes for re-sent `effect_id` within a “bounded retention window ≥ origin’s max legitimate re-send horizon.” If origin send-log is wiped and `monotonic_seq` resets, **new** `(pubkey, low seq)` can collide with **evicted** ledger rows (if retention < attacker/operator confusion horizon) or with **still-recorded** rows (false replay of old outcome to new intent). No spec for seq persistence (WAL), crash recovery, or “unknown effect_id after seq gap ⇒ reject / fence / operator action.”  
- **Evidence**: 6.1 lines 175-177 only; no seq durability or collision rules in doc.  
- **Suggested Fix**: Persist seq in origin store with fsync; include generation/tombstone in `effect_id`; on serving side, if seq regresses below high-water mark for that pubkey, refuse or require explicit re-pairing; ledger retention ≥ max(WAN retry horizon, origin WAL replay window) with documented numbers.

**v2 delta (6.1):** **NEW gap** (not in v2 text).

---

## Finding 5: 5.4 `fed:<peer>` harness vs provider validation — no registration story
- **Severity**: high  
- **Location**: 5.4; `crates/subc-mcp/src/main.rs:1149-1157`; `docs/subc-principal.md:103-104`  
- **Confidence**: medium  
- **Issue**: Profile-authored `BindIdentity` with `harness: "fed:<peer>"` is plausible for storage/routing, but **subc-mcp** already normalizes/validates harness tokens (`opencode|pi|runner|mcp:<client>`); bare tokens get `mcp:` prefix (`main.rs:1155-1156`). Real AFT is documented to treat harness as cosmetic for **trust** (`subc-principal.md:103-104`), but route.bind still carries harness to modules; federation design does not say whether AFT/MC/llm-runner **partition keys** or policy treat unknown harness as `config_divergence` / forced-restrict. Cross-peer tools may silently land in wrong store partition or fail closed.  
- **Evidence**: MCP shim harness rules in-tree; federation doc asserts profile harness without module contract.  
- **Suggested Fix**: 5.4 add normative harness registry: reserved `fed:<fingerprint>` namespace, module behavior (accept + audit tag vs reject), and CK profile validation before stamp.

**v2 delta (5.4):** Closes confused-deputy **conceptually**; **residue**: execution harness interoperability with existing providers.

---

## Finding 6: 5.3 TOFU does not bound first-contact malicious cloud; verification code binding unspecified
- **Severity**: medium  
- **Location**: 5.3  
- **Confidence**: high  
- **Issue**: TOFU pinning helps **subsequent** substitution; at **first** cloud-mediated introduction, user has no pin — malicious directory can MITM until OOB compare. Doc says manual pairing is immune (token carries key) but does not define what the **safety number** hashes (both long-term keys? device keys? account id? session transcript?), when it must be shown (every new peer vs only cloud path), or how CK distinguishes **legitimate rotation** (new device key) from **attack substitution** (both are “key changed — verify”).  
- **Evidence**: 5.3 lines 151-153; no cryptographic binding spec.  
- **Suggested Fix**: Pin normative fingerprint (e.g. Noise static keys both ends); require OOB compare on cloud-first pair; rotation = signed tombstone chain from old key + user confirm; document irreducible first-contact trust for cloud tier.

**v2 delta (5.3):** Closes “no mitigation named”; **residue**: first-contact window + rotation UX; **NEW gap**: exact code binding.

---

## Finding 7: Internal doc contradiction — Fork Cat still says “coarse re-HELLO” while body adopts P1
- **Severity**: medium  
- **Location**: `docs/subc-federation-design.md`  Fork Cat (line 214) vs 2.5/4.1  
- **Confidence**: high  
- **Issue**:  states “v1 is coarse re-HELLO per peer” while v3 changelog and 2.6 promote P1 as v1-blocking. Re-gate readers can implement the wrong catalog path.  
- **Suggested Fix**: Update Fork Cat to “P1 is mechanism; open item = staleness window numeric only.”

---

## Finding 8: 6.2 partition / silent-drop — design direction sound but not tied to subc contracts
- **Severity**: medium  
- **Location**: 6.2; `handle_route_open` / route-GOODBYE paths in `control.rs`  
- **Confidence**: medium  
- **Issue**: GOODBYE-on-partition reuses local contracts (reasonable), but no spec for fed-module marking peer tools unavailable vs daemon registry still showing `fed:*` modules as Active — risk of `NotSent` vs hang vs silent drop mismatch across the extra hop.  
- **Suggested Fix**: On partition, P1 or module marks peer catalog stale + rejects new `route.open` with `target_unavailable`; align with keepalive window numbers in phase 3.

---

## Finding 9: One-connection-per-peer vs same-connection `register_module_connection` eviction — design is consistent if N sockets
- **Severity**: low  
- **Location**: 2.5; `forwarding.rs:280-282`; `control.rs:2776-2795`  
- **Confidence**: high  
- **Issue**: Multiple `module_id`s on **one** connection would evict forwarding state; design’s one socket per peer avoids this. Phase 1 “multi-registration-per-process” must mean **multi-connection**, not multi-HELLO one connection.  
- **Suggested Fix**: Clarify in 2.5/ phase 1 test: N UDS connections, one HELLO each.

---

## Per v2-re-gate delta verdict (concise)

| Delta | Closes prior finding? | Residue / new gaps |
|--------|------------------------|---------------------|
| **P1** | Partially (avoids full peer reconnect churn) | Concurrency, `control_ops`, removed-tool vs route binding, atomicity |
| **P2** | Partially (names squatting attack class) | Prefix rules, connection ownership, nonce exposure |
| **6.1** | Yes for “name the machinery” | `CallError` mapping, seq/ledger retention, external llm-runner proof |
| **5.4** | Yes for confused-deputy split | `fed:` harness provider contract |
| **5.3** | Partially (TOFU + codes named) | First-contact MITM, rotation vs attack, code definition |

---

## Summary

| Severity | Count |
|----------|-------|
| critical | 2 |
| high | 3 |
| medium | 3 |
| low | 1 |

**Overall verdict: GO-WITH-CHANGES** — architecture direction is sound and v3 correctly drops the “zero subc-core change” fiction, but **phase 0 must not start** until P1 and P2 have implementable specs that address concurrency/`control_ops`/tool removal and prefix+connection binding (Findings 1–2).

**Must-fix before phase 0 (P1/P2 implementation):**
1. P1 normative state machine (registry + forwarding + `control_ops` + concurrency + removed tools).  
2. P2 prefix algorithm + connection-to-spawn binding (not nonce-alone).  
3. Resolve  Fork Cat vs P1 contradiction.

**Can-fix during phase 0 / parallel to spike:**
- 6.1 `CallError` test matrix and `effect_id`/ledger retention numbers (Finding 3–4).  
- 5.4 harness registry (Finding 5).  
- 5.3 OOB code definition and first-contact UX (Finding 6).  
- 6.2 partition catalog staleness wiring (Finding 8).

**Justification:** Source-grounded subc-core behavior shows HELLO-time coupling of flow control and duplicate-`module_id` registry semantics; v3’s P1/P2 bullets are the right primitives but current text does not yet guarantee safe in-place catalog refresh or non-squattable `fed:*` registration against a local nonce thief. Shipping primitives without those specs risks baking incorrect registry/forwarding invariants into the first federation spike.

</COUNCIL_MEMBER_RESPONSE>

