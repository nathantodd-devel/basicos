#!/usr/bin/env bash
# GPG-sign a release folder: a detached signature over SHA256SUMS, which
# lists every other file by hash, plus a copy of the public key.
#
#   tools/sign-release.sh release/          # sign (or re-sign) a folder
#   tools/sign-release.sh --check           # just check the key is here
#
# docker-build.sh runs this on the host after the build, so the private key
# never goes near the container. Set SIGNING_KEY to use a different key.

set -euo pipefail

KEY="${SIGNING_KEY:-E54C104123A22A44D26956F74B86F8E2888892DC}"

# gpg-agent asks for the passphrase on this terminal.
GPG_TTY="${GPG_TTY:-$(tty 2>/dev/null || true)}"
export GPG_TTY

if ! gpg --list-secret-keys "$KEY" >/dev/null 2>&1; then
    echo "sign-release.sh: no secret key for $KEY in this keyring" >&2
    echo "  use your own with SIGNING_KEY=<id>, or build with --no-sign" >&2
    exit 1
fi
if [ "${1:-}" = --check ]; then
    exit 0
fi

DIR="${1:?usage: sign-release.sh <release-dir> | --check}"
cd "$DIR"

# Never vouch for files that don't match their hashes.
sha256sum --check --quiet SHA256SUMS

rm -f SHA256SUMS.asc basicos-signing-key.asc
gpg --armor --detach-sign --local-user "$KEY" --output SHA256SUMS.asc SHA256SUMS
gpg --armor --export "$KEY" > basicos-signing-key.asc

if ! gpg --status-fd 1 --verify SHA256SUMS.asc SHA256SUMS 2>/dev/null | grep -q VALIDSIG; then
    echo "sign-release.sh: the new signature doesn't verify" >&2
    rm -f SHA256SUMS.asc
    exit 1
fi
echo ">> signed SHA256SUMS with $KEY"
