#!/usr/bin/env bash
#
# Build the debian_sshd image and run it, producing usr.sbin.sshd.ghidra in the
# current working directory (shared into the container as /work).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The build context is the undwz repo root (two levels up): the image needs the
# undwz sources, patches, setup-vendor.sh and gather_for_ghidra.py.
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

IMAGE=undwz-debian-sshd

podman build -t "$IMAGE" -f "$SCRIPT_DIR/Dockerfile" "$REPO_ROOT"

# Share the current working directory as /work so the output lands here.
# :Z relabels the mount for SELinux; rootless podman maps the container's root
# back to the invoking user, so the output file is owned by you.
podman run --rm -v "$PWD:/work:Z" "$IMAGE"

echo "wrote $PWD/usr.sbin.sshd.ghidra"
