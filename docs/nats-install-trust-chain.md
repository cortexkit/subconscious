# The NATS trust chain at install time: who signs what, and when

Scope: the local `nats-server` install-time configuration and the keys and signatures it depends on.

`ck setup` reaches the vault as a `direct` principal and can sign only through the root ceremony; `signing:msgsig` is not part of this installation trust chain.

## Server rules this design relies on

Read at nats-server tag `v2.15.0` (its `go.mod` pins `nats-io/jwt/v2 v2.8.2`). Paths are
in `github.com/nats-io/nats-server`, cited as file plus item.

- N1. Trusted operator keys are the operator JWT's subject plus its `signing_keys`; the
  subject is dropped when `strict_signing_key_usage` is set
  (`server/jwt.go::validateTrustedOperators`, the `TrustedKeys` loop). An account JWT is
  accepted only if its issuer is one of them (`server/server.go::isTrustedIssuer`, called
  from `verifyAccountClaims` and from client auth in `server/auth.go`).
- N2. A user JWT's issuer must be the account's identity key or a key in the account JWT's
  `signing_keys`; the identity key counts only when the operator is not strict
  (`server/accounts.go::updateAccountClaimsWithRefresh`, the `a.signingKeys` rebuild;
  `server/auth.go::processClientOrLeafAuthentication`, the `acc.hasIssuer(juc.Issuer)` check; `issuer_account` names the
  account when the issuer is a signing key).
- N3. With the dir resolver the system account must be named in config or in the operator
  JWT, and the two must be identical if both are set (`validateTrustedOperators`). The
  rig and the harness set both.
- N4. `resolver_preload` works with the full/dir resolver: each preload is decoded and
  stored with `Store`, which is `saveIfNewer` (`server/server.go::configureResolver`;
  `server/accounts.go::DirAccResolver.Store`). `saveIfNewer` keeps the file on disk when
  it has the same `jti` or a newer `iat` (`server/dirstore.go::saveIfNewer`). So a
  preload never overwrites a newer update pushed at runtime, and a preload re-signed with a
  fresher `iat` DOES overwrite it.
- N5. Claims updates (`$SYS.REQ.CLAIMS.UPDATE` and the per-account form) are served in
  the system account, so any system-account user allowed to publish there may push. The
  handler decodes, validates, refuses an operator-identity issuer when strict, and then
  saves with `save`, not `saveIfNewer`. It does NOT check that the issuer is trusted and
  does not compare `iat` (`server/accounts.go::DirAccResolver.Start`, both handlers). Trust
  is checked when the account is loaded (N1). A new account can be pushed, since the store
  accepts any account public key. "jwt updated" therefore does not prove the JWT will be
  trusted, and an older JWT pushed later replaces a newer one.
- N6. If the system account's JWT cannot be fetched at start, the server registers a stub
  system account whose only signer is the account's own id (`server/server.go::NewServer`,
  "inject a temporary one"). This design does not rely on the stub.
- N7. A change to the trusted operators is not supported by config reload
  (`server/reload.go::diffOptions`, the `trustedkeys` comment), so a new operator JWT needs a
  server restart. Moving to or from a resolver is also refused on reload.
- N8. Account limits missing from an account JWT decode as 0 and are enforced as "none allowed"; every account JWT here sets every limit explicitly.

## 1. The JWTs at first start and who signs each

| JWT | Signed by | Before start? | Who can push it later |
| --- | --- | --- | --- |
| Operator | `signing:ck-bus-operator-root:1` (self-signed: `iss` = `sub` = root `O...` nkey) | Yes: the `operator` config entry. No other path exists. | Nobody. A new one is a ceremony plus a server restart (N7). |
| System account | `signing:ck-bus-operator-root:1` | Yes: preloaded (N4). Without it the sys user cannot connect, and nothing can push. | By rule, nobody at runtime (section 5). |
| Box account (`box_<machine id>`) | `signing:ck-bus-operator-signer:1` | No. ck-bus creates it at first boot. | ck-bus's system-account user, over the claims update (N5). |
| ck-bus system-account user | `signing:ck-bus-sysaccount:1` (new, section 2) | No. It is presented at connect and is never pushed. | Not pushed. ck-bus issues one per boot. |
| ck-bus box-account user | `signing:ck-bus-account:1` | No. | Not pushed. |

