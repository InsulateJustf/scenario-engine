#!/bin/sh
# Build the pinned, locally patched apk-tools; print only the apk path to stdout.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
VERSION=3.0.5
ARCHIVE_SHA256=8795712ce02457d29c0beb18f82851b408d1d47f98a0cbbbd33ab5ea496f665d
URL="https://codeload.github.com/alpinelinux/apk-tools/tar.gz/refs/tags/v$VERSION"
PATCH="$ROOT/tools/apk-tools-root-owner.patch"
PATCH_SHA256=$(shasum -a 256 "$PATCH" | cut -d ' ' -f1)
CACHE_ROOT="$ROOT/target/host-tools"
SOURCE="$CACHE_ROOT/apk-tools-$VERSION-$PATCH_SHA256"
BUILD="$SOURCE/build"

if [ ! -d "$SOURCE" ]; then
	mkdir -p "$CACHE_ROOT"
	WORK=$(mktemp -d "$CACHE_ROOT/.apk-tools.XXXXXX")
	trap 'rm -rf "$WORK"' EXIT HUP INT TERM
	echo "== downloading apk-tools $VERSION ==" >&2
	curl --fail --location --silent --show-error --output "$WORK/source.tar.gz" "$URL"
	if ! printf '%s  %s\n' "$ARCHIVE_SHA256" "$WORK/source.tar.gz" | shasum -a 256 --check --status; then
		echo "apk-tools: source archive SHA-256 mismatch" >&2
		exit 1
	fi
	tar -xzf "$WORK/source.tar.gz" -C "$WORK"
	patch --batch -d "$WORK/apk-tools-$VERSION" -p1 < "$PATCH" >&2
	mv "$WORK/apk-tools-$VERSION" "$SOURCE"
fi

if [ ! -f "$BUILD/build.ninja" ]; then
	echo "== configuring apk-tools $VERSION ==" >&2
	meson setup "$BUILD" "$SOURCE" --prefix="$SOURCE/install" -Dcrypto_backend=openssl \
		-Dlua=disabled -Dhelp=disabled -Ddocs=disabled -Dtests=disabled -Dpython=disabled >&2
fi
ninja -C "$BUILD" >&2
printf '%s\n' "$BUILD/src/apk"
