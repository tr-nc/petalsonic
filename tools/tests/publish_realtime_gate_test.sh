#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
gate="$repo_root/tools/publish_realtime_gate.sh"
target="render::tests::warmed_near_capacity_quantum_meets_release_budget"
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT
fake_cargo="$fixture/cargo"
execution_log="$fixture/executions"

printf '%s\n' \
    '#!/usr/bin/env bash' \
    'set -euo pipefail' \
    'if [[ " $* " == *" --list "* ]]; then' \
    '    printf "%s\n" "${FAKE_TEST_LIST:-}"' \
    '    exit 0' \
    'fi' \
    'printf "%s\n" "$*" >> "$FAKE_EXECUTION_LOG"' \
    > "$fake_cargo"
chmod +x "$fake_cargo"

expect_closed() {
    local listed_tests=$1
    : > "$execution_log"
    if FAKE_TEST_LIST="$listed_tests" \
        FAKE_EXECUTION_LOG="$execution_log" \
        PETALSONIC_CARGO="$fake_cargo" \
        "$gate"; then
        echo "gate accepted a list without exactly one target" >&2
        exit 1
    fi
    if [[ -s "$execution_log" ]]; then
        echo "gate executed a test after rejecting its list" >&2
        exit 1
    fi
}

expect_closed "${target}_renamed: test"
expect_closed "unrelated::test: test"
expect_closed $'render::tests::warmed_near_capacity_quantum_meets_release_budget: test\nrender::tests::warmed_near_capacity_quantum_meets_release_budget: test'

: > "$execution_log"
FAKE_TEST_LIST=$'render::tests::warmed_near_capacity_quantum_meets_release_budget: test\nrender::tests::warmed_near_capacity_quantum_meets_release_budget_similar: test' \
    FAKE_EXECUTION_LOG="$execution_log" \
    PETALSONIC_CARGO="$fake_cargo" \
    "$gate"

expected="test --release -p petalsonic --lib $target -- --exact --nocapture"
if [[ $(<"$execution_log") != "$expected" ]]; then
    echo "gate did not execute the unique fully qualified target with --exact" >&2
    printf 'expected: %s\nactual:   %s\n' "$expected" "$(<"$execution_log")" >&2
    exit 1
fi
