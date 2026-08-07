#!/usr/bin/env bash
set -euo pipefail

# release.sh — tag and push a per-crate subconscious release.
#
# Usage:
#   ./scripts/release.sh <crate> <version> [--dry]
#   e.g. ./scripts/release.sh subc-transport 0.1.0
#
# Publishable crates: subc-protocol, subc-transport (subc-core is publish=false).
# On the pushed tag, CI (release.yml) takes over: verify (fmt/clippy/test on
# ubuntu + windows) → cargo publish to crates.io.
#
# NOT three platforms, despite what this said until 2026-07-26: ci.yml's matrix
# is ubuntu and windows only. macOS is absent because Blacksmith has no macOS
# runners and GitHub-hosted macOS is billing-blocked for this private free-plan
# org -- the same constraint that leaves .github/workflows/subc-fed.yml queued
# forever. So a release is verified on two platforms and SHIPS a darwin-arm64
# asset built by scripts/release-darwin-binaries.sh on the dev box, which is the
# one platform combination CI cannot check.

CRATE="${1:-}"
VERSION="${2:-}"
DRY="${3:-}"

if [[ -z "$CRATE" || -z "$VERSION" ]]; then
  echo "Usage: ./scripts/release.sh <crate> <version> [--dry]"
  echo "  e.g. ./scripts/release.sh subc-transport 0.1.0"
  exit 1
fi

MANIFEST="crates/$CRATE/Cargo.toml"
if [[ ! -f "$MANIFEST" ]]; then
  echo "Error: no crate '$CRATE' at $MANIFEST"
  exit 1
fi

if [[ "$CRATE" == "subc-core" ]]; then
  echo "Error: subc-core is publish=false and is never released to crates.io"
  exit 1
fi

if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.]+)?$ ]]; then
  echo "Error: '$VERSION' is not valid semver (expected X.Y.Z)"
  exit 1
fi

TAG="$CRATE-v$VERSION"
CURRENT_HEAD=$(git rev-parse HEAD)

# Verify the OUTCOME of a push, not the command. `set -e` catches a push that
# exits non-zero, which covers the common failure -- but it checks what the
# command CLAIMED, and the two come apart: a pipeline reports its last stage's
# status, so a push behind `| tail` exits 0 while failing, and any future
# wrapper here inherits that. These predicates are what would be TRUE if the
# push landed, so they survive a false-green wrapper.
#
# Called from BOTH push paths deliberately. The resume path below is the one
# taken after an earlier push failed, so a check present only on the first-run
# path would be absent exactly where it is most needed -- the guard belongs on
# the recovery path at least as much as on the happy one.
verify_push_landed() {
  local branch
  branch=$(git rev-parse --abbrev-ref HEAD)
  if [[ -n "$(git rev-list "origin/$branch"..HEAD 2>/dev/null)" ]]; then
    echo "Error: push reported success but origin/$branch is still behind HEAD"
    exit 1
  fi
  if ! git ls-remote --exit-code --tags origin "refs/tags/$TAG" >/dev/null 2>&1; then
    echo "Error: push reported success but tag '$TAG' is not on origin"
    exit 1
  fi
}

# Resumable release: if the tag already exists at HEAD, just (re)push it.
if git show-ref --verify --quiet "refs/tags/$TAG"; then
  tag_commit=$(git rev-list -n 1 "$TAG")
  if [[ "$tag_commit" == "$CURRENT_HEAD" ]]; then
    echo "→ Tag '$TAG' already at HEAD; resuming push."
    [[ "$DRY" == "--dry" ]] && { echo "[DRY] would push $TAG"; exit 0; }
      git push origin "$TAG"
      verify_push_landed
      exit 0
  fi
  echo "Error: tag '$TAG' exists but points at $tag_commit, not HEAD ($CURRENT_HEAD)"
  echo "       Refusing to reuse a release tag from a different commit."
  exit 1
fi

# Clean tree required (so the tagged commit is exactly what's reviewed).
if [[ -n "$(git status --porcelain)" ]]; then
  echo "Error: working tree not clean — commit or stash first"
  git status --short
  exit 1
fi

# Sync the crate's Cargo.toml version to $VERSION if it differs.
CARGO_VERSION=$(grep '^version' "$MANIFEST" | head -1 | sed -E 's/version *= *"([^"]+)"/\1/')
NEEDS_BUMP=0
if [[ "$CARGO_VERSION" != "$VERSION" ]]; then
  NEEDS_BUMP=1
  echo "→ $MANIFEST: $CARGO_VERSION → $VERSION"
fi

# Pre-tag checks (the same gates CI will run, fail-fast locally first).
echo "→ Pre-release checks (fmt, clippy, publish dry-run)..."
if [[ "$NEEDS_BUMP" == "1" && "$DRY" != "--dry" ]]; then
  sed -i.bak -E "0,/^version *= *\"[^\"]+\"/s//version = \"$VERSION\"/" "$MANIFEST"
  rm -f "$MANIFEST.bak"
fi
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo publish --package "$CRATE" --dry-run

if [[ "$DRY" == "--dry" ]]; then
  echo "[DRY] checks passed; would commit (if bumped) + tag $TAG + push."
  exit 0
fi

if [[ "$NEEDS_BUMP" == "1" ]]; then
  git add "$MANIFEST"
  git commit -m "release: $TAG"
fi

git tag -a "$TAG" -m "Release $TAG"
git push origin HEAD
git push origin "$TAG"

verify_push_landed

echo "  ✓ Pushed $TAG — CI will verify (ubuntu + windows) then publish $CRATE v$VERSION to crates.io"
