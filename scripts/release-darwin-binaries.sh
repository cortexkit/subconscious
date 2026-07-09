#!/usr/bin/env bash
# Build + upload the darwin-arm64 daemon binary assets for a subc-core release
# tag. The CI half of the release (release.yml `binaries` job) covers
# linux-x64; macOS cannot run there (Blacksmith has no macOS runners and
# GitHub-hosted macOS is billing-blocked for this private free-plan org), so
# the darwin assets are produced on the dev box against the SAME tag.
#
# Usage: scripts/release-darwin-binaries.sh subc-core-v0.1.0
set -euo pipefail

TAG="${1:?usage: $0 subc-core-v<version>}"
TARGET_NAME="darwin-arm64"
BINS=(subc-core subc-probe ck fake-aft-stub subc-mcp)
REPO="cortexkit/subconscious"

[[ "$TAG" =~ ^subc-core-v([0-9].*)$ ]] || { echo "tag must be subc-core-v<version>" >&2; exit 1; }
VERSION="${BASH_REMATCH[1]}"
CARGO_VERSION=$(grep '^version' crates/subc-core/Cargo.toml | head -1 | sed -E 's/version *= *"([^"]+)"/\1/')
[[ "$VERSION" == "$CARGO_VERSION" ]] || { echo "tag wants ${VERSION} but Cargo.toml has ${CARGO_VERSION}" >&2; exit 1; }

# Build from the tagged commit, never the working tree, so the uploaded
# binaries provably match the tag CI verified.
git diff --quiet || { echo "working tree dirty; commit or stash first" >&2; exit 1; }
git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null || { echo "tag ${TAG} not found locally" >&2; exit 1; }
[[ "$(git rev-parse HEAD)" == "$(git rev-parse "${TAG}^{commit}")" ]] || {
  echo "HEAD is not at ${TAG}; checkout the tag first" >&2; exit 1;
}

cargo build --release -p subc-core -p subc-mcp \
  --bin subc-core --bin subc-probe --bin ck --bin fake-aft-stub --bin subc-mcp

DIST=$(mktemp -d)
trap 'rm -rf "$DIST"' EXIT
for bin in "${BINS[@]}"; do
  tar -C target/release -czf "${DIST}/${bin}-${TARGET_NAME}.tar.gz" "$bin"
  shasum -a 256 "${DIST}/${bin}-${TARGET_NAME}.tar.gz" | awk '{print $1}' \
    > "${DIST}/${bin}-${TARGET_NAME}.tar.gz.sha256"
done

# The release itself is created by the CI job; --clobber makes reruns safe.
gh release upload "$TAG" "$DIST"/* --repo "$REPO" --clobber
echo "uploaded ${TARGET_NAME} assets to ${TAG}:"
ls -la "$DIST"
