#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
workflow="$repo_root/.github/workflows/platforms.yml"

python3 - "$workflow" <<'PY'
from pathlib import Path
import re
import sys

workflow = Path(sys.argv[1])
source = workflow.read_text(encoding="utf-8")

quality_jobs = list(re.finditer(r"(?m)^  quality:\s*$", source))
if len(quality_jobs) != 1:
    raise SystemExit(
        f"platform workflow must define exactly one quality job; found {len(quality_jobs)}"
    )

start = quality_jobs[0].start()
next_job = re.search(r"(?m)^  [A-Za-z0-9_-]+:\s*$", source[quality_jobs[0].end():])
end = quality_jobs[0].end() + next_job.start() if next_job else len(source)
quality = source[start:end]

if len(re.findall(r"(?m)^    runs-on: ubuntu-latest\s*$", quality)) != 1:
    raise SystemExit("quality job must run exactly once on ubuntu-latest")

required_commands = [
    "cargo fmt --all -- --check",
    "cargo clippy --workspace --all-targets -- -D warnings",
    "cargo test -p petalsonic --doc",
    "tools/tests/publish_realtime_gate_test.sh",
]
for command in required_commands:
    pattern = rf"(?m)^\s+run:\s*{re.escape(command)}\s*$"
    in_quality = len(re.findall(pattern, quality))
    in_workflow = len(re.findall(pattern, source))
    if in_quality != 1 or in_workflow != 1:
        raise SystemExit(
            f"quality command must appear exactly once and only in the quality job: {command!r} "
            f"(quality={in_quality}, workflow={in_workflow})"
        )

forbidden = {
    "release timing gate": r"tools/publish_realtime_gate\.sh|cargo\s+test\s+--release|--include-ignored",
    "publish workflow": r"tools/publish\.sh|cargo\s+publish|--publish",
}
for label, pattern in forbidden.items():
    if re.search(pattern, quality):
        raise SystemExit(f"quality job must not execute {label}")
PY