Operator JWT content: `sub` and `iss` are the root's `O...` nkey; `signing_keys` is
`[signer O...]`; `system_account` is the system account id (the same value goes in config,
N3); `strict_signing_key_usage` is false. It has to be false because the root signs the
system account JWT (N1); R14's "root out of daily use" is kept by custody (zero grants),
not by the server.

System account JWT content: `sub` is a system account id that setup generates (below);
`signing_keys` is `[sysaccount A...]`; every limit is explicit (N8). No exports or imports
are added: the server adds its own system imports.

Account identity keys. The `sub` of both account JWTs is the public half of an nkey that
is generated in memory and dropped: setup generates the system account's, and ck-bus
generates the box account's. Nobody holds either seed. User JWTs are signed by vault keys
listed in `signing_keys`, with `issuer_account` set (N2), which is the same shape the
harness already uses (`crates/ck-bus/tests/harness/signer/nats.rs::NatsFixture::start`).
Vault keys are not used as account identities, because an account id cannot change:
rotating an identity key would make a new account and strand its JetStream data. A signing
key rotates by re-signing the account JWT.

Why the box account is created by ck-bus and not preloaded. (a) Its name comes from the
machine id, which ck-bus receives from the daemon at HELLO_ACK; setup is not the
authority for it. (b) ck-bus already needs the signer to re-sign this JWT for every
revocation (R14), so creating it gives ck-bus no authority it lacks. (c) The same path
  The box account is created by ck-bus rather than preloaded.

## 2. Vault keys and grants

One new key. Ids use the `signing:<provider>:<generation>` grammar.

| Credential id | Role in NATS | Grants |
| --- | --- | --- |
| `signing:ck-bus-operator-root:1` | operator identity | none; signs by ceremony only |
| `signing:ck-bus-operator-signer:1` | operator signing key | exact `sign` and `read` to `reserved:ckbus` |
| `signing:ck-bus-account:1` | box account signing key | exact `sign` and `read` to `reserved:ckbus` |
| `signing:ck-bus-sysaccount:1` | system account signing key | exact `sign` and `read` to `reserved:ckbus` |

The new key is minted like the account key: `ck auth mint-signing-key --id
signing:ck-bus-sysaccount:1`, then two `ck auth grant --principal reserved:ckbus
--selector-kind exact --selector signing:ck-bus-sysaccount:1 --operation <sign|read>`.
That is a mint, not a root ceremony. `ck setup` gets no grant on anything.

Considered and not proposed: listing `signing:ck-bus-account:1` in the system account's
`signing_keys` as well. That saves one key, but the key that signs every participant would
then be able to mint `$SYS` users, and the two could not be rotated separately. The
foundation also names a separate system-account key. Also not proposed: a federation
account key at install. Its account JWT is pushed by ck-bus later in the same way as the
box account's, and its key (`signing:ck-bus-fedaccount:1`) stays with the federation
slices.

## 3. Ceremonies

Prerequisite: the four keys above exist. The payloads contain the signer's and the
sysaccount key's public halves, so those keys are minted first.

Install: ONE root ceremony with TWO payloads, both signed by
`signing:ck-bus-operator-root:1`:
1. the operator JWT signing input, `<b64url header>.<b64url claims>` as ASCII bytes;
2. the system account JWT signing input, in the same form.

Setup builds both, shows the decoded claims and the sha256 of each signing input for
approval, receives the two signatures, and verifies each one outside the vault against
the pinned root public key before it writes anything. The vault's pure Ed25519 signature
over the payload bytes is exactly a NATS JWT signature once it is re-encoded as base64url
without padding (the `credential.sign` wire shape in the spec's Credentials section).

At ck-bus's first boot (bootstrap, slice 4), with no ceremony:
1. `signing:ck-bus-sysaccount:1` signs ck-bus's system-account user JWT (`issuer_account`
   is the system account id). ck-bus connects to `$SYS`.
2. If the box account does not exist yet: ck-bus generates the account identity in
   memory, records its public half durably, builds the box account JWT (`name`
   `box_<machine id>`, JetStream and connection limits explicit, `signing_keys`
   `[account A...]`), has `signing:ck-bus-operator-signer:1` sign it, pushes it, and reads
   it back with a claims lookup, because the push reply does not prove trust (N5).
