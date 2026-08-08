#!/usr/bin/env bash
# Fetch the Android system binaries from their upstream source and verify them.
#
# The committed blobs that used to live under vendor/android-system/<arch> were
# removed to keep the repo light; they are downloaded here at base-image build
# time (Dockerfile.base runs this before tools/stage-system.sh).
#
# Source: zhaarey/wrapper rootfs/system, pinned to a fixed commit SHA. Every
# file is SHA-256 verified against LIBS_VERSION.json (.android_system.<arch>)
# before it is written, so a tampered or changed upstream file fails the build.

set -euo pipefail

SCRIPT_DIR="${BASH_SOURCE[0]%/*}"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LIBS_VERSION="$REPO_ROOT/LIBS_VERSION.json"

ARCH="x86_64"
PIN="57355efab9fea6872494c1dbc356e59a7293ae4c"
VENDOR=""
IGNORE_HASH=0

usage() {
    sed -n '2,10p' "$0"
    echo
    echo "usage: $0 [--arch x86_64] [--pin <commit-sha>] [--vendor <dir>] [--ignore-hash]"
    exit 0
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch)        ARCH="$2";   shift 2 ;;
        --pin)         PIN="$2";    shift 2 ;;
        --vendor)      VENDOR="$2"; shift 2 ;;
        --ignore-hash) IGNORE_HASH=1; shift ;;
        -h|--help)     usage ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [[ "$ARCH" != "x86_64" ]]; then
    echo "fetch-android-system: only x86_64 is supported (got '$ARCH')" >&2
    exit 2
fi

if [[ -z "$VENDOR" ]]; then
    VENDOR="$REPO_ROOT/vendor/android-system/$ARCH"
fi

for c in curl jq sha256sum; do
    command -v "$c" >/dev/null || { echo "fetch-android-system: $c is required" >&2; exit 3; }
done

# All files pinned for this arch, relative to rootfs/system/ in upstream.
mapfile -t RELS < <(jq -r --arg a "$ARCH" '.android_system[$a] | keys[]' "$LIBS_VERSION")

BASE="https://raw.githubusercontent.com/zhaarey/wrapper/$PIN/rootfs/system"

ok=0
for rel in "${RELS[@]}"; do
    want="$(jq -r --arg a "$ARCH" --arg r "$rel" '.android_system[$a][$r]' "$LIBS_VERSION")"
    dest="$VENDOR/$rel"
    if [[ -f "$dest" ]]; then
        got="$(sha256sum "$dest" | awk '{print $1}')"
        if [[ "$got" == "$want" ]]; then
            echo "fetch-android-system: ok (present) $rel"
            ok=$((ok + 1))
            continue
        fi
        # Stale copy: re-fetch below.
        rm -f "$dest"
    fi
    mkdir -p "$(dirname "$dest")"
    curl -fsSL --retry 3 -o "$dest" "$BASE/$rel" \
        || { echo "fetch-android-system: download failed for $rel (404 upstream?)" >&2; exit 1; }
    if [[ "$IGNORE_HASH" != "1" ]]; then
        got="$(sha256sum "$dest" | awk '{print $1}')"
        if [[ "$got" != "$want" ]]; then
            echo "fetch-android-system: hash mismatch for $rel" >&2
            echo "  expected $want" >&2
            echo "  got      $got" >&2
            rm -f "$dest"
            exit 1
        fi
    fi
    echo "fetch-android-system: ok $rel"
    ok=$((ok + 1))
done

echo "fetch-android-system: $ok files ok (arch=$ARCH pin=$PIN vendor=$VENDOR hash=checked)"
