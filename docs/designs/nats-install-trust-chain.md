# The NATS trust chain at install time: who signs what, and when

Status: DESIGN, for review, then for CKCRED as the list of keys and ceremonies.
Scope: the local `nats-server`'s install-time configuration, which `ck setup` writes
(`server-config-writer-unnamed` in `docs/specs/ck-bus-module.md` r2, now assigned to SUBC),
and the keys and signatures it depends on. Nothing here is implemented.

Keys that exist today (Ed25519, claustrum vault, minted 2026-09-24):
`signing:ck-bus-operator-root:1` (operator identity, zero grants, ceremony only, public half
pinned in setup's config), `signing:ck-bus-operator-signer:1` (operator signing key, `sign`
to `reserved:ckbus`, ruling R14), `signing:ck-bus-account:1` (`sign` and `read` to
`reserved:ckbus`, signs user JWTs). `signing:msgsig` is not involved. `ck setup` reaches
the vault as a `direct` principal, so it can sign only through the root ceremony.

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
- N8. Account limits missing from an account JWT decode as 0 and are enforced as "none
  allowed" (rig finding, `docs/designs/nats-federation-rig-results.md`, Common
  configuration). Every account JWT here sets every limit explicitly.

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
then covers a machine-id change (a new `{acct}`, open question 8) and the federation
account, with no ceremony. (d) The root signs one payload fewer. The cost is a
contradiction with the foundation's install step (4), listed in section 6.

## 2. Vault keys and grants

One new key. Ids use the `signing:<provider>:<generation>` grammar.

| Credential id | Status | Role in NATS | Grants |
| --- | --- | --- | --- |
| `signing:ck-bus-operator-root:1` | exists | operator identity | none; signs by ceremony only |
| `signing:ck-bus-operator-signer:1` | exists | operator signing key | `sign` to `reserved:ckbus` (exists); `read` proposed, see 6.2 |
| `signing:ck-bus-account:1` | exists | box account signing key | `sign` and `read` to `reserved:ckbus` (exist) |
| `signing:ck-bus-sysaccount:1` | NEW, proposed id | system account signing key | `sign` and `read` to `reserved:ckbus`, each an exact selector |

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
`cortexkit_store_types::module_data_dir(<nats-server module id>)`; SUBC names the final
path.

- `<nats dir>/operator.jwt`: the root-signed operator JWT.
- `<nats dir>/server.conf`: loopback `listen`; `jetstream { store_dir: "<nats dir>/js" }`;
  `operator: "<nats dir>/operator.jwt"`; `system_account: "<SYS id>"`;
  `resolver { type: full, dir: "<nats dir>/jwt", allow_delete: false }`;
  `resolver_preload { <SYS id>: "<system account JWT>" }`; and the TLS block the foundation
  requires (see 6.10).
- `<nats dir>/jwt/`: created empty. The server stores the preload there at start (N4),
  and runtime pushes land there too. After creating it, setup never writes, rewrites or
  deletes anything inside it.
- Setup's pinned inputs: the root public key (already pinned), plus the signer's and the
  sysaccount key's public halves (proposed; 6.8 covers where they come from).
- An input for ck-bus: the path of `operator.jwt` (the source of the system account id and
  the signer public keys), the server URL and the TLS pin. See 6.5.

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

## 6. Unagreed or contradictory, listed and not resolved

1. Key naming. The spec's Credentials table and `crates/ck-bus/src/credentials/roots.rs`
   (`RootCredential::Operator`) have one operator key, `signing:ck-bus-operator:1`, which
   signs the account JWTs. The minted reality is two keys, root and signer. The table
   should name both, and the root should get no ck-bus row.
2. The signer's grants. The spec says ck-bus needs `read` as well as `sign` on every key
   for `credential.public_key`. The signer has `sign` only, and ck-bus needs its public key
   to write the account JWT's `iss`. Two ways to meet it: grant `read` (CKCRED), or match
   the sign reply's `key_id` (`sha256(public)[..8]`) against the operator JWT's
   `signing_keys`. Not chosen here.
3. `signing:ck-bus-sysaccount:1`: its id and its two grants are this document's proposal,
   unagreed with CKCRED. The spec lists the id "by analogy".
4. Foundation install step (4) has the installer sign "both account JWTs" at install.
   This design has the root sign only the system account, and ck-bus create the box
   account through the signer. The foundation's owner (ALF) and CKCRED must agree.
5. ck-bus's inputs. The spec gives ck-bus no channel for the operator JWT path, the system
   account id, the server URL or the TLS pin. The spec's "eight shapes and nothing else"
   store also has no field for the box account's public id: `account.json` would have to
   carry it. Also open: an ABSENT (not damaged) `account.json` on a server that already has
   a box account must not create a second account. Absence is neutral, so ck-bus would
   first have to find the existing one (for example by a claims list and the account
   `name`). The rule is unwritten.
6. The sys-user grant (`permission_golden.txt`: `$SYS.REQ.CLAIMS.UPDATE`, the kick,
   connect and disconnect) has no claims-lookup subject, although the spec's revocation
   step and this design both read the account JWT back through a lookup. It also has no
   inbox subscription for the replies. A9 has to settle both. The generic update subject
   does cover pushing a new account.
