#!/usr/bin/env bash
set -euo pipefail

target="render::tests::warmed_near_capacity_quantum_meets_release_budget"
cargo_bin="${PETALSONIC_CARGO:-cargo}"
listed_tests=$(
    "$cargo_bin" test --release -p petalsonic --lib -- --list
)
match_count=$(
    printf '%s\n' "$listed_tests" \
        | awk -v expected="$target: test" '$0 == expected { count += 1 } END { print count + 0 }'
)

if [[ "$match_count" -ne 1 ]]; then
    echo "publish gate requires exactly one test named '$target'; found $match_count" >&2
    exit 1
fi

"$cargo_bin" test --release -p petalsonic --lib "$target" -- --exact --nocapture
