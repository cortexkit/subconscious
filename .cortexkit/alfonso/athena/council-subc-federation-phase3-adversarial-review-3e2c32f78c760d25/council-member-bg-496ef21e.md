## Finding 1: Lack of Proof-of-Possession (PoP) during Device Enrollment
- **Severity**: BLOCKER
- **Location**: 4.2 Device enrollment
- **Confidence**: high
- **Issue**: The enrollment endpoint `POST /v1/device/enroll` accepts a provider JWT and a payload containing `{device_pubkey, device_name, platform}`. However, there is no cryptographic proof (such as a signature over a challenge or the JWT itself using the device's private key) demonstrating that the client actually possesses the private key corresponding to `device_pubkey`. An attacker who obtains a user's WorkOS JWT (e.g., via session hijacking or a compromised client) can enroll *any* public key under that user's account. More critically, a compromised cloud or a malicious insider at the cloud provider could register arbitrary public keys to a user's account without the device's consent.
- **Evidence**: 4.2 step 2: "CLI calls `POST /v1/device/enroll` with the JWT + `{device_pubkey, device_name, platform}`." No signature or challenge-response is specified.
- **Suggested Fix**: Require the enrollment request to include a signature. The device must sign a challenge (e.g., a server-provided nonce or a combination of the JWT and a timestamp) using its private key. The Worker/AccountDO must verify this signature against the provided `device_pubkey` before completing enrollment.

## Finding 2: Cloud-Authored Profiles Can Bypass the Verify-Code Ceremony
- **Severity**: BLOCKER
- **Location**: 3.3 Profiles & identity, 4.3 What "same account" grants, 5.3 Cloud control-plane trust
- **Confidence**: high
- **Issue**: The design states that "cloud-login distributes one (signed) [profile]" and that "the profile is the single source of truth the federation module reads." The profile contains the peer list and their egress policies. The design also claims that "routability still requires the phase-2 verify-code ceremony" and that a compromised cloud cannot make any existing device trust a rogue one. If the profile is distributed and signed by the cloud, and the local federation module treats this profile as the single source of truth for routing and exposure policies, then a compromised cloud can simply distribute a tampered profile where a rogue device is marked as `verified: true` and has an active exposure allowlist. The local device, trusting the cloud-signed profile, would immediately route calls to the rogue device and allow it to invoke local tools, completely bypassing the out-of-band verify-code ceremony.
- **Evidence**:
  - 3.3: "The CK app authors profiles; cloud-login distributes one (signed)... The profile is the single source of truth the federation module reads."
  - 4.3: "Routability still requires the phase-2 verify-code ceremony... A cloud/account compromise therefore yields metadata + the ability to offer a rogue device for pairing — it cannot make any existing device trust the rogue one..."
- **Suggested Fix**: The verification state (the mapping of `peer_pubkey -> verified_status`) must be stored in a local, device-controlled database that is *never* overwritten or authored by the cloud. The cloud-distributed profile may only propose *candidate* peers (which default to `verified: false`). The transition to `verified: true` must be written locally by the device after the out-of-band ceremony and stored in a local-only tamper-proof store (e.g., the credentials vault or a local SQLite table).

## Finding 3: Simultaneous Dialing Race and Flapping in subc-core
- **Severity**: BLOCKER
- **Location**: 5.4 Dial policy
- **Confidence**: high
- **Issue**: The design states that "the initiator (pubkey tie-break as phase 2) tries candidates". However, the phase-2 design doc does not actually specify or support a pubkey tie-break mechanism (inheritance failure). If both sides dial each other simultaneously, both may succeed in establishing a connection (e.g., one via direct TCP and one via WebSocket/Relay). If two connections are established simultaneously, both will attempt to register loopback connections to subc-core for the same remote modules. In subc-core, `register_module_connection` evicts the prior registration on the same connection/module ID. This will cause constant connection flapping (eviction loop) as the two active connections repeatedly evict each other, leading to failed tool calls and high CPU usage.
- **Evidence**:
  - Phase-3 5.4: "the initiator (pubkey tie-break as phase 2) tries candidates".
  - Phase-2 design doc: No mention of "tie-break" or "initiator" or how simultaneous dialing is resolved.
  - Phase-2 2.5: "one loopback connection per (peer, remote module)... register_module_connection evicts the prior registration".
- **Suggested Fix**: Define a strict, deterministic tie-break mechanism. Compare the lexicographical order of the two device public keys. The device with the higher public key is the designated initiator. If a device receives an incoming connection from a peer with a lower public key while it is attempting an outbound connection, it must immediately abort its outbound attempt and accept the incoming connection. If both connections somehow establish, the device with the lower public key must close its outbound connection upon detecting the double-win, ensuring only one connection remains active.

## Finding 4: Lack of Local Tombstone Persistence for Revoked Devices
- **Severity**: HIGH
- **Location**: 4.2 Device removal
- **Confidence**: high
- **Issue**: When a device is removed, the AccountDO deletes the record, revokes the token, and broadcasts a registry delta. Peers unpair it, and the "profile entry flips to `revoked_by_account`, non-routable, requires re-pair if re-enrolled." If the revocation state is only updated via a transient broadcast delta or a cloud-provided profile, a compromised cloud or a malicious actor who gains temporary control of the account can delete the revocation status or re-enroll the device and distribute a profile where the device is active and verified. If the local device does not persistently store a local tombstone of revoked public keys, it will not know that the device was previously revoked and requires a re-pair.
- **Evidence**: 4.2: "peers unpair it (profile entry flips to `revoked_by_account`, non-routable, requires re-pair if re-enrolled)." The mechanism for persisting this state locally and preventing the cloud from clearing it is not specified.
- **Suggested Fix**: The local federation module must maintain a persistent, local-only blocklist/tombstone store of revoked peer public keys. Once a peer is marked as `revoked_by_account` (either via a signed revocation message or user action), this state must be persisted locally. Any future attempt by the cloud to re-introduce this public key must be rejected or treated as an unverified new device requiring a full out-of-band pairing ceremony, ignoring any cloud-provided claims to the contrary.

## Finding 5: Candidate Poisoning and SSRF Risk in Signaling
- **Severity**: HIGH
- **Location**: 5.2 Registry entry, 5.4 Dial policy
- **Confidence**: high
- **Issue**: Devices self-report their LAN candidates to the registry, and the initiator dials these candidates in order (lan -> public -> relay). There is no validation or filtering of the IP addresses provided in the candidate list. A compromised sibling device or a compromised cloud can inject malicious IP addresses into the candidate list (e.g., `127.0.0.1`, `169.254.169.254`, or private subnet IPs of sensitive internal services). When the victim device attempts to dial these candidates, it will perform TCP connections to these addresses. This enables Server-Side Request Forgery (SSRF) or port scanning of the victim's local network and loopback interface, potentially exposing local services (like Redis, databases, or cloud metadata endpoints) to the attacker.
- **Evidence**: 5.2: `"candidates": [{"kind": "lan", "addr": "192.168.1.34:7841"}]` is self-reported. 5.4: "tries candidates in order: lan → public → relay".
- **Suggested Fix**: The dialing device must strictly validate and filter all candidates before dialing:
  1. Block loopback addresses (`127.0.0.0/8`, `::1`) entirely for remote peers.
  2. Block link-local addresses (`169.254.0.0/16`, `fe80::/10`).
  3. Restrict LAN candidates to the subnet(s) of the dialing device's active network interfaces. If a candidate IP is not within the local subnet, it must not be dialed as a LAN candidate.

## Finding 6: Symmetric Relay Pipe Tokens and Lack of Single-Use Enforcement
- **Severity**: HIGH
- **Location**: 5.3 Signaling ops,  Relay
- **Confidence**: high
- **Issue**: The `relay_open` operation returns a single `pipe_token` issued to "both sides". The RelayDO bridges "exactly two authenticated WebSockets (`pipe_id` + per-side `pipe_token`, single-use, short TTL)". If the same `pipe_token` is issued to both sides (symmetric token), the RelayDO cannot distinguish between the initiator (Device A) and the responder (Device B). This leads to several issues:
  1. An attacker who intercepts the token (or a compromised cloud) can connect twice to the RelayDO using the same token, consuming both slots and blocking the legitimate devices (DoS), or bridging the connection to themselves.
  2. There is no cryptographic binding of the WebSocket connection to the specific device's identity at the relay level.
  3. If the token is symmetric, a compromised cloud can easily impersonate one of the sides to the relay.
- **Evidence**: 5.3: "`relay_open {to: pubkey}` → `relay_grant {relay_url, pipe_id, pipe_token}` issued to both sides".  "bridges exactly two authenticated WebSockets (`pipe_id` + per-side `pipe_token`...)".
- **Suggested Fix**: Issue asymmetric, per-side tokens: `pipe_token_a` for the initiator and `pipe_token_b` for the responder. The RelayDO must enforce that one connection presents `pipe_token_a` and the other presents `pipe_token_b`. Additionally, the tokens should be cryptographically bound to the respective device public keys (e.g., by requiring the client to sign the token or a challenge during the WebSocket handshake).

## Finding 7: Idle Teardown of Relay Pipes Causes Spurious GOODBYEs and Catalog Eviction
- **Severity**: HIGH
- **Location**:  Relay,  Transport adapter
- **Confidence**: high
- **Issue**: The RelayDO tears down the pipe on idle timeout, and the peers must reconnect. The design states that "relay-path partitions classify exactly like TCP partitions." In phase-2 6.2, a partition causes the keepalive reaper to close the peer's loopback connections, which delivers deterministic route-GOODBYEs to all consumers and evicts the peer's catalog. If an idle relay pipe teardown is treated exactly like a partition, it will cause the peer's catalog to be evicted and all routes to be torn down every time the connection goes idle. This results in a terrible user experience where tools constantly disappear and reappear, and in-flight calls right after an idle period fail with route errors.
- **Evidence**:
  - Phase-3  "idle timeout tears the pipe down (peers reconnect via a fresh grant...)".
  - Phase-3  "relay-path partitions classify exactly like TCP partitions."
  - Phase-2 6.2: "On declaring a peer partitioned it closes that peer's loopback connections, so subc-core's connection-granular cleanup delivers deterministic route-GOODBYEs..."
- **Suggested Fix**: Differentiate between a clean idle teardown and an abnormal network partition. When the RelayDO closes the connection due to idle timeout, it should send a specific WebSocket close code (e.g., `4000 Idle Teardown`). Upon receiving this code, the fed-module must *not* unregister the peer's catalog or close the loopback connections. Instead, it should keep the routes registered but mark the connection state as "dormant". The next tool call to that peer will trigger a transparent, on-demand reconnect (requesting a new relay grant and re-establishing the WS connection) before forwarding the call, without the consumer ever seeing a GOODBYE.

## Finding 8: WS Carrier Framing Ambiguity and Parser Vulnerability
- **Severity**: MEDIUM
- **Location**:  Transport adapter
- **Confidence**: high
- **Issue**: The `WsCarrier` uses WebSocket binary messages with "one fed record per WS message; the 4-byte length prefix is redundant inside a message framing but kept identical". The design does not specify how the receiver handles mismatches between the 4-byte length prefix and the actual WebSocket message length. If the receiver's parser reads the 4-byte length prefix and then reads that many bytes from the message, it may leave trailing bytes unparsed (if the prefix is smaller than the WS message) or hang waiting for more bytes (if the prefix is larger than the WS message). This ambiguity can lead to protocol desynchronization, resource exhaustion (hanging parsers), or message smuggling/evasion attacks.
- **Evidence**:  "one fed record per WS message; the 4-byte length prefix is redundant inside a message framing but kept identical so record parsing is carrier-agnostic".
- **Suggested Fix**: The receiver must strictly validate that the 4-byte length prefix matches the actual WebSocket message payload length minus 4 bytes. If there is any mismatch, or if a WebSocket message contains more or less than one complete record, the receiver must immediately terminate the connection with a protocol error.

## Finding 9: Noise Session Resumption and Session Scoping Conflict
- **Severity**: MEDIUM
- **Location**:  Relay,  Transport adapter
- **Confidence**: medium
- **Issue**: The design states: "Noise session resumption = ordinary re-handshake, cheap". However, in Noise IK, a re-handshake establishes a completely new Noise session with a new session key. In phase-2 2.5, "per-peer session scoping rides the session field." If a new Noise session is established on reconnect, the session identifier changes. If the serving side uses the Noise session ID for session scoping, a reconnect mid-effect or after an idle teardown will change the session ID, causing the serving side to treat it as a new session and potentially discard session-scoped resources or fail to correlate the connection with the active effect ledger's recovery reconciliation.
- **Evidence**:
  - Phase-3  "Noise session resumption = ordinary re-handshake, cheap".
  - Phase-2 2.5: "per-peer session scoping rides the session field."
- **Suggested Fix**: Ensure that session scoping and the effect ledger are decoupled from the ephemeral Noise session ID. Instead, they must key on the long-term device public key and the incarnation UUID, which remain constant across reconnects and re-handshakes.

## Finding 10: Missing Nonces/Timestamps in Signaling Messages (Replay Attack)
- **Severity**: MEDIUM
- **Location**: 5.3 Signaling ops
- **Confidence**: high
- **Issue**: The signaling messages (`connect_request`, `connect_offer`, `connect_accept`) do not specify any nonces, timestamps, or signatures. An attacker (or a compromised cloud/relay) can capture and replay old signaling messages. For example, replaying an old `connect_offer` could force a device to repeatedly attempt connections, allocate resources, or dial candidates, leading to resource exhaustion or unexpected network traffic.
- **Evidence**: 5.3: The message schemas listed (`connect_request {to: pubkey}`, `connect_offer {from: pubkey, candidates}`, `connect_accept {candidates}`) contain no nonces, timestamps, or cryptographic signatures.
- **Suggested Fix**: Include a unique session nonce and a UTC timestamp in all signaling messages. The receiving device must verify that the timestamp is within a reasonable window (e.g., 30 seconds) and track recently seen nonces to prevent replay attacks.

## Finding 11: AccountDO Hot Spotting and Scaling Limits
- **Severity**: MEDIUM
- **Location**: 5.1 Shape
- **Confidence**: high
- **Issue**: The design uses a single Durable Object per account (`AccountDO`) to manage the device registry, hold live WebSockets for all online devices, and handle all signaling. Durable Objects are single-threaded and run on a single Cloudflare coordinate. If an account has a large number of devices (e.g., a team account or a user with many automated agents/daemons), the signaling traffic and registry updates could overwhelm the single DO, leading to high latency, message drops, or CPU limit exhaustion.
- **Evidence**: 5.1: "AccountDO (one per account): SQLite-backed device registry; holds the live WebSocket per online device... pushes registry deltas and signaling messages."
- **Suggested Fix**: Enforce a strict limit on the maximum number of devices per account (e.g., 50 devices) in v1. For future scaling (teams/multi-user), the signaling and registry paths should be decoupled, or the registry should be moved to a distributed store (like Cloudflare KV or D1) with the DO only handling active signaling channels.

## Finding 12: Rendezvous Outage Bricks Discovered Peers (No Local Candidate Caching)
- **Severity**: MEDIUM
- **Location**: 5.5 Fed-module integration
- **Confidence**: high
- **Issue**: The design states that "Registry-discovered peers materialize as unverified profile peers... discovery adds candidates, not trust." It does not specify if these candidates are persisted locally. If the rendezvous server goes down, and the device restarts or clears its memory, it will lose all candidate information for discovered peers. Even if the peers are on the same LAN and could be dialed directly, they will become unreachable because the device has no way to discover their candidates without the rendezvous server.
- **Evidence**: 5.5: "Absent → phase-2 behavior exactly... Static `addr` in a profile acts as one more candidate...". No mention of local caching of discovered candidates.
- **Suggested Fix**: Persist discovered peer candidates in the local profile database. If the rendezvous server is offline, the fed-module must fall back to dialing the last-known cached candidates for all paired peers, ensuring local network federation continues to work during a cloud outage.

## Finding 13: Device Clock Skew Vulnerability in Token Expiry and Registry
- **Severity**: LOW
- **Location**: 4.2 Device enrollment, 5.2 Registry entry
- **Confidence**: medium
- **Issue**: The design mentions "long-lived [device tokens] with rotation" and `"last_seen_ms": 0` in the registry. It does not specify how clock skew is handled. If the device token rotation or expiry relies on the device's wall-clock, a device with a skewed or rolled-back clock may fail to authenticate or fail to rotate its token. If `last_seen_ms` is self-reported by the device, a malicious or misconfigured device could report a future timestamp, keeping itself marked as "online" indefinitely.
- **Evidence**: 5.2: `"last_seen_ms": 0` in the registry entry.
- **Suggested Fix**:
  1. The `last_seen_ms` timestamp must be set strictly server-side by the AccountDO using the server's clock.
  2. Device token expiry and rotation must be managed using the server's clock, and the client should use the server-provided expiration duration (relative time) rather than absolute wall-clock timestamps.

---

## ANSWERS TO THE FIVE OPEN QUESTIONS

1. **Device-token custody on disk (0600 file next to the device key today) — sufficient for v1, or fold into the credentials vault immediately?**
   - *Recommendation*: Storing the device token in a `0600` file next to the device key is sufficient for v1. The device key itself is already stored on disk (as per phase-2 2.1 "key-on-disk"), which defines the local security boundary. If an attacker can read a `0600` file, they can also read the device's private key, which is a far more critical compromise (impersonation vs signaling access). However, as a hardening step, the token should be migrated to the credentials vault in phase 4 to centralize credential management.

2. **Registry `candidates` self-reporting: any hardening needed against a malicious *sibling device* on the same account lying about its LAN addr?**
   - *Recommendation*: Yes, hardening is required. While Noise IK prevents identity impersonation, a malicious sibling device lying about its LAN address can perform SSRF or port scanning against other devices on the account (see Finding 5). The dialing device must strictly validate and filter all candidates (blocking loopback, link-local, and non-local subnets) before attempting to dial them.

3. **Relay pipe lifetime policy: per-connection grants vs a standing pipe per peer-pair — is the reconnect-per-idle-teardown churn acceptable at keepalive cadence?**
   - *Recommendation*: Reconnect-per-idle-teardown churn is acceptable *only if* the fed-module implements transparent reconnect-on-demand and does not evict the peer's catalog or tear down loopback connections on idle close (see Finding 7). If every idle teardown causes a catalog eviction and route GOODBYEs, the churn is unacceptable. We recommend a standing pipe per active peer-pair with a generous idle timeout (e.g., 5-10 minutes) to minimize churn during active sessions, combined with the "dormant" connection state to avoid catalog eviction.

4. **Should `connect_offer` require the target to be *verified* before signaling is relayed (quieter unpaired devices) or is offer-to-unverified needed for the pairing UX itself? (Current design: allowed, loud.)**
   - *Recommendation*: `connect_offer` must be allowed for unverified devices to support a seamless pairing UX (where one device initiates pairing and the other receives the prompt). However, to prevent spam and signaling floods from unverified devices, the AccountDO must enforce strict rate limits on `connect_request` messages sent to unverified targets (e.g., max 3 requests per hour per target). Additionally, the target device should only display a pairing prompt and must *never* dial candidates or establish a data-plane connection until the user explicitly initiates the pairing flow on the target.

5. **WS hibernation vs signaling latency: acceptable to pay a cold-wake round-trip on first signal to an idle account?**
   - *Recommendation*: Yes, paying a cold-wake round-trip (typically 200-800ms) on the *first* signal to an idle account is acceptable for v1, as it only occurs when initiating a new session after a period of inactivity. However, the candidate dial timeout must be increased from 2s to at least 5s for the initial connection request to accommodate this cold-wake latency and prevent spurious timeouts.

---

## OVERALL VERDICT: GO-WITH-CHANGES

### Must-Fix-Before-Build Blockers:
1. **Finding 1 (Lack of Proof-of-Possession during Device Enrollment)**: Must require a cryptographic signature from the device's private key during enrollment to prevent token hijacking and rogue key registration.
2. **Finding 2 (Cloud-Authored Profiles Can Bypass the Verify-Code Ceremony)**: Must store the peer verification status in a local, device-controlled database that cannot be overwritten or authored by the cloud.
3. **Finding 3 (Simultaneous Dialing Race and Flapping in subc-core)**: Must define a deterministic tie-break mechanism (e.g., lexicographical public key comparison) to prevent connection flapping and eviction loops in subc-core.