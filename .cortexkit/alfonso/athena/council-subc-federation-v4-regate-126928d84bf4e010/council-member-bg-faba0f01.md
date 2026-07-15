## Per-finding confirmation

#1: PARTIAL — 2.6 L84-L86 says “No tool-granular GOODBYE,” but 4.1 L130 still says “removed tools get route-GOODBYE” — 2.6 matches source (`forwarding.rs` L43-L60 route keys lack tool identity), but L130 reintroduces the impossible promise. Confidence: high.

#2: CLOSED — 2.6 L84-L86 says P1 replaces only `provides` and rejects `module_id`, role kind, `concurrency`, `control_ops` changes — this matches source: concurrency is captured at HELLO (`control.rs` L619; `forwarding.rs` L169-L174, L298-L305) and `control_ops` are stored/read separately (`registry.rs` L78-L83; `control.rs` L1260). Confidence: high.

#3: CLOSED — 2.6 L88-L92 defines delimiter prefixes, exact-over-prefix precedence, owner module nonce mapping, and says P2 is not a same-user barrier — this correctly addresses current exact-only authorization (`supervise.rs` L384-L395) and env nonce injection (`supervise.rs` L2023-L2033). Confidence: high.

#4: PARTIAL — 6.1 L200-L206 adds the fed-state→CallError table, especially L203 “NO intent row… emits a durable `not_sent` tombstone” — the table exists, but if no intent/effect row was durable, v4 still does not specify the durable correlation key/API by which recovery can emit/query that tombstone; current `CallError` remains only the 4 variants (`consumer.rs` L581-L593). Confidence: medium-high.

#5: CLOSED — 6.1 L197 defines `effect_id = (origin_device_pubkey, incarnation_uuid, seq)` and serving-side seq fencing — this closes DB-loss/restart collision by adding a durable incarnation epoch. Confidence: high.

#6: CLOSED — 6.1 L199 co-defines ledger retention with origin outcome-received confirmation plus grace and makes post-expiry re-arrival a typed ambiguity refusal — this removes the circular “max resend horizon” definition. Confidence: high.

#6a: PARTIAL — 6.1 L196 now says the WAL mechanics stand on their own, but v2→v3 changelog L23 still says it is “borrowing llm-runner’s proven intent-log discipline” — the unverifiable appeal remains in the doc. Confidence: high.

#7: CLOSED — 5.4 L180 defines a first-class `fed:<peer-fingerprint>` harness class, required provider allowlisting, default config posture, and an AFT phase-2 verification gate — this addresses the prior absence of a provider-registration story; source confirms unknown/prefixed harness handling is provider-sensitive (`subc-mcp/src/main.rs` L1149-L1156). Confidence: high.

#8: CLOSED — 5.3 L170-L174 gates first contact as non-routable until OOB code confirmation, defines old-key-signed/verified rotation, and binds the code to long-term static keys — this closes first-contact, rotation, and code-binding ambiguity. Confidence: high.

#9: CLOSED — 6.2 L213 makes the fed-module reaper authoritative and closes affected loopback connections, explicitly not relying on module-direction GOODBYE — this matches source: module GOODBYE is best-effort (`forwarding.rs` L68-L93), while module-connection removal releases client routes (`forwarding.rs` L1112-L1120, L1183-L1195). Confidence: high.

#10: CLOSED — 6.5 L223 says raw capability documents are filtered before constructing typed manifests because unknown `ProviderRole` tags fail decode — this matches `manifest.rs` L34-L39. Confidence: high.

#11: PARTIAL — 2.5 L79 and  L251 correctly choose one connection per `(peer, remote module)`, but 3.1 L102, 4.1 L129,  L238, and  L267 still say per-peer/per-peer HELLO — source requires separate connections because `register_module_connection` evicts a prior module on the same connection (`forwarding.rs` L271-L307, especially L280-L282). Confidence: high.

#12: CLOSED — 6.4 L220 accurately flags that `ClientHello` must gain device identity and says today it does not — source confirms current `ClientHello` only has `client_nonce` and `role` (`auth.rs` L24-L28); phase-4+ deferral is acceptable for phase 0. Confidence: high.

#13: CLOSED —  L243 now says Fork Cat’s mechanism is P1 `catalog.update` per `(peer, module)` and only the staleness-window number remains open — the stale “coarse re-HELLO” Fork Cat contradiction is removed. Confidence: high.

## NEW-CONTRADICTIONS

1. **Removed-tool semantics:** 2.6 L84-L86 says no tool-granular GOODBYE and removed tools get module-side typed errors; 4.1 L130 says removed tools get route-GOODBYE.
2. **Connection topology:** 2.5 L79 /  L251 say per `(peer, remote module)`; 3.1 L102, 4.1 L129,  L238, and  L267 still say per peer/per-peer HELLO.
3. **llm-runner appeal:** v4 changelog 6.1 L11 and 6.1 L196 say the external appeal is dropped/standalone; v2→v3 changelog L23 still invokes llm-runner’s “proven” discipline.

## WEAKENED-DECISIONS

- The P1 decision “no tool-granular GOODBYE” is weakened by 4.1 L130.
- The v4 topology decision “one connection per `(peer, remote module)`” is weakened by current architecture/decision-log text still saying “per peer.”

PHASE-0 VERDICT: NO-GO — P2 is now implementable, and P1’s normative 2.6 text is mostly correct, but the doc still contains a phase-0-relevant contradiction that re-promises impossible removed-tool route-GOODBYE, plus the 6.1 pre-intent-crash row lacks a concrete durable correlation/tombstone mechanism for the phase-0 test vectors. Fix 4.1 L130, update stale per-peer topology text, remove the llm-runner appeal, and specify the pre-intent recovery key/API before building P1/P2 under this design.