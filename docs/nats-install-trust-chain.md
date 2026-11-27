# The NATS trust chain at install time: who signs what, and when

Scope: the local `nats-server` install-time configuration and the keys and signatures it depends on.

On a setup-managed bus, `ck setup` uses the vault for the runtime signing keys,
but signs the install-time JWTs itself with a one-time operator root.
`signing:msgsig` is not part of this installation trust chain.

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
| Operator | Setup's one-time operator root (self-signed: `iss` = `sub` = root `O...` nkey) | Yes: the `operator` config entry. No other path exists. | Nobody. A replacement requires a fresh root and a server restart (N7). |
| System account | The same one-time operator root | Yes: preloaded (N4). Without it the sys user cannot connect, and nothing can push. | Nobody at runtime (§5). |
| Box account (`box_<machine id>`) | `signing:ck-bus-operator-signer:1` | No. ck-bus creates it at first boot. | ck-bus's system-account user, over the claims update (N5). |
| ck-bus system-account user | `signing:ck-bus-sysaccount:1` | No. It is presented at connect and is never pushed. | Not pushed. ck-bus issues one per boot. |
| ck-bus box-account user | `signing:ck-bus-account:1` | No. | Not pushed. |

Operator JWT content: `sub` and `iss` are the root's `O...` nkey; `signing_keys` is
`[signer O...]`; `system_account` is the system account id (the same value goes in config,
N3); `strict_signing_key_usage` is false. It has to be false because the root signs the
system account JWT (N1). The root stays out of daily use because setup generates
its private half in memory, signs both JWT inputs, and discards it. The private
half never reaches the vault, a file, a log, a command line or a child process's
environment. Setup records only public keys.

System account JWT content: `sub` is a system account id that setup generates (below);
`signing_keys` is `[sysaccount A...]`; every limit is explicit (N8). No exports or imports
are added: the server adds its own system imports.

Account identity keys. The `sub` of both account JWTs is the public half of an nkey that
is generated in memory and dropped: `install-plan` creates the system account's,
and ck-bus creates the box account's. Nobody holds either seed. User JWTs are
signed by vault keys listed in `signing_keys`, with `issuer_account` set (N2).
The harness uses the same shape (`crates/ck-bus/tests/harness/signer/nats.rs::NatsFixture::start`).
Vault keys are not used as account identities, because an account id cannot change:
rotating an identity key would make a new account and strand its JetStream data. A signing
key rotates by re-signing the account JWT.

Why the box account is created by ck-bus and not preloaded. (a) Its name comes from the
machine id, which ck-bus receives from the daemon at HELLO_ACK; setup is not the
authority for it. (b) ck-bus already needs the signer to re-sign this JWT for every
revocation (R14), so creating it gives ck-bus no authority it lacks.

## 2. Keys and vault grants

The operator root is not a vault key. Vault credential IDs use the
`signing:<provider>:<generation>` grammar.

| Key | Role in NATS | Grants |
| --- | --- | --- |
| Setup's one-time root (no credential ID) | operator identity; signs the operator and system account JWTs | none; never stored in the vault |
| `signing:ck-bus-operator-signer:1` | operator signing key | exact `sign` and `read` to `reserved:ckbus` |
| `signing:ck-bus-account:1` | box account signing key | exact `sign` and `read` to `reserved:ckbus` |
| `signing:ck-bus-sysaccount:1` | system account signing key | exact `sign` and `read` to `reserved:ckbus` |

Setup mints missing vault keys with `ck auth mint-signing-key --id <id>` and
does not replace or remove existing keys. It reads the existing signer and
system-account signing key through `ck auth public-key --id <id>`. It ensures
two grants per vault key, one for each operation, with `ck auth grant
--principal reserved:ckbus --selector-kind exact --selector <id>
--operation <sign|read>`. Setup itself receives no signing grant.

Considered and not proposed: listing `signing:ck-bus-account:1` in the system account's
`signing_keys` as well. That saves one key, but the key that signs every participant would
then be able to mint `$SYS` users, and the two could not be rotated separately. The
foundation also names a separate system-account key. Also not proposed: a federation
account key at install. Its account JWT is pushed by ck-bus later in the same way as the
box account's, and its key (`signing:ck-bus-fedaccount:1`) stays with the federation
slices.

## 3. Automatic root signing