3. `signing:ck-bus-account:1` signs ck-bus's box-account user JWT. Bootstrap then goes on
   as the spec says (census bucket, the five streams).

Later, at runtime: the signer signs every box account update (revocations, signing-key
changes), and each boot signs one new sys user and box users as they are issued.

## 4. What setup writes, and where

`<nats dir>` stands for the nats-server module's data directory. The proposal is
`cortexkit_store_types::module_data_dir(<nats-server module id>)`; the daemon configuration names the final
path.

- `<nats dir>/operator.jwt`: the root-signed operator JWT.
- `<nats dir>/server.conf`: `listen: "127.0.0.1:<port>"` (client `--port`, default 14222);
  `http: "127.0.0.1:<monitor_port>"` (`--monitor-port`, default 18222);
  `max_control_line: 65536`; `jetstream { store_dir: "<nats dir>/js" }`;
  `operator: "<nats dir>/operator.jwt"`; `system_account: "<SYS id>"`;
  `resolver { type: full, dir: "<nats dir>/jwt", allow_delete: false }`;
  `resolver_preload { <SYS id>: "<system account JWT>" }`. Both listeners always bind
  IPv4 loopback, never a wildcard or IPv6 host; the two ports must differ. There is no
  local TLS block (see 6.10).
  The HTTP listener exists for the daemon's plain `/healthz` readiness probe (200 when
  ready, 503 during storage recovery). NATS monitoring also exposes server, connection,
  account and JetStream statistics without authentication: any local process can read
  them under the existing local trust model. `install-apply` prints `health_url`, e.g.
  `http://127.0.0.1:18222/healthz`, for a `protocol: "none"` module's
  `health: { http: "<health_url>", cadence_ms, deadline_ms }` in `subc.jsonc`.
- `<nats dir>/jwt/`: created empty. The server stores the preload there at start (N4),
  and runtime pushes land there too. After creating it, setup never writes, rewrites or
  deletes anything inside it.
- Setup's pinned inputs: the root public key (already pinned), plus the signer's and the
  sysaccount key's public halves (proposed; 6.8 covers where they come from).
- An input for ck-bus: the path of `operator.jwt` (the source of the system account id and
  the signer public keys), the server URL and the TLS pin. See 6.5.

`install-plan` records JWT signing inputs, not listener ports. Normal `install-apply`
chooses the ports from flags/defaults, verifies supplied or stored JWTs, and atomically
rewrites **both** `operator.jwt` and `server.conf`, even when the JWT bytes are kept.
It does not regenerate keys or the system account id, and creates `jwt/` only if absent;
it never rewrites existing resolver contents. Supplying new signatures can replace JWTs.

For a monitoring-only upgrade of an existing rendered configuration, run
`"${XDG_DATA_HOME:-$HOME/.local/share}/cortexkit/bin/ck-bus" install-apply --conf-only --nats-dir "<nats dir>" [--monitor-port <port>]`.
The existing `server.conf` is the recorded configuration: its install-apply header and
IPv4-loopback `listen` value are required; hand-written configurations are refused.
Only the `http` line changes, inserted directly after `listen` if absent. An identical
value leaves the file and its mtime untouched (`status: "unchanged"`); a different
existing port is replaced only with an explicit `--monitor-port`. A non-loopback
existing `http` value is always refused. The atomic replacement keeps the file's mode
and touches no other install files. This path requires no keys or signatures and also
prints `health_url`. Restart the supervised nats-server to activate a changed listener;
the command neither restarts it nor edits `subc.jsonc`.

No seed is written: the four roots' private halves never leave the vault; the system
account identity seed exists only in setup's memory and is dropped after its public half
is read; the box account identity seed and every user seed exist only in ck-bus's memory.
Everything setup writes is a JWT (public claims plus a signature) or a public nkey.
Proposed setup check: no file under `<nats dir>` or setup's config contains an nkey seed
(a string that decodes as an nkey with the `S` seed prefix). The one exception is the TLS
listener's private key, which is not part of the NATS trust chain (6.10).

## 5. Re-run, rotation, reinstall

