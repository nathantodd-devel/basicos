#!/usr/bin/env bash
# Build basicos inside Docker and drop a self-contained release folder on
# the host. No local Rust/GRUB/mtools install needed — just Docker.
#
#   ./docker-build.sh                         # -> ./release/
#   ./docker-build.sh -o /tmp/out             # -> /tmp/out/
#   ./docker-build.sh --features heap,keyboard
#   ./docker-build.sh --no-cache              # force a full rebuild
#   RUST_TOOLCHAIN=nightly ./docker-build.sh  # try the latest nightly
#
# The release folder contains: basicos.iso (bootable), kernel.bin,
# initrd.img, testprog.elf, disk.img.gz, run.sh, BUILD-INFO.txt, SHA256SUMS.

set -euo pipefail
cd "$(dirname "$0")"

OUT=release
FEATURES=""
EXTRA=()

while [ $# -gt 0 ]; do
    case "$1" in
        -o|--output)
            OUT="${2:?--output needs a directory}"; shift 2 ;;
        --features)
            # Convenience: a bare feature list means "only these" (all others
            # off). For anything fancier, edit the flags below or use the
            # Makefile's FEATURES variable directly.
            FEATURES="--no-default-features --features ${2:?--features needs a list}"; shift 2 ;;
        --no-cache)
            EXTRA+=(--no-cache); shift ;;
        -h|--help)
            sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)
            echo "docker-build.sh: unknown argument: $1 (try --help)" >&2; exit 2 ;;
    esac
done

# Prefer docker, but only if the daemon is actually reachable (installed
# but not in the `docker` group is a common state — `docker info` fails
# there). Fall back to podman, which runs rootless. Override with
# ENGINE=podman / ENGINE=docker.
if [ -z "${ENGINE:-}" ]; then
    if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
        ENGINE=docker
    elif command -v podman >/dev/null 2>&1; then
        ENGINE=podman
    else
        echo "docker-build.sh: no usable container engine." >&2
        echo "  docker: not installed, or your user can't reach the daemon" >&2
        echo "          (add yourself to the 'docker' group, or use sudo)" >&2
        echo "  podman: not installed" >&2
        exit 1
    fi
fi

# Stamp the build with the commit it came from (.git isn't in the build
# context, so the Dockerfile can't work this out itself).
GIT_REV=unknown
if git rev-parse --git-dir >/dev/null 2>&1; then
    GIT_REV="$(git rev-parse --short HEAD)"
    if ! git diff --quiet HEAD -- 2>/dev/null || [ -n "$(git ls-files --others --exclude-standard)" ]; then
        GIT_REV="${GIT_REV}-dirty"
    fi
fi

BUILD_ARGS=(--build-arg "FEATURES=${FEATURES}" --build-arg "GIT_REV=${GIT_REV}")
if [ -n "${RUST_TOOLCHAIN:-}" ]; then
    BUILD_ARGS+=(--build-arg "RUST_TOOLCHAIN=${RUST_TOOLCHAIN}")
fi

mkdir -p "$OUT"
echo ">> building with $ENGINE (rev ${GIT_REV}, features: ${FEATURES:-<defaults>})"

# `--target export --output` copies only the FROM-scratch stage's contents
# (the release folder) to the host — no image left behind to clean up.
if [ "$ENGINE" = docker ]; then
    DOCKER_BUILDKIT=1 docker build "${EXTRA[@]}" "${BUILD_ARGS[@]}" \
        --target export --output "type=local,dest=${OUT}" .
else
    # podman has no `--output`; build the stage and copy /release out.
    IMG="basicos-release-build:$$"
    podman build "${EXTRA[@]}" "${BUILD_ARGS[@]}" --target export -t "$IMG" .
    CID="$(podman create "$IMG" /)"
    podman cp "${CID}:/." "$OUT"
    podman rm "$CID" >/dev/null
    podman rmi "$IMG" >/dev/null
fi

echo
echo ">> release folder: $OUT/"
ls -lh "$OUT"
