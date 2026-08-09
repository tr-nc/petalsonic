#!/usr/bin/env bash
set -euo pipefail

mode="${1:---dry-run}"
if [[ "$mode" != "--dry-run" && "$mode" != "--publish" ]]; then
    echo "usage: tools/publish.sh [--dry-run|--publish]" >&2
    exit 2
fi

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' petalsonic/Cargo.toml | head -1)
echo "validating petalsonic v${version}"

echo "[1/7] formatting"
cargo fmt --all -- --check

echo "[2/7] all-target compilation"
cargo check --workspace --all-targets

echo "[3/7] clippy"
cargo clippy --workspace --all-targets -- -D warnings

echo "[4/7] workspace tests"
cargo test --workspace --all-targets

echo "[5/7] documentation tests"
cargo test -p petalsonic --doc

echo "[6/7] release demo build"
cargo build --release -p petalsonic-demo

echo "[7/7] package dry run"
cargo publish -p petalsonic --dry-run

if [[ "$mode" == "--publish" ]]; then
    echo "publishing petalsonic v${version}"
    cargo publish -p petalsonic
else
    echo "dry run complete; pass --publish to perform the registry write"
fi
