#!/usr/bin/env bash
set -euo pipefail

mode="${1:---dry-run}"
if [[ "$mode" != "--dry-run" && "$mode" != "--publish" ]]; then
    echo "usage: tools/publish.sh [--dry-run|--publish]" >&2
    exit 2
fi

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' petalsonic/Cargo.toml | head -1)
echo "validating petalsonic v${version}"

echo "[1/8] formatting"
cargo fmt --all -- --check

echo "[2/8] all-target compilation"
cargo check --workspace --all-targets

echo "[3/8] clippy"
cargo clippy --workspace --all-targets -- -D warnings

echo "[4/8] workspace tests"
cargo test --workspace --all-targets

echo "[5/8] documentation tests"
cargo test -p petalsonic --doc

echo "[6/8] release realtime/performance contracts"
cargo test --release -p petalsonic \
    warmed_near_capacity_balanced_render_stays_bounded_and_meets_budget -- --nocapture

echo "[7/8] release demo build"
cargo build --release -p petalsonic-demo

echo "[8/8] package dry run"
cargo publish -p petalsonic --dry-run

if [[ "$mode" == "--publish" ]]; then
    echo "publishing petalsonic v${version}"
    cargo publish -p petalsonic
else
    echo "dry run complete; pass --publish to perform the registry write"
fi
