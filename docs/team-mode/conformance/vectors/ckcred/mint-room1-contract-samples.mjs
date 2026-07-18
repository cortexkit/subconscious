// Regenerates the Room-1 contract fixture set: the signed artifact samples the
// team-mode spec-phase Athena rounds (and later FED / ALF / serve-admission test
// suites) verify against. Shapes follow the Room-1 output contract v7
// (subconscious docs/team-mode/room1-org-grant-acting-for-contract.md @ f6ef57d5):
// §1 artifact claim sets, §7 typ/aud checklist, intent_id effect-dedup claims,
// structured mint refusals, delegation-grant scope with agent list, and the
// single-source ADMITTED → SENDING → terminal ledger state machine vectors.
//
// This set is NORMATIVE for claim schemas (contract §7): a claim-shape change is
// a contract amendment landing as a fixture diff plus a room notice; consumers
// vendor by commit hash.
//
// Same discipline as the fixtures beside this file: signed with the repo's
// THROWAWAY test key (kid test-key-1), NEVER the prod signing key. The A4
// device-record assertion is FED's artifact (fed cloud key domain) and is
// deliberately absent here — fed mints its own fixture so the two trust domains
// never share even test keys.
//
// Families and their negatives:
//   membership_assertion: valid · expired · wrong_aud · typ_confusion (presented
//     where an acting_for is expected — same bytes, verifier must die on typ)
//   refusal: revoked · expired · org_gone (positive statements of non-membership)
//   acting_for: valid · expired · cross_gateway (gateway claim != presenter) ·
//     cross_daemon (aud != verifying daemon) · replayed (same jti twice — consumer
//     side proves the second consume fails) · typ_confusion (A2-as-A3 partner)
//   epoch_push: revoked · compromised · org_dissolved
//   ask_authority vectors (not JWTs — condition tables for the three-factor
//     ask-time check): delegation_epoch_stale · grant_deleted (fail closed as
//     unknown-grant) · membership_epoch_stale
//
// Run: node test/fixtures/mint-room1-contract-samples.mjs > test/fixtures/room1-contract-samples.json

import { importJWK, SignJWT } from "jose";

const TEST_JWK = {
  kty: "OKP",
  crv: "Ed25519",
  x: "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo",
  d: "nWGxne_9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A",
  kid: "test-key-1",
  alg: "EdDSA",
};

const ISSUER = "https://account.cortexkit.io";
const FAR_FUTURE = 4102444800; // 2100-01-01 — unexpired for the fixture's life
const PAST = 946684800; // 2000-01-01 — expired long ago

// Documented sample identities (ULID-shaped, fixed so verifiers can pin equality):
const ORG = "01SAMPLEORG0000000000000000";
const ALICE = "01SAMPLEACCOUNTALICE0000000";
const ORG_DAEMON_SP = "01SAMPLESVCORGDAEMON0000000"; // aud target for A3
const OTHER_DAEMON_SP = "01SAMPLESVCOTHERDAEMON00000"; // cross_daemon negative
const GATEWAY_SP = "01SAMPLESVCGATEWAY000000000"; // legitimate presenter
const OTHER_GATEWAY_SP = "01SAMPLESVCOTHERGATEWAY0000"; // cross_gateway negative
const GRANT_ID = "01SAMPLEDELEGATIONGRANT0000";
const MEMBERSHIP_EPOCH = 7;
const DELEGATION_EPOCH = 3;
const INTENT_ID = "01SAMPLEINTENTTURN000000000"; // gateway-minted per user-visible turn
const BOB = "01SAMPLEACCOUNTBOB000000000"; // second subject for intent_collision

const b64url = (bytes) => Buffer.from(bytes).toString("base64url");
// hash(team_id:user_id) placeholder: b64url of 32x 0x0a, reconstructible.
const PLATFORM_BINDING = b64url(Buffer.alloc(32, 0x0a));

const priv = await importJWK(TEST_JWK, "EdDSA");

