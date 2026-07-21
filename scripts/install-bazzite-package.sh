#!/usr/bin/env bash

set -euo pipefail
umask 077

usage() {
  cat <<'EOF'
Usage: install-bazzite-package.sh [--payload-dir PATH]

Verify and install one Sigil Bazzite host runtime package. The release is
fully staged and validated before the current symlink changes. New packages
contain the primary `sigil` executable and a byte-identical `sigil-host`
compatibility copy. Package-owned user assets follow current, so upgrades and
rollbacks keep binaries, service, and PipeWire configuration on the same
release.

This command never creates or replaces host identity/configuration, restarts
PipeWire, starts/enables the host service, changes groups, or writes /etc.
EOF
}

die() {
  printf 'package install failed: %s\n' "$*" >&2
  exit 1
}

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
payload_dir="$script_dir"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --payload-dir)
      [[ $# -ge 2 ]] || die "--payload-dir requires a path"
      payload_dir="$2"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ "$(uname -s)" == Linux ]] || die "this package must be installed on Linux"
[[ "$(uname -m)" == x86_64 ]] || die "expected x86_64, got $(uname -m)"
[[ "$(id -u)" -ne 0 ]] || die "run as the dedicated gaming user, not root"
for command in cmp diff flock ldd sha256sum systemctl systemd-analyze timeout; do
  command -v "$command" >/dev/null 2>&1 || die "$command is required"
done

