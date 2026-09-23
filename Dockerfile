# syntax=docker/dockerfile:1
#
# Builds basicos from a clean checkout and assembles a self-contained
# release folder. Don't invoke this directly — use ./docker-build.sh,
# which runs it with the right `--target`/`--output` flags to land the
# result in ./release/ on the host.
#
#   ./docker-build.sh
#   ./docker-build.sh --features "heap,keyboard"      # reduced kernel
#   RUST_TOOLCHAIN=nightly-2026-09-07 ./docker-build.sh
#
# Stages:
#   builder  toolchain + build deps, runs the real `make` targets
#   export   FROM scratch — just /release, so `--output` copies only that

# trixie, not bookworm: bookworm's mtools (4.0.32) infers the FAT type from
# the cluster count and rejects our deliberately tiny "FAT32" initrd image
# ("Error reading FAT"); 4.0.48+ trusts the header like the Fedora build
# this was developed against.
ARG DEBIAN_TAG=trixie-slim

FROM debian:${DEBIAN_TAG} AS builder

# The kernel uses unstable features (naked functions, -Zjson-target-spec,
# build-std), so a floating `nightly` can break it without warning. This
# default is the nightly the project was last verified against; override
# with --build-arg RUST_TOOLCHAIN=nightly to try the latest.
ARG RUST_TOOLCHAIN=nightly-2026-09-07
# Extra cargo flags, e.g. "--no-default-features --features heap,keyboard".
# Passed straight through to the Makefile's FEATURES variable.
ARG FEATURES=""
ARG GIT_REV=unknown

ENV DEBIAN_FRONTEND=noninteractive

# build-essential: as/ld/strip/make. grub-pc-bin + xorriso + mtools:
# grub-mkrescue's BIOS-bootable ISO output. dosfstools + mtools + qemu-utils:
# the FAT32 disk/initrd images (mkfs.vfat, mcopy/mmd, qemu-img).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl \
        grub-common grub-pc-bin xorriso mtools dosfstools qemu-utils \
 && rm -rf /var/lib/apt/lists/*

# The Makefile calls Fedora's name for this tool; Debian ships it as
# grub-mkrescue.
RUN ln -s "$(command -v grub-mkrescue)" /usr/local/bin/grub2-mkrescue

# rustup, with the toolchain + rust-src (build-std compiles core/alloc
# from source). RUSTUP_TOOLCHAIN overrides rust-toolchain.toml's floating
# `channel = "nightly"` so the pinned version above is what actually runs.
ENV RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:${PATH}
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain none \
 && rustup toolchain install "${RUST_TOOLCHAIN}" --profile minimal --component rust-src
ENV RUSTUP_TOOLCHAIN=${RUST_TOOLCHAIN}

WORKDIR /src
COPY . .

# The actual build — same Makefile targets a developer would run by hand:
#   release  -> build/basicos-release.iso (+ stripped kernel, initrd)
#   disk     -> build/disk.img (2G FAT32, plus testprog.elf as /hello.elf)
RUN make release FEATURES="${FEATURES}" \
 && make disk

# Assemble the release folder. The 2 GiB disk image is almost entirely
# zeros, so it ships gzipped (a few KiB) and run.sh re-expands it sparsely
# — otherwise the export would materialise all 2 GiB on the host.
RUN mkdir -p /release \
 && cp build/basicos-release.iso        /release/basicos.iso \
 && cp build/kernel-release.bin         /release/kernel.bin \
 && cp build/initrd.img                 /release/initrd.img \
 && cp build/testprog.elf               /release/testprog.elf \
 && gzip -9 -c build/disk.img         > /release/disk.img.gz \
 && cp docker/run.sh                    /release/run.sh \
 && chmod +x /release/run.sh \
 && { \
      echo "basicos release build"; \
      echo "built:      $(date -u +%Y-%m-%dT%H:%M:%SZ)"; \
      echo "git rev:    ${GIT_REV}"; \
      echo "features:   ${FEATURES:-<defaults: all>}"; \
      echo "toolchain:  $(rustc --version)"; \
      echo "grub:       $(grub-mkrescue --version)"; \
    } > /release/BUILD-INFO.txt \
 && cd /release && sha256sum basicos.iso kernel.bin initrd.img testprog.elf disk.img.gz run.sh > SHA256SUMS

FROM scratch AS export
COPY --from=builder /release/ /