Setup records a pending `bus-trust` row before minting on a fresh install. Once
the three vault keys and their grants are in place, setup generates a fresh
Ed25519 root in memory. It gives `ck-bus install-plan` only the public halves of
the root, the operator signer and the system account signing key.

The plan must return two JWT signing inputs (`<b64url header>.<b64url claims>`
as ASCII bytes): one for the operator and one for the system account. Setup
checks both file paths and SHA-256 digests before it signs either input.
Signing is automatic, with no separate approval step or vault signing call.
Setup drops the root's private half and passes the two signatures (unpadded
base64url) to `ck-bus install-apply`, which verifies them before writing the
JWTs. Setup checks the install files for NATS seeds before recording the three
public halves as `applied`.

At ck-bus's first boot, with no further root signing:
1. `signing:ck-bus-sysaccount:1` signs ck-bus's system-account user JWT (`issuer_account`
   is the system account id). ck-bus connects to `$SYS`.
2. If the box account does not exist yet: ck-bus generates the account identity in
   memory, records its public half durably, builds the box account JWT (`name`
   `box_<machine id>`, JetStream and connection limits explicit, `signing_keys`
   `[account A...]`), has `signing:ck-bus-operator-signer:1` sign it, pushes it, and reads
   it back with a claims lookup, because the push reply does not prove trust (N5).
3. `signing:ck-bus-account:1` signs ck-bus's box-account user JWT. ck-bus then
   sets up the census bucket, streams and durables.

Later, at runtime: the signer signs every box account update (revocations, signing-key
changes), and each boot signs one new sys user and box users as they are issued.

## 4. What setup writes, and where

For a setup-managed bus, `<nats dir>` is `<data home>/cortexkit/nats`.
Setup uses the default ports below and refuses a fresh install if either is
already in use.

- `<nats dir>/operator.jwt`: the root-signed operator JWT.
- `<nats dir>/system_account`: the system account's public ID, reused across
  restarts and root replacements.
- `<nats dir>/server.conf`: `listen: "127.0.0.1:<port>"` (client `--port`, default 14222);
  `http: "127.0.0.1:<monitor_port>"` (`--monitor-port`, default 18222);
  `max_control_line: 65536`; `jetstream { store_dir: "<nats dir>/js" }`;
  `operator: "<nats dir>/operator.jwt"`; `system_account: "<SYS id>"`;
  `resolver { type: full, dir: "<nats dir>/jwt", allow_delete: false }`;
  `resolver_preload { <SYS id>: "<system account JWT>" }`. Both listeners always bind
  IPv4 loopback, never a wildcard or IPv6 host; the two ports must differ. There is no
  local TLS block (see 6.10).
  NATS monitoring exposes server, connection, account and JetStream statistics
  without authentication: any local process can read them under the existing
  local trust model. A hand-made install that supervises the stock server with
  `protocol: "none"` can use the `health_url` printed by `install-apply` (for
  example, `http://127.0.0.1:18222/healthz`) as `health.http` in `subc.jsonc`.
- `<nats dir>/jwt/`: created empty. The server stores the preload there at start (N4),
  and runtime pushes land there too. After creating it, setup never writes, rewrites or
  deletes anything inside it.
- The retained `bus-trust` inventory row, keyed by `<nats dir>`, records the root,
  signer and system-account signing public keys once applied, along with
  `activation_pending`. A pending row has empty public-key fields.
- In `subc.jsonc`, `modules.nats-server` runs the managed `ck-nats-server`
  wrapper with `-c <nats dir>/server.conf --subc <daemon connection file>`
  and `protocol: "subc"`. The wrapper reports readiness through the daemon's
  module health check rather than a `health.http` entry.
- `modules.ckbus` runs the managed `ck-bus` with the `env` returned by
  `install-apply`: `CKBUS_NATS_URL`, `CKBUS_OPERATOR_JWT` and
  `CKBUS_SYSTEM_ACCOUNT`. Both entries are enabled, reserved and have a
  registration channel.

`install-plan` records JWT signing inputs, not listener ports. Normal `install-apply`
chooses the ports from flags/defaults, verifies supplied or stored JWTs, and atomically
rewrites **both** `operator.jwt` and `server.conf`, even when the JWT bytes are kept.
It does not regenerate keys or the system account id, and creates `jwt/` only if absent;
it never rewrites existing resolver contents. Supplying new signatures can replace JWTs.

