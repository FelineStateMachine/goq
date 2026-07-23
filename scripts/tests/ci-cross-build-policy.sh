#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(cd -- "$script_dir/../.." && pwd -P)"

cd "$repo_dir"
python3 scripts/verify_ci_cross_build_policy.py .github/workflows/ci.yml
python3 -m unittest scripts.tests.test_ci_cross_build_policy -v

gate_call_count="$(
  grep -Fxc './scripts/run-linux-cross-build-gate.sh' \
    scripts/verify-demo-build.sh || true
)"
[[ "$gate_call_count" == 1 ]] || {
  echo 'complete repository gate must invoke the Linux cross-build gate exactly once' >&2
  exit 1
}
