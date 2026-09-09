# Build image for patchcord release binaries.
#
# Fedora-based (not the Rust official image) specifically because Fedora
# ships current libpipewire-devel/pulseaudio-libs-devel packages *and* a
# current zig package. Built and consumed per-architecture natively (amd64
# and arm64 GitHub-hosted runners, no QEMU) -- linking against real .so
# files, real headers, and real pkg-config .pc files rather than
# cross-linking, since cargo-zigbuild alone only cross-links glibc itself,
# not arbitrary system libraries like libpipewire/libpulse.
#
# cargo-zigbuild's actual job here is glibc *version* targeting (an older
# glibc floor than whatever this Fedora release ships), not cross-arch
# linking.
FROM fedora:latest

RUN dnf install -y --setopt=install_weak_deps=False \
        gcc \
        clang \
        git \
        pkgconf-pkg-config \
        pipewire-devel \
        pulseaudio-libs-devel \
        rustup \
        zig \
    && dnf clean all

ENV RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:$PATH

# This image also serves as the CI container (see ci.yml) -- so it needs
# clippy/rustfmt in addition to the plain compiler used for release
# builds, hence the "minimal" rustup profile plus explicit components
# rather than "default".
RUN rustup-init -y --profile minimal --default-toolchain stable --component clippy,rustfmt
RUN cargo install --locked cargo-zigbuild

WORKDIR /io