Second `ck setup` with everything present. Setup reads `operator.jwt` and the preloaded
system account JWT, verifies both signatures against the pinned root, and compares their
claims, excluding `iat` and `jti`, with what it would build from the current pinned
inputs. If they match, setup re-renders `server.conf` from the same stored JWTs, byte for
byte, so there is no change, no restart and no ceremony. The system account id is read
back from the stored JWT, never generated again. If they differ, the plan names the
payloads that need a ceremony and writes nothing until they are signed. Setup never
re-signs a JWT only to refresh it: a fresher `iat` in a preload replaces what is on disk
(N4).

Invariant: the system account JWT is written only by the root ceremony. ck-bus never
pushes an update to it. Its trust therefore never depends on the signer, and ck-bus can
still reach `$SYS` after the signer is lost or removed. One consequence: a previous
incarnation's system-account users are not revoked through the revocation list. Their
seeds died with that process, so their JWTs cannot pass a nonce challenge, and they expire
once `user-jwt-ttl-unpinned` is pinned. This differs from the spec (6.7).

- Box account key (`signing:ck-bus-account:1 --replace`): no ceremony. ck-bus sees the
  new `key_id`, re-signs the box account JWT with the signer, listing the new key and
  dropping the old one, pushes it, and re-issues users, as the spec already requires.
- Operator signer (`--replace`): a one-payload root ceremony producing a new operator JWT
  with `signing_keys` `[new]`, followed by a server restart (N7). After the restart, the box
  account JWT signed by the old signer is untrusted (N1), so box users are refused. The
  system account is still trusted (root-signed), so ck-bus connects, reads the box account
  JWT with a claims lookup (the lookup returns the stored file whatever its trust),
  re-signs it with the new signer, carrying its revocations over, and pushes it. The
  outage lasts from the restart until that push. For a planned rotation without an outage,
  a transitional operator JWT listing `[old, new]` avoids it, at the cost of a second
  ceremony to drop the old key.
- Sysaccount key (`--replace`): a one-payload root ceremony (a system account JWT listing
  the new key), and ck-bus re-issues its sys user.
- Root key: a new trust root and a new pin. It is a two-payload ceremony, like install,
  plus a restart. The account ids stay the same; ck-bus re-signs the box account with the
  signer, which the new operator JWT must list.
- Machine-id change (a new `{acct}`): ck-bus creates a second box account through the
  signer, as at first boot. No ceremony and no new keys.

## 7. Installation requirements

The `6.x` labels below identify individual installation rules referenced by the earlier sections (for example the loopback/TLS rule `6.10`). Repeat the root-signing procedure in §3 only when the operator or system-account record changes.

- 6.1: the root-credential table above and `RootCredential` distinguish the operator identity key from its signing key; ck-bus never requests vault access to the operator identity key.
- 6.2: the signer requires an exact `read` grant to `reserved:ckbus`. Matching
  `key_id` against the operator JWT is a second path to the same fact, and it would silently
  disagree the day the id derivation changes.
- 6.3: `signing:ck-bus-sysaccount:1` requires exact `sign` and `read` grants to `reserved:ckbus`.
- 6.4: ck-bus creates the box account through the signer.
- 6.5: ck-bus reads the operator JWT path, the server URL and the system
  account id from its supervised config (env), and `account.json` gains the box account's
  public id. An ABSENT `account.json` triggers a lookup by account name before any create,
  and a found account is adopted, never duplicated.
- 6.6: the system grant includes claims lookup and `_INBOX.<cred>.>` subscription.
- 6.7: ck-bus does not revoke its own previous system-account users; Their seeds died with the process, so they can't answer a
  nonce challenge.
- 6.8: the signer and sysaccount public halves are pinned from mint output beside
  the root pin. ck-bus refuses to start (health down, naming the mismatch) when the vault
  disagrees with what the JWTs list.
- 6.10: no TLS on the local listener. It binds loopback only, and every client authenticates
  by JWT nonce challenge, the same trust boundary as the daemon's loopback HMAC. TLS
  belongs on the leaf link to the hub.
- 6.11: setup asks the daemon to restart the nats-server module (`supervisor.restart`) after
  writing a new operator JWT, and verifies the new process.
- 6.12: no new root keys for a new `{acct}`.
- N5 requires that only ck-bus's system user hold claims-update permission, and every push is read back through lookup before ck-bus treats it as applied, because updates are saved without a trust or `iat` check.

## 8. Installation default

`install-apply` defaults the local listener to port 14222. That avoids a developer's own
nats-server on the stock 4222 and stays below every common ephemeral port range.
