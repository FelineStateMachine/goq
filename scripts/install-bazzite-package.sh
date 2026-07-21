#!/usr/bin/env bash

set -euo pipefail
umask 077

usage() {
  cat <<'EOF'
Usage: install-bazzite-package.sh [--payload-dir PATH]

Verify and install one Sigil Spark Bazzite host runtime package. The release is
fully staged and validated before the current symlink changes. Package-owned
user assets follow current, so upgrades and rollbacks keep binaries, service,
and PipeWire configuration on the same release.

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
for command in diff flock ldd sha256sum systemctl systemd-analyze timeout; do
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

install -d -m 0755 "$HOME/.local" "$HOME/.local/libexec" "$install_root" "$releases_root"
exec 9>"$lock_path"
flock -x 9

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  if [[ -d "$staging_path" ]]; then
    find "$staging_path" -type f -delete 2>/dev/null || true
    find "$staging_path" -depth -type d -empty -delete 2>/dev/null || true
  fi
  [[ ! -L "$next_current" ]] || rm -f -- "$next_current"
  [[ ! -L "$next_previous" ]] || rm -f -- "$next_previous"
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
  for binary in sigil-host sigil-probe; do
    [[ -f "$path/$binary" && ! -L "$path/$binary" && -x "$path/$binary" ]] \
      || die "$binary is not a regular executable"
    mode="$(stat -Lc '%a' "$path/$binary")"
    (( (8#$mode & 8#022) == 0 )) || die "$binary is group/world writable"
  done
  ldd_output="$(ldd "$path/sigil-host")"
  printf '%s\n' "$ldd_output"
  grep -q 'not found' <<<"$ldd_output" && die "host binary has an unresolved shared library"
  timeout --signal=TERM --kill-after=2s 5s "$path/sigil-host" --version
  timeout --signal=TERM --kill-after=2s 5s "$path/sigil-probe" --help >/dev/null
  awk -v executable="$path/sigil-host" '
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

config_root="$HOME/.config"
pipewire_dir="$config_root/pipewire/pipewire-pulse.conf.d"
systemd_dir="$config_root/systemd/user"
local_bin="$HOME/.local/bin"
asset_dir="$HOME/.local/share/sigil-spark/package-assets"
install -d -m 0700 "$config_root" "$pipewire_dir" "$systemd_dir" "$local_bin" "$asset_dir"

ensure_managed_link() {
  local link="$1"
  local target="$2"
  if [[ -e "$link" || -L "$link" ]]; then
    [[ -L "$link" && "$(readlink -- "$link")" == "$target" ]] \
      || die "refusing to replace unmanaged path: $link"
    return
  fi
  ln -s "$target" "$link"
}

ensure_managed_link "$pipewire_dir/50-sigil-spark-audio.conf" \
  "$install_root/current/assets/50-sigil-spark-audio.conf"
ensure_managed_link "$systemd_dir/sigil-host.service" \
  "$install_root/current/assets/sigil-host.service"
ensure_managed_link "$local_bin/sigil-spark-host-rollback" \
  "$install_root/current/tools/rollback-bazzite-release.sh"
ensure_managed_link "$asset_dir/70-sigil-remote-input.rules" \
  "$install_root/current/assets/70-sigil-remote-input.rules"

ln -s "releases/$release_id" "$next_current"
if [[ -n "$current_release_id" && "$current_release_id" != "$release_id" ]]; then
  ln -s "releases/$current_release_id" "$next_previous"
  mv -Tf -- "$next_previous" "$previous_link"
fi
mv -Tf -- "$next_current" "$current_link"

systemctl --user daemon-reload

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
