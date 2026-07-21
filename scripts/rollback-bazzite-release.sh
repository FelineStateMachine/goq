#!/usr/bin/env bash

set -euo pipefail
umask 077

usage() {
  cat <<'EOF'
Usage: rollback-bazzite-release.sh [--to RELEASE_ID] [--restart]

Atomically switch ~/.local/libexec/sigil-spark/current to the requested
installed release. Without --to, switch to the release recorded by the
previous symlink. The release is validated before activation, including
legacy releases that predate the primary `sigil` executable and contain only
`sigil-host`. The former current release becomes previous, making a second
rollback reversible.

The host service is not restarted unless --restart is explicit.
EOF
}

die() {
  printf 'rollback failed: %s\n' "$*" >&2
  exit 1
}

target_release_id=""
restart_service=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --to)
      [[ $# -ge 2 ]] || die "--to requires a release ID"
      target_release_id="$2"
      shift 2
      ;;
    --restart)
      restart_service=true
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ "$(uname -s)" == Linux ]] || die "this rollback helper must run on Linux"
[[ "$(uname -m)" == x86_64 ]] || die "expected x86_64, got $(uname -m)"
[[ "$(id -u)" -ne 0 ]] || die "run as the dedicated gaming user, not root"
for command in cmp flock ldd sha256sum systemctl systemd-analyze timeout; do
  command -v "$command" >/dev/null 2>&1 || die "$command is required"
done

install_root="$HOME/.local/libexec/sigil-spark"
releases_root="$install_root/releases"
current_link="$install_root/current"
previous_link="$install_root/previous"
lock_path="$install_root/.install.lock"

[[ -d "$install_root" && ! -L "$install_root" ]] || die "install root is missing or unsafe"
exec 9>"$lock_path"
flock -x 9

resolve_activation() {
  local label="$1"
  local link="$2"
  local resolved

  [[ -L "$link" ]] || die "$label activation is missing or is not a symlink"
  resolved="$(readlink -f "$link")"
  [[ -n "$resolved" && -d "$resolved" ]] || die "$label activation is dangling"
  # Fedora Atomic/Bazzite exposes /home through /var/home. Compare canonical
  # paths on both sides so the managed symlink does not appear to escape.
  [[ "$(dirname -- "$resolved")" == "$(readlink -f "$releases_root")" ]] \
    || die "$label activation escapes the release root"
  basename -- "$resolved"
}

current_release_id="$(resolve_activation current "$current_link")"
[[ "$current_release_id" =~ ^[0-9a-f]{7,64}$ ]] || die "current activation has an invalid release ID"

if [[ -z "$target_release_id" ]]; then
  target_release_id="$(resolve_activation previous "$previous_link")"
fi
[[ "$target_release_id" =~ ^[0-9a-f]{7,64}$ ]] || die "target release ID must be 7-64 lowercase hexadecimal characters"
[[ "$target_release_id" != "$current_release_id" ]] || die "target release is already current"

target_path="$releases_root/$target_release_id"
next_current="$install_root/.current-rollback-$$"
next_previous="$install_root/.previous-rollback-$$"
verify_unit="$install_root/.verify-rollback-$target_release_id-$$.service"

# BEGIN release activation transaction helpers
activation_mutated=false
activation_committed=false
activation_reload_service=false
original_current_present=false
original_current_target=""
original_previous_present=false
original_previous_target=""
restore_current_temp="${current_link}.restore-$$"
restore_previous_temp="${previous_link}.restore-$$"

snapshot_activation_links() {
  if [[ -L "$current_link" ]]; then
    original_current_present=true
    original_current_target="$(readlink "$current_link")"
  else
    original_current_present=false
    original_current_target=""
  fi
  if [[ -L "$previous_link" ]]; then
    original_previous_present=true
    original_previous_target="$(readlink "$previous_link")"
  else
    original_previous_present=false
    original_previous_target=""
  fi
}

restore_activation_link() {
  local link="$1"
  local present="$2"
  local target="$3"
  local restore_temp="$4"

  rm -f -- "$restore_temp"
  if [[ "$present" == true ]]; then
    ln -s "$target" "$restore_temp" || return 1
    mv -Tf -- "$restore_temp" "$link" || return 1
  elif [[ -L "$link" ]]; then
    rm -f -- "$link" || return 1
  elif [[ -e "$link" ]]; then
    return 1
  fi
}

restore_activation_links() {
  if ! $activation_mutated || $activation_committed; then
    return 0
  fi

  restore_activation_link \
    "$current_link" "$original_current_present" "$original_current_target" \
    "$restore_current_temp" || true
  restore_activation_link \
    "$previous_link" "$original_previous_present" "$original_previous_target" \
    "$restore_previous_temp" || true
  if $activation_reload_service; then
    systemctl --user daemon-reload >/dev/null 2>&1 || true
  fi
}

