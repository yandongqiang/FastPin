#!/usr/bin/env bash
# Regenerate the vendored gpui-pre deltas. Run from the repo root.
set -euo pipefail

regen() {
    local NAME=$1 PRISTINE=$2 VENDORED=$3
    local PRISTINE_DIR
    PRISTINE_DIR=$(echo "$HOME"/.cargo/registry/src/*/"$PRISTINE")
    if [ ! -d "$PRISTINE_DIR" ]; then
        echo "pristine $PRISTINE not in the cargo registry cache; run 'cargo fetch' first" >&2
        exit 1
    fi
    local WORK
    WORK=$(mktemp -d)
    mkdir "$WORK/a" "$WORK/b"
    cp -r "$PRISTINE_DIR/." "$WORK/a/"
    cp -r "$VENDORED/." "$WORK/b/"
    chmod -R u+w "$WORK"

    # diff exits 1 when there are differences, which is the normal case here.
    local OUT="patches/${NAME}-fastpin-overlay.patch"
    ( cd "$WORK" && diff -ruN --strip-trailing-cr \
        -x Cargo.lock -x .cargo-ok -x .cargo_vcs_info.json -x Cargo.toml.orig \
        -x examples -x UPSTREAM.md \
        a b ) > "$OUT" || true

    rm -rf "$WORK"
    echo "wrote $OUT ($(grep -c '^+' "$OUT") added, $(grep -c '^-' "$OUT") removed)"
}

regen gpui-pre-0.3.9 gpui-pre-0.3.9 third_party/gpui-pre
regen gpui-pre-windows-0.3.9 gpui-pre-windows-0.3.9 third_party/gpui-pre-windows
