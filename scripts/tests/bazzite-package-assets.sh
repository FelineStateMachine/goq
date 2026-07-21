#!/usr/bin/env bash

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/../.." && pwd -P)"
installer="$repo_dir/scripts/install-bazzite-package.sh"
temp_parent="${TMPDIR:-/tmp}"
temp_parent="${temp_parent%/}"
temp_root="$(mktemp -d "$temp_parent/sigil-package-assets.XXXXXX")"
helper_script="$temp_root/managed-assets.sh"

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  case "$temp_root" in
    "$temp_parent"/sigil-package-assets.??????) rm -rf -- "$temp_root" ;;
  esac
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

sed -n \
  '/^# BEGIN managed asset helpers /,/^# END managed asset helpers$/p' \
  "$installer" | sed '1d;$d' >"$helper_script"
grep -Fq 'install_managed_links()' "$helper_script"

# shellcheck source=/dev/null
source "$helper_script"

die() {
  printf 'test installer rejected: %s\n' "$*" >&2
  exit 1
}

# Referenced by the sourced production helper block.
# shellcheck disable=SC2034
release_id=0123456789abcdef
# shellcheck disable=SC2034
next_managed_links=()

make_assets() {
  local root="$1"
  local index
  install -d -m 0700 "$root/assets" "$root/links"
  for index in 0 1 2 3; do
    printf 'release asset %s\n' "$index" >"$root/assets/$index"
  done
}

set_managed_arrays() {
  local root="$1"
  managed_links=(
    "$root/links/0"
    "$root/links/1"
    "$root/links/2"
    "$root/links/3"
  )
  managed_targets=(
    "$root/future/0"
    "$root/future/1"
    "$root/future/2"
    "$root/future/3"
  )
  managed_release_assets=(
    "$root/assets/0"
    "$root/assets/1"
    "$root/assets/2"
    "$root/assets/3"
  )
}

success_root="$temp_root/success"
make_assets "$success_root"
set_managed_arrays "$success_root"
ln -s "${managed_targets[0]}" "${managed_links[0]}"
cp "${managed_release_assets[2]}" "${managed_links[2]}"
cp "${managed_release_assets[3]}" "${managed_links[3]}"
install_managed_links
for index in 0 1 2 3; do
  [[ -L "${managed_links[$index]}" ]]
  [[ "$(readlink "${managed_links[$index]}")" == "${managed_targets[$index]}" ]]
done

mismatch_root="$temp_root/mismatch"
make_assets "$mismatch_root"
set_managed_arrays "$mismatch_root"
cp "${managed_release_assets[1]}" "${managed_links[1]}"
ln -s "${managed_targets[2]}" "${managed_links[2]}"
printf 'locally modified\n' >"${managed_links[3]}"
if ( install_managed_links ); then
  printf 'FAIL: mismatched regular asset unexpectedly succeeded\n' >&2
  exit 1
fi
[[ ! -e "${managed_links[0]}" && ! -L "${managed_links[0]}" ]]
[[ -f "${managed_links[1]}" && ! -L "${managed_links[1]}" ]]
cmp -s "${managed_release_assets[1]}" "${managed_links[1]}"
[[ -L "${managed_links[2]}" ]]
[[ "$(readlink "${managed_links[2]}")" == "${managed_targets[2]}" ]]
[[ -f "${managed_links[3]}" && ! -L "${managed_links[3]}" ]]
grep -Fq 'locally modified' "${managed_links[3]}"

unsafe_root="$temp_root/unsafe"
make_assets "$unsafe_root"
set_managed_arrays "$unsafe_root"
cp "${managed_release_assets[1]}" "${managed_links[1]}"
ln -s "${managed_targets[2]}" "${managed_links[2]}"
mkfifo "${managed_links[3]}"
if ( install_managed_links ); then
  printf 'FAIL: unsafe managed asset type unexpectedly succeeded\n' >&2
  exit 1
fi
[[ ! -e "${managed_links[0]}" && ! -L "${managed_links[0]}" ]]
[[ -f "${managed_links[1]}" && ! -L "${managed_links[1]}" ]]
[[ -L "${managed_links[2]}" ]]
[[ -p "${managed_links[3]}" ]]

# Intentional literal verifies the ownership gate in the extracted production helper.
# shellcheck disable=SC2016
grep -Fq '[[ -f "$link" && -O "$link" ]]' "$helper_script"
printf 'bazzite_package_asset_tests=ok\n'