// Protected header carries typ:"JWT" to match production jwt.ts
// (setProtectedHeader({alg,kid,typ:"JWT"})) and the r2 spec §2 pin (header
// typ="JWT" always; the artifact discriminator is the PAYLOAD typ claim below).
function sign({ typ, aud, claims = {}, exp = FAR_FUTURE, jti, iat = 0 }) {
  return new SignJWT({ typ, ...claims })
    .setProtectedHeader({ alg: "EdDSA", kid: TEST_JWK.kid, typ: "JWT" })
    .setIssuer(ISSUER)
    .setAudience(aud)
    .setIssuedAt(iat)
    .setExpirationTime(exp)
    .setJti(jti ?? crypto.randomUUID())
    .sign(priv);
}

const assertionClaims = {
  subject: ALICE,
  org: ORG,
  role: "member",
  membership_epoch: MEMBERSHIP_EPOCH,
};

const actingForClaims = {
  sub: ALICE,
  org: ORG,
  surface: "slack",
  platform_binding: PLATFORM_BINDING,
  gateway: GATEWAY_SP,
  grant_id: GRANT_ID,
  intent_id: INTENT_ID,
};

const A2_AUD = `cortexkit-org:${ORG}`;

const REPLAY_JTI = "room1-fixture-replayed-jti-0001";

