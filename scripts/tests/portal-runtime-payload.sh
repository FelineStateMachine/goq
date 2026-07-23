#!/usr/bin/env bash

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/../.." && pwd -P)"
source_dir="$repo_dir/portal"
dist_dir="$repo_dir/portal-dist"
manifest="$source_dir/runtime-files.txt"
fixture_name="unlisted-runtime-payload-fixture.mjs"
fixture_source="$source_dir/$fixture_name"
fixture_dist="$dist_dir/$fixture_name"
fixture_dir="$dist_dir/unlisted-runtime-payload-fixture"
temp_parent="${TMPDIR:-/tmp}"
temp_parent="${temp_parent%/}"
temp_root="$(mktemp -d "$temp_parent/goq-portal-runtime-payload.XXXXXX")"

cleanup() {
  local exit_status=$?
  trap - EXIT INT TERM HUP
  rm -f -- "$fixture_source" "$fixture_dist"
  case "$fixture_dir" in
    "$dist_dir"/unlisted-runtime-payload-fixture)
      rm -rf -- "$fixture_dir"
      ;;
  esac
  case "$temp_root" in
    "$temp_parent"/goq-portal-runtime-payload.??????)
      rm -rf -- "$temp_root"
      ;;
  esac
  exit "$exit_status"
}
trap cleanup EXIT INT TERM HUP

if [[ -L "$manifest" || ! -f "$manifest" ]]; then
  printf 'portal runtime payload manifest must be a regular file: %s\n' "$manifest" >&2
  exit 1
fi

printf 'export const shouldNotShip = true;\n' >"$fixture_source"
mkdir -p "$fixture_dir"
printf 'stale\n' >"$fixture_dist"
printf 'stale\n' >"$fixture_dir/stale.test.mjs"

if [[ -f "${HOME}/.cargo/env" ]]; then
  # shellcheck source=/dev/null
  source "${HOME}/.cargo/env"
fi
cargo check --locked -p portal --quiet

if [[ -e "$fixture_dist" || -e "$fixture_dir" ]]; then
  printf 'portal build retained an unallowlisted generated payload entry\n' >&2
  exit 1
fi

sed -e '/^[[:space:]]*#/d' -e '/^[[:space:]]*$/d' "$manifest" \
  | LC_ALL=C sort >"$temp_root/expected"
find "$dist_dir" -mindepth 1 -maxdepth 1 -type f -exec basename {} \; \
  | LC_ALL=C sort >"$temp_root/actual"
cmp "$temp_root/expected" "$temp_root/actual"

while IFS= read -r -d '' entry; do
  if [[ -L "$entry" || ! -f "$entry" ]]; then
    printf 'portal-dist contains a non-regular entry: %s\n' "$entry" >&2
    exit 1
  fi
done < <(find "$dist_dir" -mindepth 1 -maxdepth 1 -print0)

if find "$dist_dir" -type f -name '*.test.mjs' -print -quit | grep -q .; then
  printf 'portal-dist contains a test suite\n' >&2
  exit 1
fi

printf 'portal_runtime_payload_tests=ok\n'
