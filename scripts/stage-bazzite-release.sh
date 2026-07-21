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

The script changes the `current` symlink only after the installed `sigil`
executable, its byte-identical `sigil-host` compatibility copy, and
`sigil-probe` pass their hashes, dynamic-library check, and bounded startup
checks. It does not create an identity, install configuration, or start/enable
a service. It refuses to replace `current` when package-managed assets follow
that link; build the runtime package and use `payload/stage-this-release.sh`
for upgrades on a package-managed host.
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

command -v cmp >/dev/null 2>&1 || die "cmp is required"
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
next_current="$install_root/.current-$release_id-$$"
previous_link="$install_root/previous"
next_previous="$install_root/.previous-$release_id-$$"

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

# BEGIN package-managed activation guard
refuse_package_managed_activation() {
  local -a managed_links=(
    "$HOME/.config/pipewire/pipewire-pulse.conf.d/50-sigil-spark-audio.conf"
    "$HOME/.config/systemd/user/sigil-host.service"
    "$HOME/.local/bin/sigil-spark-host-rollback"
    "$HOME/.local/share/sigil-spark/package-assets/70-sigil-remote-input.rules"
  )
  local -a managed_targets=(
    "$install_root/current/assets/50-sigil-spark-audio.conf"
    "$install_root/current/assets/sigil-host.service"
    "$install_root/current/tools/rollback-bazzite-release.sh"
    "$install_root/current/assets/70-sigil-remote-input.rules"
  )
  local index
  local link_target
  local resolved_link
  local resolved_target

  for index in "${!managed_links[@]}"; do
    if [[ -L "${managed_links[$index]}" ]]; then
      link_target="$(readlink "${managed_links[$index]}")"
      resolved_link="$(readlink -f "${managed_links[$index]}" 2>/dev/null || true)"
      resolved_target="$(readlink -f "${managed_targets[$index]}" 2>/dev/null || true)"
      if [[ "$link_target" == "${managed_targets[$index]}" \
        || ( -n "$resolved_link" && "$resolved_link" == "$resolved_target" ) ]]; then
        die "package-managed activation detected at ${managed_links[$index]}; build the runtime package and use payload/stage-this-release.sh"
      fi
    fi
  done
}
# END package-managed activation guard

refuse_package_managed_activation

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
  restore_activation_links
  if [[ -d "$staging_path" ]]; then
    rm -f -- "$staging_path/sigil" "$staging_path/sigil-host" "$staging_path/sigil-probe"
    rmdir -- "$staging_path" 2>/dev/null || true
  fi
  [[ ! -L "$next_current" ]] || rm -f -- "$next_current"
  [[ ! -L "$next_previous" ]] || rm -f -- "$next_previous"
  [[ ! -L "$restore_current_temp" ]] || rm -f -- "$restore_current_temp"
  [[ ! -L "$restore_previous_temp" ]] || rm -f -- "$restore_previous_temp"
  exit "$exit_status"
}
trap cleanup EXIT INT TERM HUP

if [[ -e "$release_path" || -L "$release_path" ]]; then
  [[ -d "$release_path" && ! -L "$release_path" ]] || die "unsafe existing release path: $release_path"
  [[ "$(sha256sum "$release_path/sigil" | awk '{print $1}')" == "$host_sha256" ]] || die "existing release host hash differs"
  [[ "$(sha256sum "$release_path/sigil-host" | awk '{print $1}')" == "$host_sha256" ]] || die "existing release compatibility host hash differs"
  [[ "$(sha256sum "$release_path/sigil-probe" | awk '{print $1}')" == "$probe_sha256" ]] || die "existing release probe hash differs"
else
  install -d -m 0755 "$staging_path"
  install -m 0755 "$host_binary" "$staging_path/sigil"
  install -m 0755 "$host_binary" "$staging_path/sigil-host"
  install -m 0755 "$probe_binary" "$staging_path/sigil-probe"
  [[ "$(sha256sum "$staging_path/sigil" | awk '{print $1}')" == "$host_sha256" ]] || die "installed host hash differs"
  [[ "$(sha256sum "$staging_path/sigil-host" | awk '{print $1}')" == "$host_sha256" ]] || die "installed compatibility host hash differs"
  [[ "$(sha256sum "$staging_path/sigil-probe" | awk '{print $1}')" == "$probe_sha256" ]] || die "installed probe hash differs"
  mv -- "$staging_path" "$release_path"
fi

cmp -s "$release_path/sigil" "$release_path/sigil-host" \
  || die "sigil-host compatibility executable differs from sigil"
ldd_output="$(ldd "$release_path/sigil")"
printf '%s\n' "$ldd_output"
grep -q 'not found' <<<"$ldd_output" && die "host binary has an unresolved shared library"
timeout --signal=TERM --kill-after=2s 5s "$release_path/sigil" --version
timeout --signal=TERM --kill-after=2s 5s "$release_path/sigil-probe" --help >/dev/null

current_release_id=""
if [[ -e "$current_link" || -L "$current_link" ]]; then
  [[ -L "$current_link" ]] || die "current activation is not a symlink"
  current_path="$(readlink -f "$current_link")"
  [[ -n "$current_path" && -d "$current_path" ]] || die "current activation is dangling"
  # Fedora Atomic/Bazzite exposes /home through /var/home. Compare canonical
  # paths on both sides so the managed symlink does not appear to escape.
  [[ "$(dirname -- "$current_path")" == "$(readlink -f "$releases_root")" ]] \
    || die "current activation escapes the release root"
  current_release_id="$(basename -- "$current_path")"
  [[ "$current_release_id" =~ ^[0-9a-f]{7,64}$ ]] || die "current activation has an invalid release ID"
fi

if [[ -e "$previous_link" || -L "$previous_link" ]]; then
  [[ -L "$previous_link" ]] || die "previous activation is not a symlink"
fi
snapshot_activation_links

ln -s "releases/$release_id" "$next_current"
if [[ -n "$current_release_id" && "$current_release_id" != "$release_id" ]]; then
  ln -s "releases/$current_release_id" "$next_previous"
fi
activate_release_links

printf 'release_id=%s\n' "$release_id"
printf 'host_sha256=%s\n' "$(sha256sum "$release_path/sigil" | awk '{print $1}')"
printf 'probe_sha256=%s\n' "$(sha256sum "$release_path/sigil-probe" | awk '{print $1}')"
printf 'current=%s\n' "$(readlink -f "$current_link")"
if [[ -L "$previous_link" ]]; then
  printf 'previous=%s\n' "$(readlink -f "$previous_link")"
else
  printf 'previous=none\n'
fi
printf 'release_activation=current-symlink-updated\n'
printf 'service_activation=not-attempted\n'
