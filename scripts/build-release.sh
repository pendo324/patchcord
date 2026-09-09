#!/usr/bin/env bash
# Builds release binaries for one architecture, producing exactly the
# assets, .sha256 sidecars, and .build-id sidecars patchcordAppAudio's
# native asset store expects into ./dist:
#   patchcord-linux-<arch>
#   discord-capture-shim-linux-<arch>.so
#   discord-capture-setup-linux-<arch>
# plus a provenance-<arch>.txt recording exactly which toolchain
# versions (and, when the release workflow supplies them, which builder
# image/digest) produced these binaries, so a shipped binary's
# build-id/sha256 can later be traced back to the builder environment
# that produced it.
#
# Must be run inside the Fedora build container (docker/build.Dockerfile)
# -- either via the release workflow's `container:` job (which now runs
# natively on an arch-matched GitHub-hosted runner, amd64 or arm64, no
# QEMU involved) or locally via:
#   docker run --rm -v "$PWD":/io ghcr.io/<owner>/patchcord-builder:latest \
#       scripts/build-release.sh x86_64
#
# cargo-zigbuild's job here is glibc *version* targeting (an older glibc
# floor than whatever this Fedora release ships), not cross-arch linking
# -- this script only ever builds for the host's own architecture.
#
# Usage: scripts/build-release.sh <x86_64|aarch64>

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

GLIBC_VERSION="${PATCHCORD_GLIBC_VERSION:-2.17}"

arch="${1:?usage: scripts/build-release.sh <x86_64|aarch64>}"

declare -A RUST_TARGET=(
    [x86_64]="x86_64-unknown-linux-gnu"
    [aarch64]="aarch64-unknown-linux-gnu"
)
declare -A ASSET_ARCH=(
    [x86_64]="x64"
    [aarch64]="arm64"
)

target="${PATCHCORD_RUST_TARGET:-${RUST_TARGET[$arch]:?unknown arch: $arch}}"
asset_arch="${PATCHCORD_ASSET_ARCH:-${ASSET_ARCH[$arch]:?unknown arch: $arch}}"

echo "==> Building for $arch ($target)"

RUSTFLAGS='-L/usr/lib64 -Clink-arg=-Wl,--build-id=sha1' cargo zigbuild --release -p patchcord -p discord-capture-shim \
    --target "${target}.${GLIBC_VERSION}"

out="target/${target}/release"

rm -rf dist
mkdir -p dist

cp "$out/patchcord" "dist/patchcord-linux-${asset_arch}"
cp "$out/libdiscord_capture_shim.so" "dist/discord-capture-shim-linux-${asset_arch}.so"
cp "$out/discord-capture-setup" "dist/discord-capture-setup-linux-${asset_arch}"

chmod +x dist/patchcord-linux-* dist/discord-capture-setup-linux-* dist/discord-capture-shim-linux-*.so

for f in dist/*; do
    [ -f "$f" ] || continue
    case "$f" in *.sha256|*.build-id|dist/provenance-*.txt) continue ;; esac
    sha256sum "$f" | awk '{print $1}' > "$f.sha256"
    readelf -n "$f" 2>/dev/null | awk '/Build ID:/ {print $3}' > "$f.build-id"
done

{
    # `container:` jobs run git commands as a different uid than the one
    # that owns the checked-out tree, which git treats as "dubious
    # ownership" and refuses by default -- silence that here rather than
    # relying on the caller to have configured it already.
    git config --global --add safe.directory "$(pwd)" 2>/dev/null || true

    echo "arch: $arch ($target, glibc $GLIBC_VERSION)"
    echo "git commit: $(git rev-parse HEAD 2>/dev/null || echo unknown)"
    echo "builder image: ${PATCHCORD_BUILDER_IMAGE:-unknown}"
    echo "builder image digest: ${PATCHCORD_BUILDER_IMAGE_DIGEST:-unknown}"
    echo "rustc: $(rustc --version)"
    echo "cargo: $(cargo --version)"
    echo "cargo-zigbuild: $(cargo-zigbuild --version)"
    echo "zig: $(zig version)"
    echo
    for f in dist/*; do
        [ -f "$f" ] || continue
        case "$f" in *.sha256|*.build-id|dist/provenance-*.txt) continue ;; esac
        echo "$(basename "$f"):"
        echo "  sha256: $(cat "$f.sha256")"
        echo "  build-id: $(cat "$f.build-id" 2>/dev/null || echo "(none)")"
    done
} > "dist/provenance-${asset_arch}.txt"

echo "==> dist/"
ls -la dist/
