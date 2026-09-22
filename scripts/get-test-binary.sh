#!/usr/bin/env bash
# Fetch a real foreign-architecture binary for testing: Alpine's
# busybox-static (AArch64). Optional; scripts/test.sh picks it up if present.
set -euo pipefail
cd "$(dirname "$0")/.."

TMP=tests/tmp
mkdir -p "$TMP"

BASE="${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}/latest-stable/main/aarch64"

echo "== fetching package index: $BASE/"
index=$(curl -fsS "$BASE/")
file=$(printf '%s' "$index" | grep -o 'busybox-static-[0-9][^"<]*\.apk' | head -n1 || true)
[ -n "$file" ] || { echo 'no busybox-static found in index' >&2; exit 1; }

echo "== downloading $file"
curl -fsS -o "$TMP/busybox.apk" "$BASE/$file"

echo '== extracting bin/busybox.static'
tar -xzf "$TMP/busybox.apk" -C "$TMP" bin/busybox.static
mv -f "$TMP/bin/busybox.static" "$TMP/busybox-aarch64"
rmdir "$TMP/bin" 2>/dev/null || true
rm -f "$TMP/busybox.apk"

file "$TMP/busybox-aarch64" || true
echo "== saved to $TMP/busybox-aarch64"
