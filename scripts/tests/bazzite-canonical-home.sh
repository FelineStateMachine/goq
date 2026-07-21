#!/usr/bin/env bash

set -euo pipefail

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/sigil-home-path.XXXXXX")"
cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  case "$tmp_root" in
    */sigil-home-path.??????) rm -rf -- "$tmp_root" ;;
  esac
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

release_id=0123456789abcdef
canonical_releases="$tmp_root/var/home/tank/.local/libexec/sigil-spark/releases"
install -d -m 0700 "$canonical_releases/$release_id"
ln -s var/home "$tmp_root/home"
install_root="$tmp_root/home/tank/.local/libexec/sigil-spark"
ln -s "releases/$release_id" "$install_root/current"

resolved_current="$(readlink -f "$install_root/current")"
spelled_releases="$install_root/releases"
[[ "$(dirname -- "$resolved_current")" != "$spelled_releases" ]]
[[ "$(dirname -- "$resolved_current")" == "$(readlink -f "$spelled_releases")" ]]

repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
for script in \
  install-bazzite-package.sh \
  rollback-bazzite-release.sh \
  stage-bazzite-release.sh
do
  # Intentional literal: the production scripts must canonicalize this exact
  # runtime variable rather than the test shell expanding it here.
  # shellcheck disable=SC2016
  grep -Fq '$(readlink -f "$releases_root")' "$repo_dir/scripts/$script"
done

printf 'bazzite_canonical_home_tests=ok\n'