[[ "$payload_dir" == /* ]] || die "payload directory must be absolute"
[[ -d "$payload_dir" && ! -L "$payload_dir" ]] || die "payload directory is unsafe"
[[ "$(stat -Lc '%u' "$payload_dir")" -eq "$(id -u)" ]] || die "payload directory must be owned by the current user"
for required in PACKAGE-SHA256SUMS release-id release/SHA256SUMS; do
  [[ -f "$payload_dir/$required" && ! -L "$payload_dir/$required" ]] \
    || die "$required is missing or unsafe"
done
[[ -z "$(find "$payload_dir" -type l -print -quit)" ]] || die "package payload must not contain symlinks"
if ! diff -u \
  <(printf '%s\n' \
    PACKAGE-SHA256SUMS \
    install-bazzite-package.sh \
    release-id \
    release/LICENSE \
    release/SHA256SUMS \
    release/assets/50-sigil-spark-audio.conf \
    release/assets/70-sigil-remote-input.rules \
    release/assets/sigil-host.service \
    release/release-manifest.json \
    release/sigil \
    release/sigil-host \
    release/sigil-probe \
    release/tools/rollback-bazzite-release.sh \
    stage-this-release.sh | LC_ALL=C sort) \
  <(find "$payload_dir" -type f -printf '%P\n' | LC_ALL=C sort)
then
  die "package payload does not match the runtime allowlist"
fi
if ! diff -u \
  <(printf '%s\n' \
    LICENSE \
    assets/50-sigil-spark-audio.conf \
    assets/70-sigil-remote-input.rules \
    assets/sigil-host.service \
    release-manifest.json \
    sigil \
    sigil-host \
    sigil-probe \
    tools/rollback-bazzite-release.sh | LC_ALL=C sort) \
  <(awk '{print $2}' "$payload_dir/release/SHA256SUMS" | LC_ALL=C sort)
then
  die "release checksums do not match the installed-file allowlist"
fi

(
  cd "$payload_dir"
  sha256sum -c PACKAGE-SHA256SUMS
  cd release
  sha256sum -c SHA256SUMS
)

release_id="$(tr -d '\n' <"$payload_dir/release-id")"
[[ "$release_id" =~ ^[0-9a-f]{64}$ ]] || die "release-id is not a lowercase SHA-256"
[[ "$(sha256sum "$payload_dir/release/SHA256SUMS" | awk '{print $1}')" == "$release_id" ]] \
  || die "release ID does not bind the release checksums"

install_root="$HOME/.local/libexec/sigil-spark"
releases_root="$install_root/releases"
release_path="$releases_root/$release_id"
staging_path="$releases_root/.stage-package-$release_id-$$"
current_link="$install_root/current"
previous_link="$install_root/previous"
next_current="$install_root/.current-package-$release_id-$$"
next_previous="$install_root/.previous-package-$release_id-$$"
lock_path="$install_root/.install.lock"
verify_unit="$install_root/.verify-package-$release_id-$$.service"
next_managed_links=()

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

install -d -m 0755 "$HOME/.local" "$HOME/.local/libexec" "$install_root" "$releases_root"
exec 9>"$lock_path"
flock -x 9

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  restore_activation_links
  if [[ -d "$staging_path" ]]; then
    find "$staging_path" -type f -delete 2>/dev/null || true
    find "$staging_path" -depth -type d -empty -delete 2>/dev/null || true
  fi
  [[ ! -L "$next_current" ]] || rm -f -- "$next_current"
  [[ ! -L "$next_previous" ]] || rm -f -- "$next_previous"
  [[ ! -L "$restore_current_temp" ]] || rm -f -- "$restore_current_temp"
  [[ ! -L "$restore_previous_temp" ]] || rm -f -- "$restore_previous_temp"
  for next_managed_link in "${next_managed_links[@]}"; do
    [[ ! -L "$next_managed_link" ]] || rm -f -- "$next_managed_link"
  done
  [[ ! -f "$verify_unit" ]] || rm -f -- "$verify_unit"
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

validate_release_tree() {
  local path="$1"
  local ldd_output

  [[ -d "$path" && ! -L "$path" ]] || die "release tree is unsafe"
  [[ "$(stat -Lc '%u' "$path")" -eq "$(id -u)" ]] || die "release tree is not owned by the current user"
  (
    cd "$path"
    sha256sum -c SHA256SUMS
  )
  for binary in sigil sigil-host sigil-probe; do
    [[ -f "$path/$binary" && ! -L "$path/$binary" && -x "$path/$binary" ]] \
      || die "$binary is not a regular executable"
    mode="$(stat -Lc '%a' "$path/$binary")"
    (( (8#$mode & 8#022) == 0 )) || die "$binary is group/world writable"
  done
  cmp -s "$path/sigil" "$path/sigil-host" \
    || die "sigil-host compatibility executable differs from sigil"
  ldd_output="$(ldd "$path/sigil")"
  printf '%s\n' "$ldd_output"
  grep -q 'not found' <<<"$ldd_output" && die "host binary has an unresolved shared library"
  timeout --signal=TERM --kill-after=2s 5s "$path/sigil" --version
  timeout --signal=TERM --kill-after=2s 5s "$path/sigil-probe" --help >/dev/null
  awk -v executable="$path/sigil" '
    /^ExecStart=/ {
      print "ExecStart=" executable " serve --config=%h/.config/sigil-spark/host.toml"
      next
    }
    { print }
  ' "$path/assets/sigil-host.service" >"$verify_unit"
  chmod 0600 "$verify_unit"
  systemd-analyze --user verify "$verify_unit"
  rm -f -- "$verify_unit"
}

if [[ -e "$release_path" || -L "$release_path" ]]; then
  [[ -d "$release_path" && ! -L "$release_path" ]] || die "existing release path is unsafe"
  validate_release_tree "$release_path"
else
  install -d -m 0755 "$staging_path"
  (
    cd "$payload_dir/release"
    while IFS= read -r relative; do
      [[ -n "$relative" && "$relative" != /* && "$relative" != *..* ]] \
        || die "release checksum contains an unsafe path"
      install -D -m "$(stat -c '%a' "$relative")" "$relative" "$staging_path/$relative"
    done < <(awk '{print $2}' SHA256SUMS)
    install -m 0644 SHA256SUMS "$staging_path/SHA256SUMS"
  )
  validate_release_tree "$staging_path"
  mv -- "$staging_path" "$release_path"
fi

current_release_id=""
if [[ -e "$current_link" || -L "$current_link" ]]; then
  [[ -L "$current_link" ]] || die "current activation is not a symlink"
  current_path="$(readlink -f "$current_link")"
  [[ -n "$current_path" && -d "$current_path" ]] || die "current activation is dangling"
  # Fedora Atomic/Bazzite exposes /home through /var/home. Compare canonical
  # paths on both sides so a managed activation remains valid regardless of
  # which equivalent spelling systemd or the login session used for $HOME.
  [[ "$(dirname -- "$current_path")" == "$(readlink -f "$releases_root")" ]] \
    || die "current activation escapes the release root"
  current_release_id="$(basename -- "$current_path")"
fi
if [[ -e "$previous_link" || -L "$previous_link" ]]; then
  [[ -L "$previous_link" ]] || die "previous activation is not a symlink"
fi
snapshot_activation_links
activation_reload_service=true

config_root="$HOME/.config"
pipewire_dir="$config_root/pipewire/pipewire-pulse.conf.d"
systemd_dir="$config_root/systemd/user"
local_bin="$HOME/.local/bin"
asset_dir="$HOME/.local/share/sigil-spark/package-assets"
install -d -m 0700 "$config_root" "$pipewire_dir" "$systemd_dir" "$local_bin" "$asset_dir"

# BEGIN managed asset helpers (exercised directly by bazzite-package-assets.sh)
preflight_managed_link() {
  local link="$1"
  local target="$2"
  local release_asset="$3"

  if [[ -L "$link" ]]; then
    [[ "$(readlink "$link")" == "$target" ]] \
      || die "refusing to replace unmanaged symlink: $link"
    managed_link_action=keep
  elif [[ -e "$link" ]]; then
    [[ -f "$link" && -O "$link" ]] \
      || die "refusing to replace unsafe or unowned path: $link"
    cmp -s "$link" "$release_asset" \
      || die "refusing to replace modified path: $link"
    managed_link_action=replace
  else
    managed_link_action=create
  fi
}

install_managed_links() {
  local -a actions=()
  local -a prepared_links=()
  local index link next_link

  [[ "${#managed_links[@]}" -eq 4 \
    && "${#managed_links[@]}" -eq "${#managed_targets[@]}" \
    && "${#managed_links[@]}" -eq "${#managed_release_assets[@]}" ]] \
    || die "internal managed asset list mismatch"

  # Classify every destination before changing any of them. This prevents a
  # late unmanaged path from leaving an earlier package asset half-migrated.
  for index in "${!managed_links[@]}"; do
    preflight_managed_link \
      "${managed_links[$index]}" \
      "${managed_targets[$index]}" \
      "${managed_release_assets[$index]}"
    actions+=("$managed_link_action")
  done

  next_managed_links=()
  for index in "${!managed_links[@]}"; do
    [[ "${actions[$index]}" == keep ]] && continue
    link="${managed_links[$index]}"
    next_link="${link}.sigil-package-$release_id-$$-$index"
    [[ ! -e "$next_link" && ! -L "$next_link" ]] \
      || die "temporary managed link path already exists: $next_link"
    prepared_links[index]="$next_link"
    next_managed_links+=("$next_link")
  done

  for index in "${!managed_links[@]}"; do
    [[ "${actions[$index]}" == keep ]] && continue
    next_link="${prepared_links[$index]}"
    ln -s "${managed_targets[$index]}" "$next_link"
    if mv --help 2>&1 | grep -q -- '--no-target-directory'; then
      mv -Tf -- "$next_link" "${managed_links[$index]}"
    else
      mv -f "$next_link" "${managed_links[$index]}"
    fi
  done
}
# END managed asset helpers

managed_links=(
  "$pipewire_dir/50-sigil-spark-audio.conf"
  "$systemd_dir/sigil-host.service"
  "$local_bin/sigil-spark-host-rollback"
  "$asset_dir/70-sigil-remote-input.rules"
)
managed_targets=(
  "$install_root/current/assets/50-sigil-spark-audio.conf"
  "$install_root/current/assets/sigil-host.service"
  "$install_root/current/tools/rollback-bazzite-release.sh"
  "$install_root/current/assets/70-sigil-remote-input.rules"
)
managed_release_assets=(
  "$release_path/assets/50-sigil-spark-audio.conf"
  "$release_path/assets/sigil-host.service"
  "$release_path/tools/rollback-bazzite-release.sh"
  "$release_path/assets/70-sigil-remote-input.rules"
)
install_managed_links

ln -s "releases/$release_id" "$next_current"
if [[ -n "$current_release_id" && "$current_release_id" != "$release_id" ]]; then
  ln -s "releases/$current_release_id" "$next_previous"
fi
activate_release_links

printf 'package_release=%s\n' "$release_id"
printf 'current=%s\n' "$(readlink -f "$current_link")"
if [[ -L "$previous_link" ]]; then
  printf 'previous=%s\n' "$(readlink -f "$previous_link")"
else
  printf 'previous=none\n'
fi
printf 'package_install=release-and-user-assets-activated\n'
printf 'pipewire_restart=not-attempted\n'
printf 'service_activation=not-attempted\n'
printf 'system_asset=%s\n' "$asset_dir/70-sigil-remote-input.rules"
printf '%s\n' \
  'next: complete the explicit identity, hardware config, uinput-group/udev, PipeWire restart, and service-enable gates from docs/fresh-bazzite-host.md'
