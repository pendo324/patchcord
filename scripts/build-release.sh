#!/usr/bin/env bash
# Builds release binaries for both x86_64 and aarch64 using the Fedora
# build container (docker/build.Dockerfile), producing exactly the assets
# and .sha256 sidecars patchcordAppAudio's native asset store expects:
#   patchcord-linux-<arch>
#   discord-capture-shim-linux-<arch>.so
#   discord-capture-setup-linux-<arch>
# and a <name>.sha256 file for each, into ./dist.
#
# Runs each arch's build natively under Docker --platform emulation
# (QEMU for the non-host arch) rather than cross-compiling from one
# container -- cargo-zigbuild only cross-links glibc itself, not
# libpipewire/libpulse, so a genuine per-arch root with real .so files
# is required regardless of linker. See build.Dockerfile's own comment.
#
# Usage: scripts/build-release.sh [--image-tag TAG] [x86_64] [aarch64]
# With no arch arguments, builds both.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

IMAGE="${PATCHCORD_BUILDER_IMAGE:-ghcr.io/pendo324/patchcord-builder:latest}"
GLIBC_VERSION="${PATCHCORD_GLIBC_VERSION:-2.17}"

ARCHES=()
while [ $# -gt 0 ]; do
    case "$1" in
        --image-tag) IMAGE="$2"; shift 2 ;;
        x86_64|aarch64) ARCHES+=("$1"); shift ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done
if [ ${#ARCHES[@]} -eq 0 ]; then
    ARCHES=(x86_64 aarch64)
fi

rm -rf dist
mkdir -p dist

declare -A RUST_TARGET=(
    [x86_64]="x86_64-unknown-linux-gnu"
    [aarch64]="aarch64-unknown-linux-gnu"
)
declare -A DOCKER_PLATFORM=(
    [x86_64]="linux/amd64"
    [aarch64]="linux/arm64"
)
declare -A ASSET_ARCH=(
    [x86_64]="x64"
    [aarch64]="arm64"
)

for arch in "${ARCHES[@]}"; do
    target="${RUST_TARGET[$arch]}"
    platform="${DOCKER_PLATFORM[$arch]}"
    asset_arch="${ASSET_ARCH[$arch]}"

    echo "==> Building for $arch ($target) via $platform"

    docker run --rm \
        -v "$(pwd)":/io \
        --platform "$platform" \
        "$IMAGE" \
        bash -c "RUSTFLAGS='-L/usr/lib64' cargo zigbuild --release -p patchcord -p discord-capture-shim --target ${target}.${GLIBC_VERSION}"

    out="target/${target}/release"

    cp "$out/patchcord" "dist/patchcord-linux-${asset_arch}"
    cp "$out/libdiscord_capture_shim.so" "dist/discord-capture-shim-linux-${asset_arch}.so"
    cp "$out/discord-capture-setup" "dist/discord-capture-setup-linux-${asset_arch}"
done

chmod +x dist/patchcord-linux-* dist/discord-capture-setup-linux-* dist/discord-capture-shim-linux-*.so

for f in dist/*; do
    [ -f "$f" ] || continue
    case "$f" in *.sha256) continue ;; esac
    sha256sum "$f" | awk '{print $1}' > "$f.sha256"
done

echo "==> dist/"
ls -la dist/
