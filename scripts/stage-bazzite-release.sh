#!/usr/bin/env bash

set -euo pipefail
umask 077

usage() {
  cat <<'EOF'
Usage: scripts/stage-bazzite-release.sh \
  --release-id HEX \
  --host-binary PATH --host-sha256 HEX \
  --probe-binary PATH --probe-sha256 HEX

Verify and atomically stage prebuilt x86_64 Linux Sigil binaries beneath:

  ~/.local/libexec/sigil-spark/releases/<release-id>/

The script changes the `current` symlink only after both installed binaries
pass their hashes, dynamic-library check, and bounded startup checks. It does
not create an identity, install configuration, or start/enable a service.
EOF
}

die() {
  printf 'stage release failed: %s\n' "$*" >&2
  exit 1
}

release_id=""
host_binary=""
host_sha256=""
probe_binary=""
probe_sha256=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --release-id)
      [[ $# -ge 2 ]] || die "--release-id requires a value"
      release_id="$2"
      shift 2
      ;;
    --host-binary)
      [[ $# -ge 2 ]] || die "--host-binary requires a value"
      host_binary="$2"
      shift 2
      ;;
    --host-sha256)
      [[ $# -ge 2 ]] || die "--host-sha256 requires a value"
      host_sha256="$2"
      shift 2
      ;;
    --probe-binary)
      [[ $# -ge 2 ]] || die "--probe-binary requires a value"
      probe_binary="$2"
      shift 2
      ;;
    --probe-sha256)
      [[ $# -ge 2 ]] || die "--probe-sha256 requires a value"
      probe_sha256="$2"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ "$(uname -s)" == Linux ]] || die "this stager must run on Linux"
[[ "$(uname -m)" == x86_64 ]] || die "expected x86_64, got $(uname -m)"
[[ "$(id -u)" -ne 0 ]] || die "run as the dedicated gaming user, not root"
[[ "$release_id" =~ ^[0-9a-f]{7,64}$ ]] || die "release ID must be 7-64 lowercase hexadecimal characters"
[[ "$host_sha256" =~ ^[0-9a-f]{64}$ ]] || die "host SHA-256 must be 64 lowercase hexadecimal characters"
[[ "$probe_sha256" =~ ^[0-9a-f]{64}$ ]] || die "probe SHA-256 must be 64 lowercase hexadecimal characters"

command -v sha256sum >/dev/null 2>&1 || die "sha256sum is required"
command -v ldd >/dev/null 2>&1 || die "ldd is required"
command -v timeout >/dev/null 2>&1 || die "timeout is required"

validate_input() {
  local label="$1"
  local path="$2"
  local expected_sha256="$3"
  local actual_sha256
  local mode

  [[ "$path" == /* ]] || die "$label path must be absolute"
  [[ ! -L "$path" ]] || die "$label must not be a symlink"
  [[ -f "$path" ]] || die "$label is not a regular file: $path"
  [[ "$(stat -Lc '%u' "$path")" -eq "$(id -u)" ]] || die "$label must be owned by the current user"
  mode="$(stat -Lc '%a' "$path")"
  (( (8#$mode & 8#022) == 0 )) || die "$label must not be group/world writable"

  actual_sha256="$(sha256sum "$path" | awk '{print $1}')"
  [[ "$actual_sha256" == "$expected_sha256" ]] || die "$label SHA-256 mismatch"
}

validate_input "host binary" "$host_binary" "$host_sha256"
validate_input "probe binary" "$probe_binary" "$probe_sha256"

install_root="$HOME/.local/libexec/sigil-spark"
releases_root="$install_root/releases"
release_path="$releases_root/$release_id"
staging_path="$releases_root/.stage-$release_id-$$"
current_link="$install_root/current"
next_link="$install_root/.current-$release_id-$$"
previous_link="$install_root/previous"
next_previous_link="$install_root/.previous-$release_id-$$"

for directory in "$HOME/.local" "$HOME/.local/libexec" "$install_root" "$releases_root"; do
  if [[ -e "$directory" ]]; then
    [[ -d "$directory" && ! -L "$directory" ]] || die "unsafe install directory: $directory"
    [[ "$(stat -Lc '%u' "$directory")" -eq "$(id -u)" ]] || die "install directory is not owned by current user: $directory"
    mode="$(stat -Lc '%a' "$directory")"
    (( (8#$mode & 8#022) == 0 )) || die "install directory is group/world writable: $directory"
  else
    install -d -m 0755 "$directory"
  fi
done

cleanup() {
  local exit_status=$?
  trap - EXIT INT TERM HUP
  if [[ -d "$staging_path" ]]; then
    rm -f -- "$staging_path/sigil-host" "$staging_path/sigil-probe"
    rmdir -- "$staging_path" 2>/dev/null || true
  fi
  [[ ! -L "$next_link" ]] || rm -f -- "$next_link"
  [[ ! -L "$next_previous_link" ]] || rm -f -- "$next_previous_link"
  exit "$exit_status"
}
trap cleanup EXIT INT TERM HUP

if [[ -e "$release_path" || -L "$release_path" ]]; then
  [[ -d "$release_path" && ! -L "$release_path" ]] || die "unsafe existing release path: $release_path"
  [[ "$(sha256sum "$release_path/sigil-host" | awk '{print $1}')" == "$host_sha256" ]] || die "existing release host hash differs"
  [[ "$(sha256sum "$release_path/sigil-probe" | awk '{print $1}')" == "$probe_sha256" ]] || die "existing release probe hash differs"
else
  install -d -m 0755 "$staging_path"
  install -m 0755 "$host_binary" "$staging_path/sigil-host"
  install -m 0755 "$probe_binary" "$staging_path/sigil-probe"
  [[ "$(sha256sum "$staging_path/sigil-host" | awk '{print $1}')" == "$host_sha256" ]] || die "installed host hash differs"
  [[ "$(sha256sum "$staging_path/sigil-probe" | awk '{print $1}')" == "$probe_sha256" ]] || die "installed probe hash differs"
  mv -- "$staging_path" "$release_path"
fi

ldd_output="$(ldd "$release_path/sigil-host")"
printf '%s\n' "$ldd_output"
grep -q 'not found' <<<"$ldd_output" && die "host binary has an unresolved shared library"
timeout --signal=TERM --kill-after=2s 5s "$release_path/sigil-host" --version
timeout --signal=TERM --kill-after=2s 5s "$release_path/sigil-probe" --help >/dev/null

current_release_id=""
if [[ -e "$current_link" || -L "$current_link" ]]; then
  [[ -L "$current_link" ]] || die "current activation is not a symlink"
  current_path="$(readlink -f "$current_link")"
  [[ -n "$current_path" && -d "$current_path" ]] || die "current activation is dangling"
  [[ "$(dirname -- "$current_path")" == "$releases_root" ]] || die "current activation escapes the release root"
  current_release_id="$(basename -- "$current_path")"
  [[ "$current_release_id" =~ ^[0-9a-f]{7,64}$ ]] || die "current activation has an invalid release ID"
fi

if [[ -e "$previous_link" || -L "$previous_link" ]]; then
  [[ -L "$previous_link" ]] || die "previous activation is not a symlink"
fi

ln -s "releases/$release_id" "$next_link"
if [[ -n "$current_release_id" && "$current_release_id" != "$release_id" ]]; then
  ln -s "releases/$current_release_id" "$next_previous_link"
  mv -Tf -- "$next_previous_link" "$previous_link"
fi
mv -Tf -- "$next_link" "$current_link"

printf 'release_id=%s\n' "$release_id"
printf 'host_sha256=%s\n' "$(sha256sum "$release_path/sigil-host" | awk '{print $1}')"
printf 'probe_sha256=%s\n' "$(sha256sum "$release_path/sigil-probe" | awk '{print $1}')"
printf 'current=%s\n' "$(readlink -f "$current_link")"
if [[ -L "$previous_link" ]]; then
  printf 'previous=%s\n' "$(readlink -f "$previous_link")"
else
  printf 'previous=none\n'
fi
printf 'release_activation=current-symlink-updated\n'
printf 'service_activation=not-attempted\n'