activate_release_links() {
  activation_mutated=true
  if [[ -L "$next_previous" ]]; then
    mv -Tf -- "$next_previous" "$previous_link" || return 1
  fi
  mv -Tf -- "$next_current" "$current_link" || return 1
  if $activation_reload_service; then
    systemctl --user daemon-reload || return 1
  fi
  activation_committed=true
}
# END release activation transaction helpers

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  restore_activation_links
  [[ ! -L "$next_current" ]] || rm -f -- "$next_current"
  [[ ! -L "$next_previous" ]] || rm -f -- "$next_previous"
  [[ ! -L "$restore_current_temp" ]] || rm -f -- "$restore_current_temp"
  [[ ! -L "$restore_previous_temp" ]] || rm -f -- "$restore_previous_temp"
  [[ ! -f "$verify_unit" ]] || rm -f -- "$verify_unit"
  exit "$status"
}
trap cleanup EXIT INT TERM HUP
snapshot_activation_links
activation_reload_service=true

[[ -d "$target_path" && ! -L "$target_path" ]] || die "target release is not an installed directory"
[[ "$(stat -Lc '%u' "$target_path")" -eq "$(id -u)" ]] || die "target release is not owned by the current user"
[[ -f "$target_path/SHA256SUMS" && ! -L "$target_path/SHA256SUMS" ]] \
  || die "target release checksums are missing or unsafe"
[[ "$(sha256sum "$target_path/SHA256SUMS" | awk '{print $1}')" == "$target_release_id" ]] \
  || die "target release ID does not bind its checksums"
(
  cd "$target_path"
  sha256sum -c SHA256SUMS
)

# BEGIN release host executable compatibility helper
release_host_executable() {
  local release_path="$1"

  if [[ -e "$release_path/sigil" || -L "$release_path/sigil" ]]; then
    [[ -f "$release_path/sigil" && ! -L "$release_path/sigil" \
      && -x "$release_path/sigil" ]] || return 1
    [[ -f "$release_path/sigil-host" && ! -L "$release_path/sigil-host" \
      && -x "$release_path/sigil-host" ]] || return 1
    cmp -s "$release_path/sigil" "$release_path/sigil-host" || return 1
    printf '%s\n' "$release_path/sigil"
    return 0
  fi
  if [[ -f "$release_path/sigil-host" && ! -L "$release_path/sigil-host" \
    && -x "$release_path/sigil-host" ]]; then
    printf '%s\n' "$release_path/sigil-host"
    return 0
  fi
  return 1
}
# END release host executable compatibility helper

host_executable="$(release_host_executable "$target_path")" \
  || die "target release has no valid host executable layout"
release_binaries=("$host_executable" "$target_path/sigil-probe")
if [[ "$host_executable" == "$target_path/sigil" ]]; then
  release_binaries+=("$target_path/sigil-host")
fi
for path in "${release_binaries[@]}"; do
  binary="$(basename -- "$path")"
  [[ -f "$path" && ! -L "$path" && -x "$path" ]] || die "target $binary is not a regular executable"
  mode="$(stat -Lc '%a' "$path")"
  (( (8#$mode & 8#022) == 0 )) || die "target $binary is group/world writable"
done

ldd_output="$(ldd "$host_executable")"
printf '%s\n' "$ldd_output"
grep -q 'not found' <<<"$ldd_output" && die "target host binary has an unresolved shared library"
timeout --signal=TERM --kill-after=2s 5s "$host_executable" --version
timeout --signal=TERM --kill-after=2s 5s "$target_path/sigil-probe" --help >/dev/null
awk -v executable="$host_executable" '
  /^ExecStart=/ {
    print "ExecStart=" executable " serve --config=%h/.config/sigil-spark/host.toml"
    next
  }
  { print }
' "$target_path/assets/sigil-host.service" >"$verify_unit"
chmod 0600 "$verify_unit"
systemd-analyze --user verify "$verify_unit"
rm -f -- "$verify_unit"

ln -s "releases/$target_release_id" "$next_current"
ln -s "releases/$current_release_id" "$next_previous"
activate_release_links

printf 'current=%s\n' "$(readlink -f "$current_link")"
printf 'previous=%s\n' "$(readlink -f "$previous_link")"
printf 'release_activation=rolled-back\n'

if $restart_service; then
  systemctl --user restart sigil-host.service
  systemctl --user is-active --quiet sigil-host.service
  printf 'service_activation=restarted\n'
else
  printf 'service_activation=not-attempted\n'
fi