const out = {
  comment:
    "Room-1 contract fixture set (tracks the contract version pinned below). Throwaway test key (kid test-key-1) — see mint script header. A4 device-record assertions are fed-cloud-key domain and live in fed's fixtures, not here.",
  contract: "room1-org-grant-acting-for-contract.md @ f02d9b2f (v7.3)",
  jwks: { keys: [{ kty: TEST_JWK.kty, crv: TEST_JWK.crv, x: TEST_JWK.x, kid: TEST_JWK.kid, alg: TEST_JWK.alg }] },
  // Stable vector ids (r1-<family>-<case> scheme) for cross-seat conformance-corpus
  // citation: a failing test names the exact vector, not a file+index. Maps each
  // stable id to the dotted path of the artifact below. Additive — ids never change
  // once assigned; a new artifact gets a new id, never a renumber.
  vector_ids: {
    "r1-a2-valid-01": "membership_assertion.valid",
    "r1-a2-expired-01": "membership_assertion.expired",
    "r1-a2-wrongaud-01": "membership_assertion.wrong_aud",
    "r1-refusal-revoked-01": "refusal.revoked",
    "r1-refusal-expired-01": "refusal.expired",
    "r1-refusal-orggone-01": "refusal.org_gone",
    "r1-a3-valid-01": "acting_for.valid",
    "r1-a3-expired-01": "acting_for.expired",
    "r1-a3-crossgateway-01": "acting_for.cross_gateway",
    "r1-a3-crossdaemon-01": "acting_for.cross_daemon",
    "r1-a3-replayed-01": "acting_for.replayed_pair",
    "r1-a3-remint-01": "acting_for.remint_pair",
    "r1-a3-collision-01": "acting_for.intent_collision",
    "r1-typ-a2asa3-01": "typ_confusion.a2_presented_as_a3",
    "r1-typ-a3asa2-01": "typ_confusion.a3_presented_as_a2",
    "r1-grant-valid-01": "delegation_grant.valid",
    "r1-grant-emptylist-01": "delegation_grant.empty_list",
    "r1-mintref-ratelimited-01": "mint_refusal.rate_limited",
    "r1-mintref-nodelegation-01": "mint_refusal.no_delegation",
    "r1-mintref-delegationrevoked-01": "mint_refusal.delegation_revoked",
    "r1-mintref-unknownsubject-01": "mint_refusal.unknown_subject",
    "r1-mintref-orggone-01": "mint_refusal.org_gone",
    "r1-mintref-intentexpired-01": "mint_refusal.intent_expired",
    "r1-mintref-intentcollision-01": "mint_refusal.intent_collision",
    "r1-push-revoked-01": "epoch_push.revoked",
    "r1-push-compromised-01": "epoch_push.compromised",
    "r1-push-orgdissolved-01": "epoch_push.org_dissolved",
    "r1-env-bundle-01": "serving_envelopes.bundle",
    "r1-env-bundleunchanged-01": "serving_envelopes.bundle_unchanged",
    "r1-env-servicekeys-steady-01": "serving_envelopes.service_keys.steady",
    "r1-env-servicekeys-rotation-01": "serving_envelopes.service_keys.rotation_overlap",
    "r1-env-servicekeys-orggone-01": "serving_envelopes.service_keys.org_gone",
    "r1-env-pushwebhook-01": "serving_envelopes.epoch_push_webhook",
  },
  identities: {
    org: ORG,
    alice: ALICE,
    org_daemon_sp: ORG_DAEMON_SP,
    other_daemon_sp: OTHER_DAEMON_SP,
    gateway_sp: GATEWAY_SP,
    other_gateway_sp: OTHER_GATEWAY_SP,
    grant_id: GRANT_ID,
    membership_epoch: MEMBERSHIP_EPOCH,
    delegation_epoch: DELEGATION_EPOCH,
    intent_id: INTENT_ID,
    bob: BOB,
  },
  membership_assertion: {
    valid: await sign({ typ: "membership_assertion", aud: A2_AUD, claims: assertionClaims }),
    expired: await sign({ typ: "membership_assertion", aud: A2_AUD, claims: assertionClaims, exp: PAST }),
    wrong_aud: await sign({ typ: "membership_assertion", aud: "cortexkit-fed", claims: assertionClaims }),
  },
  refusal: {
    revoked: await sign({ typ: "refusal", aud: A2_AUD, claims: { subject: ALICE, org: ORG, reason: "revoked" } }),
    expired: await sign({ typ: "refusal", aud: A2_AUD, claims: { subject: ALICE, org: ORG, reason: "expired" } }),
    org_gone: await sign({ typ: "refusal", aud: A2_AUD, claims: { subject: ALICE, org: ORG, reason: "org_gone" } }),
  },
  acting_for: {
    valid: await sign({ typ: "acting_for", aud: ORG_DAEMON_SP, claims: actingForClaims }),
    expired: await sign({ typ: "acting_for", aud: ORG_DAEMON_SP, claims: actingForClaims, exp: PAST }),
    // Presenter authenticates as GATEWAY_SP; this token names OTHER_GATEWAY_SP:
    // admission must fail presenter-identity == gateway claim.
    cross_gateway: await sign({
      typ: "acting_for",
      aud: ORG_DAEMON_SP,
      claims: { ...actingForClaims, gateway: OTHER_GATEWAY_SP },
    }),
    // aud names a different org daemon: verifying daemon must fail aud == self.
    cross_daemon: await sign({ typ: "acting_for", aud: OTHER_DAEMON_SP, claims: actingForClaims }),
    // Two DISTINCT valid tokens sharing one jti (iat differs to force distinct
    // bytes — Ed25519 signing is deterministic, so identical claims would yield
    // an identical token): consume the first, the second must die on (org, jti)
    // uniqueness — replay across re-mints, not just byte-identical re-sends.
    replayed_pair: [
      await sign({ typ: "acting_for", aud: ORG_DAEMON_SP, claims: actingForClaims, jti: REPLAY_JTI, iat: 0 }),
      await sign({ typ: "acting_for", aud: ORG_DAEMON_SP, claims: actingForClaims, jti: REPLAY_JTI, iat: 1 }),
    ],
    // The F1/R1 recovery shape: same intent_id, FRESH jti — a legitimate re-mint
    // after a crashed turn. Verifies fine; the serve ledger's (org, intent_id)
    // dedup serves the recorded outcome for ADMITTED/RECORDED rows instead of
    // dispatching twice.
    remint_pair: [
      await sign({ typ: "acting_for", aud: ORG_DAEMON_SP, claims: actingForClaims, jti: "room1-fixture-remint-jti-a" }),
      await sign({ typ: "acting_for", aud: ORG_DAEMON_SP, claims: actingForClaims, jti: "room1-fixture-remint-jti-b" }),
    ],
    // G4 intent_collision: same intent_id, DIFFERENT subject. The mint refuses
    // this at first-seen (intent_id -> subject atomic insert-if-absent), so this
    // token existing at all represents a compromised/buggy mint path — serve
    // admission must still refuse it against the ledger row's subject.
    intent_collision: await sign({
      typ: "acting_for",
      aud: ORG_DAEMON_SP,
      claims: { ...actingForClaims, sub: BOB },
    }),
  },
  typ_confusion: {
    comment:
      "Cross-slot presentations: each verifies cryptographically under the shared JWKS; the verifier MUST die on typ (and would also die on aud). Both directions provided.",
    a2_presented_as_a3: await sign({
      typ: "membership_assertion",
      aud: ORG_DAEMON_SP,
      claims: assertionClaims,
    }),
    a3_presented_as_a2: await sign({ typ: "acting_for", aud: A2_AUD, claims: actingForClaims }),
  },
  delegation_grant: {
    comment:
      "Contract \u00a75 grant shape with the v7 agent list. Not a JWT on the wire today (grants are D1 rows served via bundles); these samples pin the SCHEMA. agents:[] authorizes NOTHING (fail-closed) \u2014 the empty_list sample is the negative.",
    valid: {
      grant_id: GRANT_ID,
      gateway_principal: GATEWAY_SP,
      subject_account: ALICE,
      org: ORG,
      scope: { invoke: true, agents: ["01SAMPLESVCTRIAGEAGENT00000", "01SAMPLESVCOPSAGENT0000000"] },
      delegation_epoch: DELEGATION_EPOCH,
    },
    empty_list: {
      grant_id: "01SAMPLEDELEGATIONGRANT0001",
      gateway_principal: GATEWAY_SP,
      subject_account: ALICE,
      org: ORG,
      scope: { invoke: true, agents: [] },
      delegation_epoch: 1,
      expect: "every gateway turn refused: agents:[] authorizes nothing",
    },
  },
  mint_refusal: {
    comment:
      "Structured mint-refusal reasons (contract \u00a75): the gateway renders honest UX from the reason; the anomaly-detection lane reads the same stream. Signed like any CKCRED artifact.",
    rate_limited: await sign({ typ: "refusal", aud: GATEWAY_SP, claims: { org: ORG, subject: ALICE, reason: "rate_limited", retry_after_secs: 12 } }),
    no_delegation: await sign({ typ: "refusal", aud: GATEWAY_SP, claims: { org: ORG, subject: ALICE, reason: "no_delegation" } }),
    delegation_revoked: await sign({ typ: "refusal", aud: GATEWAY_SP, claims: { org: ORG, subject: ALICE, reason: "delegation_revoked" } }),
    unknown_subject: await sign({ typ: "refusal", aud: GATEWAY_SP, claims: { org: ORG, subject: BOB, reason: "unknown_subject" } }),
    org_gone: await sign({ typ: "refusal", aud: GATEWAY_SP, claims: { org: ORG, subject: ALICE, reason: "org_gone" } }),
    intent_expired: await sign({ typ: "refusal", aud: GATEWAY_SP, claims: { org: ORG, subject: ALICE, reason: "intent_expired", intent_id: INTENT_ID } }),
    intent_collision: await sign({ typ: "refusal", aud: GATEWAY_SP, claims: { org: ORG, subject: BOB, reason: "intent_collision", intent_id: INTENT_ID } }),
  },
  epoch_push: {
    revoked: await sign({
      typ: "epoch_push",
      aud: A2_AUD,
      claims: { org: ORG, account: ALICE, new_epoch: MEMBERSHIP_EPOCH + 1, reason: "revoked" },
    }),
    compromised: await sign({
      typ: "epoch_push",
      aud: A2_AUD,
      claims: { org: ORG, account: ALICE, new_epoch: MEMBERSHIP_EPOCH + 1, reason: "compromised" },
    }),
    org_dissolved: await sign({
      typ: "epoch_push",
      aud: A2_AUD,
      claims: { org: ORG, account: ALICE, new_epoch: MEMBERSHIP_EPOCH + 1, reason: "org_dissolved" },
    }),
  },
  serving_envelopes: {
    comment:
      "Structural serving-surface samples (spec r2 §3.2/3.3/3.4). NOT JWTs themselves — they WRAP signed artifacts (the JWS strings are opaque couriers). These pin the envelope SHAPE consumers read; the wrapped tokens reuse the families above. Added in the v7.3 refresh (r1 gate FIXTURE-FIDELITY: the bundle/service-keys/epoch-push envelopes were previously deferred).",
    bundle: {
      comment:
        "GET /v1/org/{org}/bundle response (§3.2). ONE atomic-snapshot read: version + service_keys_version + fresh A2s (one per live member) + FULL delegation grant bodies (agents[] for path-(b) target_agent gate) + the cheap delegation_epochs freshness map. A2s ride exactly `version`.",
      version: 42,
      service_keys_version: 3,
      assertions: [await sign({ typ: "membership_assertion", aud: A2_AUD, claims: assertionClaims })],
      delegation_grants: [
        {
          grant_id: GRANT_ID,
          subject_account: ALICE,
          gateway_principal: GATEWAY_SP,
          agents: ["01SAMPLESVCTRIAGEAGENT00000", "01SAMPLESVCOPSAGENT0000000"],
          scope_invoke: true,
          delegation_epoch: DELEGATION_EPOCH,
        },
      ],
      delegation_epochs: { [GRANT_ID]: DELEGATION_EPOCH },
    },
    bundle_unchanged: {
      comment: "since == current version → no content, just the version echo.",
      version: 42,
      unchanged: true,
    },
    service_keys: {
      comment:
        "GET /v1/org/{org}/service-keys (§3.3). The served doc is the SINGLE source of which keys verify. steady = one key; the rotation-overlap sample lists BOTH (prepared state, either verifies).",
      steady: {
        service_keys_version: 3,
        keys: [
          { principal: ORG_DAEMON_SP, kty: "OKP", crv: "Ed25519", x: TEST_JWK.x, key_epoch: 0 },
        ],
      },
      rotation_overlap: {
        service_keys_version: 4,
        keys: [
          { principal: ORG_DAEMON_SP, kty: "OKP", crv: "Ed25519", x: TEST_JWK.x, key_epoch: 0 },
          { principal: ORG_DAEMON_SP, kty: "OKP", crv: "Ed25519", x: "3uJj-tc0jQhF9pC6i0BapFnnaE_wAynKFbY24CB4Fw0", key_epoch: 1 },
        ],
      },
      org_gone: await sign({ typ: "refusal", aud: A2_AUD, claims: { org: ORG, reason: "org_gone" } }),
    },
    epoch_push_webhook: {
      comment:
        "POST body to the org daemon's registered webhook (§3.4) AND the /pushes poll-fallback row shape. events[] carries the opaque epoch_push JWS strings verbatim (courier rule: FED fans out as-is, never re-signs). seq is the per-org monotonic poll cursor.",
      seq: 91,
      events: [
        await sign({
          typ: "epoch_push",
          aud: A2_AUD,
          claims: { org: ORG, account: ALICE, new_epoch: MEMBERSHIP_EPOCH + 1, reason: "revoked" },
        }),
      ],
    },
  },
  ledger_lifecycle_vectors: {
    comment:
      "Contract §3 single-source state machine (v7): ADMITTED → SENDING → terminal{RECORDED|ABORTED|OUTCOME_UNKNOWN}. Three durable txn points (T1 admit, T2 send-intent fsync BEFORE external call, T3 settle). INITIATE order: revalidate → T2 → call → T3; ABORTED reachable ONLY from ADMITTED. Exhaustion is STATE-SCOPED: ADMITTED→ABORTED, SENDING→OUTCOME_UNKNOWN, never crossed. A refused mint (intent_expired/intent_collision/etc.) creates NO ledger row — refusals are mint-surface artifacts.",
    crash_recovery: [
      { row_state: "absent", meaning: "never admitted (incl. every refused mint — refusal creates nothing)", recovery: "fresh dispatch permitted (after §3 revalidation)" },
      { row_state: "ADMITTED", meaning: "admitted, never sent (T2 not durable)", recovery: "INITIATE may proceed under §3 revalidation; revalidation refusal or exhaustion -> ABORTED (never OUTCOME_UNKNOWN: an ADMITTED row cannot have sent)" },
      { row_state: "SENDING", provider_class: "idempotency_key", recovery: "re-send permitted (provider dedups) or re-query; converges to true terminal" },
      { row_state: "SENDING", provider_class: "status_query", recovery: "re-query ONLY, never re-send; converges when query returns terminal" },
      { row_state: "SENDING", provider_class: "neither", recovery: "bounded wait -> OUTCOME_UNKNOWN; no re-drive exists" },
      { row_state: "RECORDED", meaning: "terminal known", recovery: "serve recorded outcome to any re-mint presentation" },
      { row_state: "ABORTED", meaning: "admitted, deliberately never sent (revalidation refused at INITIATE, before T2)", recovery: "serve aborted; turn consumed; fresh turn needs new intent_id" },
      { row_state: "OUTCOME_UNKNOWN", meaning: "reconciliation exhausted from SENDING; may have sent", recovery: "serve unknown (honest, never retried); late provider outcome -> LATE_RECORDED annotation, never a transition" },
    ],
    re_presentation: [
      { row_state: "ADMITTED", response: "outcome_pending — terminal-for-this-turn; gateway MUST NOT auto-remint" },
      { row_state: "SENDING", response: "outcome_pending — terminal-for-this-turn; gateway MUST NOT auto-remint" },
      { row_state: "RECORDED", response: "recorded outcome" },
      { row_state: "ABORTED", response: "aborted (turn consumed)" },
      { row_state: "OUTCOME_UNKNOWN", response: "unknown (honest render, never retried)" },
    ],
    target_agent_check: [
      { target_agent: "01SAMPLESVCTRIAGEAGENT00000", grant: "valid", expect: "authorized (agent in grant.agents)" },
      { target_agent: "01SAMPLESVCOPSAGENT0000000", grant: "valid", expect: "authorized (agent in grant.agents)" },
      { target_agent: "01SAMPLESVCUNLISTEDAGENT000", grant: "valid", expect: "refused (agent not in grant.agents)" },
      { target_agent: "01SAMPLESVCTRIAGEAGENT00000", grant: "empty_list", expect: "refused (agents:[] authorizes nothing)" },
    ],
    slot_lifecycle: [
      { at: "mint (first-seen insert-if-absent wins)", slot: "HELD", note: "atomic; concurrent first mints of one intent_id -> one winner, loser refused intent_collision" },
      { at: "remint within horizon, same subject", slot: "HELD (same slot)", note: "fresh jti, same intent_id" },
      { at: "remint past horizon (1h)", slot: "n/a", note: "refused intent_expired; no A3 can exist past horizon" },
      { at: "horizon + A3 TTL", slot: "RELEASED (age-out)", note: "the last possible A3 expired; slot release and remint eligibility differ by exactly one TTL (H3)" },
    ],
  },
  ask_authority_vectors: {
    comment:
      "Condition tables for the three-factor ask-time check (durable record + membership_epoch + delegation_epoch of the record's frozen grant_id). Not JWTs — the record is a daemon-internal composition; these pin the expected disposition per state.",
    cases: [
      { name: "all_fresh", record: { grant_id: GRANT_ID, minted_membership_epoch: MEMBERSHIP_EPOCH, minted_delegation_epoch: DELEGATION_EPOCH }, current: { membership_epoch: MEMBERSHIP_EPOCH, delegation_epoch: DELEGATION_EPOCH, grant_exists: true }, expect: "authorized" },
      { name: "membership_epoch_stale", record: { grant_id: GRANT_ID, minted_membership_epoch: MEMBERSHIP_EPOCH, minted_delegation_epoch: DELEGATION_EPOCH }, current: { membership_epoch: MEMBERSHIP_EPOCH + 1, delegation_epoch: DELEGATION_EPOCH, grant_exists: true }, expect: "denied_pending_asks_die" },
      { name: "delegation_epoch_stale", record: { grant_id: GRANT_ID, minted_membership_epoch: MEMBERSHIP_EPOCH, minted_delegation_epoch: DELEGATION_EPOCH }, current: { membership_epoch: MEMBERSHIP_EPOCH, delegation_epoch: DELEGATION_EPOCH + 1, grant_exists: true }, expect: "denied_pending_asks_die" },
      { name: "grant_deleted", record: { grant_id: GRANT_ID, minted_membership_epoch: MEMBERSHIP_EPOCH, minted_delegation_epoch: DELEGATION_EPOCH }, current: { membership_epoch: MEMBERSHIP_EPOCH, delegation_epoch: null, grant_exists: false }, expect: "fail_closed_unknown_grant" },
      { name: "member_path_no_grant", record: { grant_id: null, minted_membership_epoch: MEMBERSHIP_EPOCH }, current: { membership_epoch: MEMBERSHIP_EPOCH }, expect: "authorized_membership_only" },
    ],
  },
};

console.log(JSON.stringify(out, null, 2));