7. The spec's bootstrap says ck-bus revokes the previous incarnation's own users, which
   includes system-account users. That would make ck-bus re-sign the system account and
   break section 5's invariant. This design proposes not revoking them (seeds gone,
   expiry pending).
8. Where setup gets the signer's and sysaccount key's public halves. Setup is `direct` and
   holds no `read` grant. The proposal is to pin them from `mint-signing-key`'s printed
   output next to the root pin, and to have ck-bus refuse to start (health down, naming
   the mismatch) when the vault's public keys differ from what the operator JWT and the
   system account JWT list. CKCRED may prefer a direct read op.
9. The ceremony's shape for two payloads: one approval handle over two sha256 values, or
   two handles in one session. CKCRED's to define.
10. TLS. The foundation's install step (4) also writes TLS material for the local
    listener. Stock `nats-server` reads its TLS private key from a file, so "no private
    key on disk" holds only for the NATS trust chain. Whether a loopback listener needs
    TLS, and who owns that key, is open (SUBC, ALF).
11. `server-config-writer-unnamed` is still "unnamed" in the spec, but is now assigned to
    SUBC. The spec also needs a named restart path: a new operator JWT needs a server
    restart (N7), and setup has to ask the daemon for it, since nats-server is a supervised
    sibling.
12. Open question 8 (does a new `{acct}` need new root keys): this design's answer is no,
    because the new account lists the same account signing key. Sharing that key across
    the old and new accounts is a choice for CKCRED and the operator to confirm.

## 7. SUBC's decisions on section 6 (2026-09-24)

Outcome, read from the vault by CKCRED on 2026-09-25: 6.2 and 6.3 were granted as proposed
on 2026-09-24 (`reserved:ckbus` holds exact `sign` and `read` on
`signing:ck-bus-operator-signer:1` and on `signing:ck-bus-sysaccount:1`), and the ceremony
for the operator and system-account roots ran the same day in one session with one handle
and two approvals (vault seq 18413-18416). No further ceremony is needed unless the
operator or system-account record changes.

- 6.1: the spec's Credentials table and `RootCredential` get two operator rows, root and
  signer, and the root gets no ck-bus row. That lands with slice 4.
- 6.2: ask CKCRED for an exact `read` grant on the signer to `reserved:ckbus`. Matching
  `key_id` against the operator JWT is a second path to the same fact, and it would silently
  disagree the day the id derivation changes.
- 6.3: proposed to CKCRED as written: `signing:ck-bus-sysaccount:1`, exact `sign` and `read`
  to `reserved:ckbus`.
- 6.4: agreed, ck-bus creates the box account through the signer. That needs ALF to amend
  the foundation's install step (4).
- 6.5: slice 4 owns it. ck-bus reads the operator JWT path, the server URL and the system
  account id from its supervised config (env), and `account.json` gains the box account's
  public id. An ABSENT `account.json` triggers a lookup by account name before any create,
  and a found account is adopted, never duplicated.
- 6.6: already settled. commons `bd7f699` adds the claims lookup and `_INBOX.<cred>.>`
  subscribe to the system grant, and ck-bus re-pins in slice 4.
- 6.7: agreed, ck-bus does not revoke its own previous system-account users; the spec's
  bootstrap wording is amended. Their seeds died with the process, so they can't answer a
  nonce challenge.
- 6.8: agreed. The signer and sysaccount public halves are pinned from mint output beside
  the root pin. ck-bus refuses to start (health down, naming the mismatch) when the vault
  disagrees with what the JWTs list.
- 6.9: CKCRED's call.
- 6.10: no TLS on the local listener. It binds loopback only, and every client authenticates
  by JWT nonce challenge, the same trust boundary as the daemon's loopback HMAC. TLS
  belongs on the leaf link to the hub, which is a federation slice. That needs ALF to amend
  the foundation's install step (4).
- 6.11: setup asks the daemon to restart the nats-server module (`supervisor.restart`) after
  writing a new operator JWT, and verifies the new process. The spec gate is renamed to
  SUBC.
- 6.12: agreed, no new root keys for a new `{acct}`.
- N5 is a security fact the slices must carry: a claims update is saved without a trust or
  `iat` check. So only ck-bus's system user may hold the claims-update permission, and every
  push is read back through the lookup before ck-bus treats it as applied.

## 8. As installed on the first box (2026-09-24)

The root ceremony ran once, as section 3 describes: two approvals (one over each payload's
sha256), one handle, two signatures, then revocation. `ck-bus install-plan` built the operator
JWT and the system account JWT; `ck-bus install-apply` verified both signatures against the
pinned root before writing `operator.jwt` and `server.conf`. A second `install-plan` reports "no
ceremony needed".

The system account's own identity key is not a vault key. `install-plan` generates it in memory,
keeps only the public half (`<nats dir>/system_account` and the JWTs), and drops the seed, so
nobody holds the private half. An account identity key only ever signs its account's own claims,
and here the root signs those, so the seed has no job. Users are signed by the sysaccount signing
key in the vault. Re-keying the system account means a new root ceremony, which is intended.

`install-apply` defaults the local listener to port 14222. That avoids a developer's own
nats-server on the stock 4222 and stays below every common ephemeral port range.
