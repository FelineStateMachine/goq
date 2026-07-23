#!/usr/bin/env bash

set -euo pipefail

LC_ALL=C
export LC_ALL

test_dir="${1:-}"
if [[ -z "$test_dir" ]]; then
  printf 'usage: %s TEST_DIRECTORY\n' "${0##*/}" >&2
  exit 2
fi
if [[ -L "$test_dir" || ! -d "$test_dir" ]]; then
  printf 'shell test directory must be a real directory: %s\n' "$test_dir" >&2
  exit 1
fi
if [[ ! -r "$test_dir" || ! -x "$test_dir" ]]; then
  printf 'shell test directory must be readable and searchable: %s\n' "$test_dir" >&2
  exit 1
fi

# Bash expands this array in LC_ALL=C order. dotglob makes a test such as
# `.release-contract.sh` part of the same inventory, while nullglob lets us
# diagnose an empty inventory instead of trying to execute a literal glob.
shopt -s dotglob nullglob
shell_tests=("$test_dir"/*.sh)
shopt -u dotglob nullglob

if (( ${#shell_tests[@]} == 0 )); then
  printf 'no shell tests were discovered in: %s\n' "$test_dir" >&2
  exit 1
fi

# Validate the complete inventory before running its first entry. This keeps a
# malformed or non-executable test late in the sort order from producing a
# partially successful gate.
for shell_test in "${shell_tests[@]}"; do
  printf 'shell_test_discovered=%s\n' "$shell_test"
  if [[ "$shell_test" == *$'\n'* || "$shell_test" == *$'\r'* ]]; then
    printf 'shell test path contains an unsafe line break: %q\n' "$shell_test" >&2
    exit 1
  fi
  if [[ -L "$shell_test" || ! -f "$shell_test" ]]; then
    printf 'shell test entry must be a regular, non-symlink file: %s\n' \
      "$shell_test" >&2
    exit 1
  fi
  if [[ ! -r "$shell_test" ]]; then
    printf 'shell test entry is not readable: %s\n' "$shell_test" >&2
    exit 1
  fi
  if [[ ! -x "$shell_test" ]]; then
    printf 'shell test entry is not executable: %s\n' "$shell_test" >&2
    exit 1
  fi
done

for shell_test in "${shell_tests[@]}"; do
  printf 'shell_test_start=%s\n' "$shell_test"
  bash "$shell_test"
done
