#!/usr/bin/env bash
set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/../.." && pwd -P)"

PYTHONDONTWRITEBYTECODE=1 python3 -m unittest \
  "$repo_dir/scripts/tests/test_portal_release.py"
printf 'portal_release_tests=ok\n'
