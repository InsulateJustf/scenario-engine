#!/bin/sh
# build-package.sh — build the static musl binary and pack it as an apk (apk-tools v3).
# Output: dist/scenario-engine-<version>.apk
#
# Lifecycle scripts are embedded and the ADB manifest is checked before publishing.
set -eu

ROOT=$(cd "$(dirname "$0")" && pwd)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)
OUT="$ROOT/dist/scenario-engine-$VERSION.apk"
STAGE_SOURCE="$ROOT/packaging/rootfs"

echo "== building musl binary =="
cargo zigbuild --manifest-path "$ROOT/Cargo.toml" --locked --release --target aarch64-unknown-linux-musl -p scenario-engine
mkdir -p "$ROOT/dist"
WORK=$(mktemp -d "$ROOT/dist/.scenario-engine.XXXXXX")
trap 'rm -rf "$WORK"' EXIT HUP INT TERM
TMP="$WORK/package.apk"
STAGE="$WORK/root"
mkdir -p "$STAGE"
cp -R "$STAGE_SOURCE/." "$STAGE/"
mkdir -p "$STAGE/usr/bin"
cp "$ROOT/target/aarch64-unknown-linux-musl/release/scenario-engine" "$STAGE/usr/bin/scenario-engine"
cp "$ROOT/LICENSE" "$STAGE/usr/share/scenario/LICENSE"
chmod 755 "$STAGE/usr/bin/scenario-engine"
chmod 755 "$STAGE/etc/init.d/scenario" "$STAGE/usr/share/scenario/wifiguard.sh" "$STAGE/usr/share/scenario/radio-recover.sh"

APK=$("$ROOT/tools/build-apk-tools.sh")

echo "== packing $OUT =="
"$APK" mkpkg \
	-F "$STAGE" \
	--no-xattrs \
	--root-owner \
	-I "name:scenario-engine" \
	-I "version:$VERSION" \
	-I "arch:aarch64_generic" \
	-I "description:luya-z scenario engine port (scan SSID, switch the AP off/on) for OpenWrt" \
	-I "license:PolyForm-Noncommercial-1.0.0" \
	-s "post-install:$ROOT/packaging/post-install" \
	-s "post-upgrade:$ROOT/packaging/post-install" \
	-s "pre-deinstall:$ROOT/packaging/pre-deinstall" \
	-o "$TMP"

echo "== sanity: the package must carry files =="
SIZE=$(wc -c < "$TMP")
[ "$SIZE" -gt 100000 ] || { echo "PACKAGE TOO SMALL ($SIZE bytes) — refusing"; exit 1; }
echo "ok, $SIZE bytes"
MANIFEST=$("$APK" adbdump "$TMP")
for expected in 'name: scenario-engine' 'name: scenario' 'name: wifiguard.sh' 'name: radio-recover.sh' 'name: scenarios.json.default' 'name: NOTICE' 'name: LICENSE' 'post-install:' 'post-upgrade:' 'pre-deinstall:'; do
	printf '%s\n' "$MANIFEST" | grep -Fq "$expected" || { echo "PACKAGE MISSING $expected — refusing"; exit 1; }
done
if printf '%s\n' "$MANIFEST" | grep -E '^[[:space:]]+(user|group):' | grep -vE ': root$'; then
	echo "PACKAGE HAS NON-ROOT OWNERS — refusing"; exit 1
fi
mv "$TMP" "$OUT"
ls -la "$OUT"
