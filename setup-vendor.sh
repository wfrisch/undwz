#!/usr/bin/env bash
#
# Materialize vendor/gimli = pristine gimli <VERSION> + patches/gimli-<VERSION>-undwz.patch
#
# undwz depends on a slightly modified gimli (see README and the patch header).
# Instead of committing a full copy of gimli, the repo ships only the small patch;
# this script rebuilds the vendored crate that Cargo's [patch.crates-io] in
# Cargo.toml points at (vendor/gimli).
#
# Run once after cloning (and whenever the patch changes), then `cargo build`.
#
# No Rust/tooling dependencies beyond tar, patch, and curl-or-wget.
set -euo pipefail

VERSION="0.34.0"
CRATE="gimli-${VERSION}.crate"
PATCH_FILE="patches/gimli-${VERSION}-undwz.patch"

cd "$(dirname "$0")"

[[ -f "$PATCH_FILE" ]] || { echo "error: $PATCH_FILE not found" >&2; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# 1. Obtain the pristine .crate: prefer cargo's local cache, else download.
cached="$(find "${CARGO_HOME:-$HOME/.cargo}/registry/cache" -name "$CRATE" 2>/dev/null | head -1 || true)"
if [[ -n "$cached" ]]; then
    echo "using cached crate: $cached"
    cp "$cached" "$work/$CRATE"
else
    url="https://static.crates.io/crates/gimli/${CRATE}"
    echo "downloading $url"
    if command -v curl >/dev/null; then curl -fsSL "$url" -o "$work/$CRATE"
    else wget -qO "$work/$CRATE" "$url"; fi
fi

# 2. Extract to vendor/gimli (the tarball unpacks to gimli-<VERSION>/).
tar xzf "$work/$CRATE" -C "$work"
rm -rf vendor/gimli
mkdir -p vendor
mv "$work/gimli-${VERSION}" vendor/gimli

# 3. Apply the undwz patch.
patch -p1 -d vendor/gimli < "$PATCH_FILE"

echo "vendor/gimli ready (gimli ${VERSION} + undwz patch). Now run: cargo build --release"