For a monitoring-only change to a hand-made install with an existing rendered
configuration, run
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

The three runtime signing keys stay in the vault. The system account identity
seed is discarded after its public ID is read; the box account identity seed and
each user seed exist only in ck-bus's memory. After signing and before marking
the row `applied`, setup scans `server.conf`, `operator.jwt`, `system_account`
and the two signing inputs for NATS seeds. A hit names the file without printing
its contents, leaves the row pending, and prevents activation.

## 5. Re-run, rotation, reinstall

On a rerun with matching vault public keys and an applied `bus-trust` row,
`install-plan` verifies both stored JWTs against the recorded root and compares
their claims, ignoring `iat` and `jti`. If neither needs to change, setup calls
`install-apply` without signatures to reuse both JWTs and render `server.conf`.
A settled rerun leaves the record, both entries and `server.conf` byte-identical
at the end: no mint, signature, grant change, entry write, rescan or restart.

Any change that requires replacing either install JWT gets a fresh root and
two new signatures, even if the check with the old root asked for only one.
Setup replaces the `bus-trust` record with the new public halves. The system
account ID stays the same. On restart NATS loads the new system account JWT
from the preload under the same ID (N4); setup does not delete resolver files
or JetStream data.

Setup commits both module entries together if they need changes, then asks the
daemon to rescan when activation needs it. The rescan starts newly added or
re-enabled modules. A trust change restarts already supervised modules that the
rescan did not start, `nats-server` first and then `ckbus`. Setup waits for
both modules to be live and healthy within one 180-second budget and checks
that a restarted module has a new PID. ck-bus retries its first connection
while NATS comes up.

A pending row or mismatched signer keys lead to a fresh root on the next run.
If signing is complete but activation was interrupted, the applied row keeps
`activation_pending` set until both modules are ready. An activation retry
with unchanged trust files can then finish without signing again. Uninstall
keeps the row, the NATS directory and both module entries, so reinstall can
resume setup's ownership instead of treating the bus as hand-made.

ck-bus never pushes updates to the system account JWT. It is signed by setup's
root, not the vault-held operator signer, so losing the signer does not itself
invalidate system-account access. Previous system-account users are not revoked
through the account JWT: their seeds died with the process and cannot answer
a nonce challenge (6.7).

- Box account signing key: no new operator root. ck-bus sees the
  new `key_id`, re-signs the box account JWT with the signer, listing the new key and
  dropping the old one, pushes it, and re-issues users, as the spec already requires.
- Operator signer: setup signs both JWTs with a fresh root and lists the new
  signer in the operator JWT. After restart the box account JWT signed by the
  old signer is no longer trusted (N1). ck-bus connects to the system account,
  re-signs the box account JWT with the new signer while preserving revocations,
  and pushes it. Box users are refused until that push takes effect.
- System account signing key: setup also signs both JWTs with a fresh root,
  listing the new key in the system account JWT. ck-bus issues its system user
  with that key when it restarts.
- A new box account name does not by itself require new bus keys (6.12).

## 7. Installation requirements

The `6.x` labels below identify individual installation rules referenced by the earlier sections (for example the loopback/TLS rule `6.10`). After installation, §5 governs when setup repeats the root-signing procedure in §3.

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
- 6.7: ck-bus does not revoke its own previous system-account users. Their
  seeds died with the process, so they can't answer a nonce challenge.
- 6.8: the signer and sysaccount public halves are read from mint output or
  `ck auth public-key` and recorded beside the root pin. ck-bus reports health
  down and names the mismatch when the vault disagrees with what the JWTs list.
- 6.10: no TLS on the local listener. It binds loopback only, and every client authenticates
  by JWT nonce challenge, the same trust boundary as the daemon's loopback HMAC. TLS
  belongs on the leaf link to the hub.
- 6.11: after replacing the trust files, setup activates `nats-server` and
  `ckbus` and verifies readiness as described in §5.
- 6.12: no new root keys for a new `{acct}`.
- N5 requires that only ck-bus's system user hold claims-update permission, and every push is read back through lookup before ck-bus treats it as applied, because updates are saved without a trust or `iat` check.

## 8. Installation default

`install-apply` defaults the local listener to port 14222. That avoids a developer's own
nats-server on the stock 4222 and stays below every common ephemeral port range.
