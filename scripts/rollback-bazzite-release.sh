#!/usr/bin/env bash

set -euo pipefail
umask 077

usage() {
  cat <<'EOF'
Usage: rollback-bazzite-release.sh [--to RELEASE_ID] [--restart]

Atomically switch ~/.local/libexec/sigil-spark/current to the requested
installed release. Without --to, switch to the release recorded by the
previous symlink. The release is validated before activation. The former
current release becomes previous, making a second rollback reversible.

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
for command in flock ldd sha256sum systemd-analyze timeout; do
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
  [[ "$(dirname -- "$resolved")" == "$releases_root" ]] || die "$label activation escapes the release root"
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
for binary in sigil-host sigil-probe; do
  path="$target_path/$binary"
  [[ -f "$path" && ! -L "$path" && -x "$path" ]] || die "target $binary is not a regular executable"
  mode="$(stat -Lc '%a' "$path")"
  (( (8#$mode & 8#022) == 0 )) || die "target $binary is group/world writable"
done

ldd_output="$(ldd "$target_path/sigil-host")"
printf '%s\n' "$ldd_output"
grep -q 'not found' <<<"$ldd_output" && die "target host binary has an unresolved shared library"
timeout --signal=TERM --kill-after=2s 5s "$target_path/sigil-host" --version
timeout --signal=TERM --kill-after=2s 5s "$target_path/sigil-probe" --help >/dev/null
systemd-analyze --user verify "$target_path/assets/sigil-host.service"

next_current="$install_root/.current-rollback-$$"
next_previous="$install_root/.previous-rollback-$$"
cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  [[ ! -L "$next_current" ]] || rm -f -- "$next_current"
  [[ ! -L "$next_previous" ]] || rm -f -- "$next_previous"
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

ln -s "releases/$target_release_id" "$next_current"
ln -s "releases/$current_release_id" "$next_previous"
mv -Tf -- "$next_previous" "$previous_link"
mv -Tf -- "$next_current" "$current_link"

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
