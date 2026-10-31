#!/bin/sh
# Boot basicos in QEMU from this release folder.
#
#   ./run.sh              # the console is the QEMU window; this terminal mirrors it
#   ./run.sh -nographic   # headless: everything over this terminal
#
# Extra arguments are passed straight through to qemu-system-x86_64.
#
# Needs: qemu-system-x86_64. The 2 GiB test disk ships gzipped and is
# expanded (sparsely) into ./disk.img on first run; delete disk.img to get a
# pristine one back. The disk is the `C:` drive — writes to it persist
# across runs. (`A:` is a RAM disk and always starts empty.)

set -eu
cd "$(dirname "$0")"

if ! command -v qemu-system-x86_64 >/dev/null 2>&1; then
    echo "run.sh: qemu-system-x86_64 not found in PATH" >&2
    exit 1
fi

if [ ! -f disk.img ]; then
    echo "Expanding disk.img (2 GiB, sparse)..."
    gzip -dc disk.img.gz | dd of=disk.img bs=1M conv=sparse status=none
fi

# -boot d: boot the CD, not the (unbootable, data-only) hard disk.
# -serial stdio: the full kernel log, and a mirror of the console.
# -netdev user + rtl8139: NAT'd networking (guest 10.0.2.15, gateway
# 10.0.2.2), the NIC the kernel has a driver for.
exec qemu-system-x86_64 \
    -cdrom basicos.iso \
    -boot d \
    -m 256M \
    -serial stdio \
    -drive file=disk.img,format=raw,if=ide,index=0 \
    -netdev user,id=n0 -device rtl8139,netdev=n0 \
    "$@"
