# cortexkit.io worker

Serves the install script mirror and the signed release index at the
`cortexkit.io` apex. Worker name stays `ck-install` so the deployed worker
and its custom domain are unchanged.

Do not deploy from this checkout; the operator deploys.

## Secrets

```bash
wrangler secret put GITHUB_WEBHOOK_SECRET
wrangler secret put ADMIN_TOKEN
wrangler secret put RELEASE_INDEX_SIGNING_KEY
wrangler secret put GITHUB_APP_ID
wrangler secret put GITHUB_APP_INSTALLATION_ID
wrangler secret put GITHUB_APP_PRIVATE_KEY
```

| Secret | Source |
| --- | --- |
| `GITHUB_WEBHOOK_SECRET` | The secret configured on the org webhook below. |
| `ADMIN_TOKEN` | Operator-chosen bearer for `/releases/v1/reingest`, `/releases/v1/status`, and `/releases/v1/refusals.json`. |
| `RELEASE_INDEX_SIGNING_KEY` | PKCS#8 Ed25519 PEM from claustrum vault record `cortexkit:release-index-signing:1:ed25519-pem`. |
| `GITHUB_APP_ID` | GitHub App `cortexkit-ci` (4124360). |
| `GITHUB_APP_INSTALLATION_ID` | Org installation 142118098. |
| `GITHUB_APP_PRIVATE_KEY` | PKCS#8 or PKCS#1 RSA PEM from claustrum vault record `github_app:cortexkit-ci`. |

A PAT is a long-lived user credential in a Worker secret. The App installation
token is org-owned, short-lived, and scoped by the installation.

## KV

```bash
wrangler kv namespace create RELEASE_INDEX
```

Put the returned id in `wrangler.toml` under the `RELEASE_INDEX` binding.

## Durable Object migration

The release-index coordinator is a Durable Object. The first deploy after this
change must use `wrangler deploy` so Wrangler applies its `new_classes`
migration; do not deploy from this checkout.

## Org webhook

Create an organization webhook on `cortexkit`:

- Payload URL: `https://cortexkit.io/webhooks/github`
- Content type: `application/json`
- Secret: the same value as `GITHUB_WEBHOOK_SECRET`
- SSL verification: enabled
- Events: **Let me select individual events** → **Releases** (`published`, `edited`, `deleted`)
- Active: yes

## Cron

Daily rebuild at `0 4 * * *` (04:00 UTC), in addition to the webhook and
`POST /releases/v1/reingest` (`Authorization: Bearer <ADMIN_TOKEN>`). All three
sources queue the same single-flight coordinator; reingest starts immediately,
while webhook and cron requests use a 30-second debounce.
