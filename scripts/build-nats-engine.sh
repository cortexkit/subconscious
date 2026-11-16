#!/usr/bin/env bash
# Build only from checksum-verified modules; never modify the Go module cache.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
MODULE="$ROOT/engine/nats"
CALLER=$PWD
export GOTOOLCHAIN=go1.26.4 GOWORK=off CGO_ENABLED=0
cd "$MODULE"
go mod download
go mod verify
mkdir -p "$ROOT/target"
BUILD=$(mktemp -d "$ROOT/target/nats-engine.XXXXXX")
finished=
cleanup() {
    local rc=$?
    rm -rf "$BUILD"
    if [ "$rc" -eq 0 ] && [ "${finished:-}" != 1 ]; then
        echo "Engine build exited before completion" >&2
        rc=1
    fi
    exit "$rc"
}
trap cleanup EXIT
UPSTREAM=$(go list -m -f '{{.Dir}}' github.com/nats-io/nats-server/v2)
mkdir -p "$BUILD/module"
cp "$MODULE/"*.go "$MODULE/go.mod" "$MODULE/go.sum" "$BUILD/module/"
cp -R "$UPSTREAM" "$BUILD/module/upstream"
chmod -R u+w "$BUILD/module/upstream"
patch --batch --fuzz=0 -d "$BUILD/module/upstream" -p1 < "$MODULE/reload-signaturecb.patch"
cd "$BUILD/module"
# A relative replacement keeps random build paths out of embedded Go build metadata.
go mod edit -replace 'github.com/nats-io/nats-server/v2=./upstream'
if [ "${1:-}" = --test ]; then
    shift
    go test -mod=readonly "$@" ./...
else
    GOOS=${GOOS:-$(go env GOHOSTOS)}
    GOARCH=${GOARCH:-$(go env GOHOSTARCH)}
    EXT=
    if [ "$GOOS" = windows ]; then EXT=.exe; fi
    OUTPUT=${1:-$ROOT/target/nats-engine/$GOOS-$GOARCH/ck-nats-engine$EXT}
    case "$OUTPUT" in /*) ;; *) OUTPUT="$CALLER/$OUTPUT" ;; esac
    mkdir -p "$(dirname "$OUTPUT")"
    GOOS=$GOOS GOARCH=$GOARCH go build -mod=readonly \
        -trimpath -buildvcs=false -ldflags '-s -w' -o "$OUTPUT" .
    printf 'Built %s (%s bytes)\n' "$OUTPUT" "$(wc -c < "$OUTPUT" | tr -d ' ')"
fi
finished=1
