#!/usr/bin/env bash
# The test sources production helpers dynamically, so ShellCheck cannot see
# their references to the fixture overrides and shared state variables.
# shellcheck disable=SC2034,SC2329

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/../.." && pwd -P)"
temp_parent="${TMPDIR:-/tmp}"
temp_parent="${temp_parent%/}"
temp_root="$(mktemp -d "$temp_parent/sigil-release-activation.XXXXXX")"

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  case "$temp_root" in
    "$temp_parent"/sigil-release-activation.??????) rm -rf -- "$temp_root" ;;
  esac
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

# Production targets GNU mv on Bazzite. Translate its atomic no-target-directory
# form to the equivalent operation for this macOS-hosted shell test.
mv() {
  if [[ "${1:-}" == -Tf && "${2:-}" == -- && "$#" -eq 4 ]]; then
    command mv -f -- "$3" "$4"
  else
    command mv "$@"
  fi
}

extract_activation_helper() {
  local source_path="$1"
  local output_path="$2"
  sed -n \
    '/^# BEGIN release activation transaction helpers$/,/^# END release activation transaction helpers$/p' \
    "$source_path" | sed '1d;$d' >"$output_path"
  grep -Fq 'activate_release_links()' "$output_path"
}

install_helper="$temp_root/install-activation.sh"
stage_helper="$temp_root/stage-activation.sh"
rollback_helper="$temp_root/rollback-activation.sh"
extract_activation_helper "$repo_dir/scripts/install-bazzite-package.sh" "$install_helper"
extract_activation_helper "$repo_dir/scripts/stage-bazzite-release.sh" "$stage_helper"
extract_activation_helper "$repo_dir/scripts/rollback-bazzite-release.sh" "$rollback_helper"
cmp "$install_helper" "$stage_helper"
cmp "$install_helper" "$rollback_helper"

success_root="$temp_root/success"
install -d -m 0700 "$success_root"
(
  current_link="$success_root/current"
  previous_link="$success_root/previous"
  next_current="$success_root/.next-current"
  next_previous="$success_root/.next-previous"
  ln -s releases/old "$current_link"
  ln -s releases/older "$previous_link"
  ln -s releases/new "$next_current"
  ln -s releases/old "$next_previous"
  # shellcheck source=/dev/null
  source "$install_helper"
  snapshot_activation_links
  activate_release_links
  [[ "$(readlink "$current_link")" == releases/new ]]
  [[ "$(readlink "$previous_link")" == releases/old ]]
  restore_activation_links
  [[ "$(readlink "$current_link")" == releases/new ]]
  [[ "$(readlink "$previous_link")" == releases/old ]]
)

reload_failure_root="$temp_root/reload-failure"
install -d -m 0700 "$reload_failure_root"
(
  current_link="$reload_failure_root/current"
  previous_link="$reload_failure_root/previous"
  next_current="$reload_failure_root/.next-current"
  next_previous="$reload_failure_root/.next-previous"
  ln -s releases/old "$current_link"
  ln -s releases/older "$previous_link"
  ln -s releases/new "$next_current"
  ln -s releases/old "$next_previous"
  # shellcheck source=/dev/null
  source "$install_helper"
  reload_calls=0
  systemctl() {
    reload_calls=$((reload_calls + 1))
    [[ "$reload_calls" -gt 1 ]]
  }
  snapshot_activation_links
  activation_reload_service=true
  if activate_release_links; then
    printf 'FAIL: activation unexpectedly survived daemon-reload failure\n' >&2
    exit 1
  fi
  restore_activation_links
  [[ "$(readlink "$current_link")" == releases/old ]]
  [[ "$(readlink "$previous_link")" == releases/older ]]
  [[ "$reload_calls" -eq 2 ]]
)

empty_root="$temp_root/empty"
install -d -m 0700 "$empty_root"
(
  current_link="$empty_root/current"
  previous_link="$empty_root/previous"
  next_current="$empty_root/.next-current"
  next_previous="$empty_root/.next-previous"
  ln -s releases/new "$next_current"
  # shellcheck source=/dev/null
  source "$install_helper"
  systemctl() { return 1; }
  snapshot_activation_links
  activation_reload_service=true
  if activate_release_links; then
    printf 'FAIL: first activation unexpectedly survived daemon-reload failure\n' >&2
    exit 1
  fi
  restore_activation_links
  [[ ! -e "$current_link" && ! -L "$current_link" ]]
  [[ ! -e "$previous_link" && ! -L "$previous_link" ]]
)

guard_helper="$temp_root/package-managed-guard.sh"
sed -n \
  '/^# BEGIN package-managed activation guard$/,/^# END package-managed activation guard$/p' \
  "$repo_dir/scripts/stage-bazzite-release.sh" | sed '1d;$d' >"$guard_helper"
grep -Fq 'refuse_package_managed_activation()' "$guard_helper"

guard_home="$temp_root/guard-home"
install_root="$guard_home/.local/libexec/sigil-spark"
service_link="$guard_home/.config/systemd/user/sigil-host.service"
install -d -m 0700 "$(dirname -- "$service_link")"
ln -s "$guard_home/unmanaged.service" "$service_link"
(
  HOME="$guard_home"
  # shellcheck source=/dev/null
  source "$guard_helper"
  die() {
    printf 'guard rejected: %s\n' "$*" >&2
    exit 1
  }
  refuse_package_managed_activation
)

ln -sfn "$install_root/current/assets/sigil-host.service" "$service_link"
guard_log="$temp_root/guard.log"
if (
  HOME="$guard_home"
  # shellcheck source=/dev/null
  source "$guard_helper"
  die() {
    printf 'guard rejected: %s\n' "$*" >&2
    exit 1
  }
  refuse_package_managed_activation
) >"$guard_log" 2>&1; then
  printf 'FAIL: thin stager accepted a package-managed activation\n' >&2
  exit 1
fi
grep -Fq 'build the runtime package and use payload/stage-this-release.sh' "$guard_log"

canonical_root="$temp_root/canonical-guard"
canonical_home="$canonical_root/var/home/tank"
install -d -m 0700 "$canonical_home"
ln -s var/home "$canonical_root/home"
spelled_home="$canonical_root/home/tank"
spelled_install_root="$spelled_home/.local/libexec/sigil-spark"
canonical_service_target="$canonical_home/.local/libexec/sigil-spark/current/assets/sigil-host.service"
canonical_service_link="$canonical_home/.config/systemd/user/sigil-host.service"
install -d -m 0700 "$(dirname -- "$canonical_service_target")" \
  "$(dirname -- "$canonical_service_link")"
printf '[Service]\n' >"$canonical_service_target"
ln -s "$canonical_service_target" "$canonical_service_link"
canonical_guard_log="$temp_root/canonical-guard.log"
if (
  HOME="$spelled_home"
  install_root="$spelled_install_root"
  # shellcheck source=/dev/null
  source "$guard_helper"
  die() {
    printf 'guard rejected: %s\n' "$*" >&2
    exit 1
  }
  refuse_package_managed_activation
) >"$canonical_guard_log" 2>&1; then
  printf 'FAIL: thin stager missed a canonically equivalent managed link\n' >&2
  exit 1
fi
grep -Fq 'build the runtime package and use payload/stage-this-release.sh' \
  "$canonical_guard_log"

printf 'bazzite_release_activation_tests=ok\n'
