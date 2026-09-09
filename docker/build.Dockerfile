# Build image for patchcord release binaries.
#
# Fedora-based (not the Rust official image) specifically because Fedora
# ships current libpipewire-devel/pulseaudio-libs-devel packages *and* a
# current zig package, and because Docker's --platform emulation (QEMU)
# lets this same image build real, natively-linked aarch64 binaries on an
# x86_64 host -- cargo-zigbuild alone only cross-links glibc itself, not
# arbitrary system libraries like libpipewire/libpulse, so a genuine
# aarch64 root (real .so files, real headers, real pkg-config .pc files)
# is required regardless of the linker used. Running this image under
# --platform linux/arm64 makes every dnf install and every compile happen
# against the real target arch.
#
# cargo-zigbuild is still used (not proxied away) even though each build
# now runs natively per-arch: its actual job here is glibc *version*
# targeting (an older glibc floor than whatever this Fedora release ships),
# not cross-arch linking.
FROM fedora:latest

RUN dnf install -y --setopt=install_weak_deps=False \
        gcc \
        clang \
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
