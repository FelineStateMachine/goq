#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(cd -- "$script_dir/../.." && pwd -P)"

cd "$repo_dir"
python3 scripts/verify_ci_cross_build_policy.py \
  .github/workflows/ci.yml \
  scripts/verify-demo-build.sh
python3 -m unittest scripts.tests.test_ci_cross_build_policy -v
