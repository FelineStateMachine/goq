#!/usr/bin/env bash

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/../.." && pwd -P)"
runner="$repo_dir/scripts/run-shell-tests.sh"
temp_parent="${TMPDIR:-/tmp}"
temp_parent="${temp_parent%/}"
temp_root="$(mktemp -d "$temp_parent/goq-shell-test-discovery.XXXXXX")"

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  case "$temp_root" in
    "$temp_parent"/goq-shell-test-discovery.??????) rm -rf -- "$temp_root" ;;
  esac
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

write_passing_test() {
  local path="$1"
  local marker="$2"
  printf '%s\n' \
    '#!/usr/bin/env bash' \
    'set -euo pipefail' \
    "printf '%s\\n' '$marker' >>\"\$GOQ_TEST_MARKERS\"" \
    >"$path"
  chmod 0755 "$path"
}

assert_failed_with() {
  local expected="$1"
  local output_path="$2"
  shift 2

  if "$@" >"$output_path" 2>&1; then
    printf 'command unexpectedly succeeded: %s\n' "$*" >&2
    exit 1
  fi
  if ! grep -Fq -- "$expected" "$output_path"; then
    printf 'failure did not contain expected text: %s\n' "$expected" >&2
    sed -n '1,120p' "$output_path" >&2
    exit 1
  fi
}

ordered_dir="$temp_root/ordered"
ordered_markers="$temp_root/ordered.markers"
mkdir "$ordered_dir"
write_passing_test "$ordered_dir/zeta.sh" zeta
write_passing_test "$ordered_dir/.hidden.sh" hidden
write_passing_test "$ordered_dir/alpha.sh" alpha
GOQ_TEST_MARKERS="$ordered_markers" "$runner" "$ordered_dir" \
  >"$temp_root/ordered.out"
printf '%s\n' hidden alpha zeta >"$temp_root/ordered.expected"
cmp "$temp_root/ordered.expected" "$ordered_markers"
for expected_path in \
  "$ordered_dir/.hidden.sh" \
  "$ordered_dir/alpha.sh" \
  "$ordered_dir/zeta.sh"
do
  grep -Fq "shell_test_discovered=$expected_path" "$temp_root/ordered.out"
  grep -Fq "shell_test_start=$expected_path" "$temp_root/ordered.out"
done

nonexec_dir="$temp_root/nonexec"
nonexec_markers="$temp_root/nonexec.markers"
mkdir "$nonexec_dir"
write_passing_test "$nonexec_dir/alpha.sh" should-not-run
printf '%s\n' '#!/usr/bin/env bash' 'exit 0' >"$nonexec_dir/zeta.sh"
chmod 0644 "$nonexec_dir/zeta.sh"
assert_failed_with \
  "shell test entry is not executable: $nonexec_dir/zeta.sh" \
  "$temp_root/nonexec.out" \
  env GOQ_TEST_MARKERS="$nonexec_markers" "$runner" "$nonexec_dir"
grep -Fq "shell_test_discovered=$nonexec_dir/zeta.sh" "$temp_root/nonexec.out"
[[ ! -e "$nonexec_markers" ]]

symlink_dir="$temp_root/symlink"
symlink_markers="$temp_root/symlink.markers"
mkdir "$symlink_dir"
write_passing_test "$symlink_dir/alpha.sh" should-not-run
ln -s alpha.sh "$symlink_dir/zeta.sh"
assert_failed_with \
  "shell test entry must be a regular, non-symlink file: $symlink_dir/zeta.sh" \
  "$temp_root/symlink.out" \
  env GOQ_TEST_MARKERS="$symlink_markers" "$runner" "$symlink_dir"
[[ ! -e "$symlink_markers" ]]

empty_dir="$temp_root/empty"
mkdir "$empty_dir"
assert_failed_with \
  "no shell tests were discovered in: $empty_dir" \
  "$temp_root/empty.out" \
  "$runner" "$empty_dir"

printf 'shell_test_discovery_tests=ok\n'
