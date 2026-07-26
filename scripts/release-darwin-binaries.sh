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
REPO="cortexkit/subconscious"

# The binary list is DERIVED from release.yml rather than transcribed here.
# Both halves of a release must ship the same set: CI produces linux-x64, this
# script produces darwin-arm64, and a consumer pinning the tag fetches whichever
# its platform needs. A hand-copied list in this file would agree with the
# workflow on the day it was written and drift the first time a binary is added
# to one and not the other -- and the drift is silent, because each half
# succeeds. The symptom lands on a consumer as a 404 for one platform only.
WORKFLOW=".github/workflows/release.yml"
[[ -f "$WORKFLOW" ]] || { echo "cannot derive binary list: ${WORKFLOW} not found" >&2; exit 1; }
# grep -o rather than sed: every --bin flag sits on ONE line in the workflow, and
# a line-oriented substitution yields only the last match on that line.
read -r -a BINS <<<"$(grep -oE -- '--bin [a-z0-9-]+' "$WORKFLOW" | awk '{print $2}' | sort -u | tr '\n' ' ')"
(( ${#BINS[@]} > 0 )) || { echo "derived an empty binary list from ${WORKFLOW}" >&2; exit 1; }
echo "binaries derived from ${WORKFLOW}: ${BINS[*]}"

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
  --bin ck-subc --bin subc-probe --bin ck --bin fake-aft-stub --bin ck-subc-mcp

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
