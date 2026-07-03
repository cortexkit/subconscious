# npm Trusted Publishing (OIDC) for the monorepo's npm packages

Goal: publish the monorepo's npm packages (`@cortexkit/subc-client`, `@cortexkit/store`) from CI with **no NPM_TOKEN and no manual
passkey** — every release becomes a tag push. Replaces the passkey-gated manual
`npm publish` that has gated every release since the 0.1.0 bootstrap.

How it works: the release workflow (`.github/workflows/release-npm.yml`) mints a
short-lived GitHub OIDC token; npm verifies that token against a **Trusted
Publisher** you configure once on the package, scoped to this exact repo +
workflow file. No long-lived secret exists to leak. Provenance attestation is
generated automatically (supply-chain win).

## One-time setup (requires Ufuk, ~2 min, at a computer with npm login)

The package already exists on npm (0.2.0), so this is pure configuration — no
publish needed to do it.

1. Log in to npmjs.com (the passkey step — but only THIS once, ever).
2. Go to the package: https://www.npmjs.com/package/@cortexkit/subc-client
3. Settings → **Trusted Publisher** → Add GitHub Actions publisher:
   - Organization/user: `cortexkit`
   - Repository: `subconscious`
   - Workflow filename: `release-npm.yml`  (exactly this, not a path)
   - Environment: leave blank (we don't gate on a GH environment)
4. Save.

That's it. From then on, a tag push publishes with zero interactive auth.

Note: because the org is 2FA-enforced, also confirm the package's publish
setting allows "automation / trusted publishing" (Settings → "Require
two-factor authentication or automation tokens" is compatible with trusted
publishing — OIDC counts as automation and satisfies 2FA).

### Provenance is OFF on purpose (Blacksmith constraint)

The workflow sets `NPM_CONFIG_PROVENANCE=false`. sigstore provenance attestation
is only accepted by npm from **github-hosted** runners; this repo runs on
Blacksmith (self-hosted class, forced by the free-plan private-repo Actions
block). OIDC trusted-publishing AUTH works fine on Blacksmith — only the
provenance bundle is rejected (`E422: Only github-hosted runners are
supported`). npm >= 11.5.1 auto-enables provenance under trusted publishing, so
we must override it off explicitly. If this repo ever moves to github-hosted
runners (e.g. goes public), drop the override to regain provenance.

## Releasing (after the one-time setup)

```
# bump the version in clients/subc-client/package.json, commit, then:
git tag subc-client@0.2.1
git push origin subc-client@0.2.1
```

The tag format is `subc-client@<version>` (npm-style) — deliberately NOT
`<name>-v<version>`, so it never triggers the crates.io `release.yml` (which
matches `*-v*`). The workflow verifies types + tests, asserts the tag matches
`package.json`, then publishes via OIDC. A retag of an already-published version
is treated as success (idempotent recovery).

## Why this over a token

An npm automation token would also work from CI, but it's a long-lived secret
sitting in repo secrets — the exact leak surface trusted publishing removes.
OIDC tokens live for the job only and are cryptographically scoped to this repo
+ workflow, so a stolen CI log or a compromised unrelated workflow cannot
publish. Same reasoning that put the Rust crates on their own release pipeline.


## Adding a package (the @cortexkit/store bootstrap, and any future one)

Trusted Publishers are configured PER PACKAGE, and npm only lets you configure
one on a package that already EXISTS. So a brand-new package needs a one-time
manual bootstrap publish (passkey), then the same one-time Trusted Publisher
setup, then it's tag-push forever:

1. Bump the version in `clients/<pkg>/package.json` (already done for store: 0.1.0).
2. Bootstrap publish (passkey, once): `cd clients/store && npm publish --access public`
3. Configure Trusted Publisher on npmjs.com for @cortexkit/store:
   org `cortexkit`, repo `subconscious`, workflow `release-npm.yml`, env blank.
4. Future releases: bump version, commit, `git tag store@<version>`, push the tag.

The workflow maps tag prefixes to package dirs (`subc-client@*` ->
clients/subc-client, `store@*` -> clients/store); adding a future package =
one tag pattern + one case arm + its npm-side Trusted Publisher.
