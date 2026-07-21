#!/usr/bin/env bash

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/../.." && pwd -P)"
rollback="$repo_dir/scripts/rollback-bazzite-release.sh"
temp_parent="${TMPDIR:-/tmp}"
temp_parent="${temp_parent%/}"
temp_root="$(mktemp -d "$temp_parent/sigil-host-layout.XXXXXX")"
helper_script="$temp_root/release-host-executable.sh"

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  case "$temp_root" in
    "$temp_parent"/sigil-host-layout.??????) rm -rf -- "$temp_root" ;;
  esac
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

sed -n \
  '/^# BEGIN release host executable compatibility helper$/,/^# END release host executable compatibility helper$/p' \
  "$rollback" | sed '1d;$d' >"$helper_script"
grep -Fq 'release_host_executable()' "$helper_script"

# shellcheck source=/dev/null
source "$helper_script"

make_executable() {
  local path="$1"
  local marker="$2"
  printf '#!/usr/bin/env bash\n# %s\nexit 0\n' "$marker" >"$path"
  chmod 0755 "$path"
}

legacy="$temp_root/legacy"
install -d -m 0700 "$legacy"
make_executable "$legacy/sigil-host" legacy
[[ "$(release_host_executable "$legacy")" == "$legacy/sigil-host" ]]

current="$temp_root/current"
install -d -m 0700 "$current"
make_executable "$current/sigil" current
cp "$current/sigil" "$current/sigil-host"
chmod 0755 "$current/sigil-host"
[[ "$(release_host_executable "$current")" == "$current/sigil" ]]

missing_alias="$temp_root/missing-alias"
install -d -m 0700 "$missing_alias"
make_executable "$missing_alias/sigil" current
if release_host_executable "$missing_alias" >/dev/null; then
  printf 'FAIL: new release without sigil-host compatibility copy was accepted\n' >&2
  exit 1
fi

mismatched_alias="$temp_root/mismatched-alias"
install -d -m 0700 "$mismatched_alias"
make_executable "$mismatched_alias/sigil" current
make_executable "$mismatched_alias/sigil-host" different
if release_host_executable "$mismatched_alias" >/dev/null; then
  printf 'FAIL: mismatched sigil-host compatibility copy was accepted\n' >&2
  exit 1
fi

unsafe_primary="$temp_root/unsafe-primary"
install -d -m 0700 "$unsafe_primary"
make_executable "$unsafe_primary/elsewhere" current
ln -s elsewhere "$unsafe_primary/sigil"
cp "$unsafe_primary/elsewhere" "$unsafe_primary/sigil-host"
chmod 0755 "$unsafe_primary/sigil-host"
if release_host_executable "$unsafe_primary" >/dev/null; then
  printf 'FAIL: symlink primary executable was accepted as a legacy layout\n' >&2
  exit 1
fi

printf 'bazzite_host_executable_layout_tests=ok\n'
