#!/usr/bin/env bash

set -euo pipefail

if [[ "$(uname -s)" != Linux ]]; then
  printf 'bazzite_package_install_e2e=skipped (Linux required)\n'
  exit 0
fi
if [[ "$(id -u)" -eq 0 ]]; then
  printf 'bazzite_package_install_e2e=skipped (non-root user required)\n'
  exit 0
fi

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/../.." && pwd -P)"
installer="$repo_dir/scripts/install-bazzite-package.sh"
rollback="$repo_dir/scripts/rollback-bazzite-release.sh"
temp_parent="${TMPDIR:-/tmp}"
temp_parent="${temp_parent%/}"
temp_root="$(mktemp -d "$temp_parent/sigil-package-e2e.XXXXXX")"

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  case "$temp_root" in
    "$temp_parent"/sigil-package-e2e.??????) rm -rf -- "$temp_root" ;;
  esac
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

make_payload() {
  local root="$1"
  local tag="$2"
  local commit="$3"
  local asset_name="sigil-$tag-bazzite-x86_64.tar.gz"
  local release="$root/release"
  local true_binary
  true_binary="$(readlink -f /usr/bin/true 2>/dev/null || readlink -f /bin/true)"
  [[ -x "$true_binary" ]]

  install -d -m 0700 \
    "$root" "$release/assets" "$release/docs" "$release/tools"
  install -m 0755 "$true_binary" "$release/sigil"
  install -m 0755 "$true_binary" "$release/sigil-host"
  install -m 0755 "$true_binary" "$release/sigil-probe"
  install -m 0755 "$rollback" "$release/tools/rollback-bazzite-release.sh"
  printf '[Service]\nExecStart=/fixture serve\n' >"$release/assets/sigil-host.service"
  printf 'audio fixture\n' >"$release/assets/50-sigil-spark-audio.conf"
  printf 'udev fixture\n' >"$release/assets/70-sigil-remote-input.rules"
  printf 'early uinput fixture\n' >"$release/assets/72-sigil-uinput.rules"
  printf 'final uinput fixture\n' >"$release/assets/99-sigil-uinput.rules"
  printf '# Activation fixture %s\n' "$tag" \
    >"$release/docs/sigil-host-activation.md"
  printf 'MIT\n' >"$release/LICENSE"
  chmod 0600 "$release/assets/50-sigil-spark-audio.conf"
  chmod 0644 \
    "$release/assets/70-sigil-remote-input.rules" \
    "$release/assets/72-sigil-uinput.rules" \
    "$release/assets/99-sigil-uinput.rules" \
    "$release/assets/sigil-host.service" \
    "$release/docs/sigil-host-activation.md" \
    "$release/LICENSE"

  python3 - "$release/release-manifest.json" "$tag" "$commit" "$asset_name" <<'PY'
import json
import pathlib
import sys

path, tag, commit, asset = sys.argv[1:]
manifest = {
    "format": 2,
    "product": "sigil-host",
    "primary_executable": "sigil",
    "compatibility_executable": "sigil-host",
    "version": tag[1:],
    "target": "x86_64-unknown-linux-gnu.2.17",
    "profile": "release",
    "features": ["default", "in-process-gstreamer"],
    "demo_direct_node": False,
    "git_commit": commit,
    "git_dirty": False,
    "cargo_lock_sha256": "b" * 64,
    "rust_toolchain_sha256": "c" * 64,
    "cargo_zigbuild": "cargo-zigbuild 0.23.0",
    "binary_provenance": "self-built-clean-head",
    "binary_provenance_verified": True,
    "release_tag": tag,
    "asset_name": asset,
    "release_kind": "product-candidate",
}
pathlib.Path(path).write_text(json.dumps(manifest, sort_keys=True, indent=2) + "\n")
PY
  chmod 0644 "$release/release-manifest.json"

  : >"$release/SHA256SUMS"
  for relative in \
    sigil sigil-host sigil-probe \
    assets/50-sigil-spark-audio.conf \
    assets/70-sigil-remote-input.rules \
    assets/72-sigil-uinput.rules \
    assets/99-sigil-uinput.rules \
    assets/sigil-host.service \
    docs/sigil-host-activation.md \
    tools/rollback-bazzite-release.sh \
    LICENSE release-manifest.json
  do
    printf '%s  %s\n' "$(sha256sum "$release/$relative" | awk '{print $1}')" \
      "$relative" >>"$release/SHA256SUMS"
  done
  chmod 0644 "$release/SHA256SUMS"
  sha256sum "$release/SHA256SUMS" | awk '{print $1}' >"$root/release-id"
  install -m 0755 "$installer" "$root/install-bazzite-package.sh"
  # Intentional literals form the package-owned wrapper fixture.
  # shellcheck disable=SC2016
  printf '%s\n' \
    '#!/usr/bin/env bash' \
    'set -euo pipefail' \
    'script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"' \
    'exec "$script_dir/install-bazzite-package.sh" --payload-dir "$script_dir" "$@"' \
    >"$root/stage-this-release.sh"
  chmod 0755 "$root/stage-this-release.sh"
  : >"$root/PACKAGE-SHA256SUMS"
  for relative in release-id install-bazzite-package.sh stage-this-release.sh release/SHA256SUMS; do
    printf '%s  %s\n' "$(sha256sum "$root/$relative" | awk '{print $1}')" \
      "$relative" >>"$root/PACKAGE-SHA256SUMS"
  done
  chmod 0644 "$root/PACKAGE-SHA256SUMS"
}

payload_a="$temp_root/payload-a"
payload_b="$temp_root/payload-b"
make_payload "$payload_a" v0.1.0 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
make_payload "$payload_b" v0.1.1 bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
release_a="$(<"$payload_a/release-id")"
release_b="$(<"$payload_b/release-id")"
[[ "$release_a" != "$release_b" ]]

fake_bin="$temp_root/bin"
install -d -m 0700 "$fake_bin"
systemctl_log="$temp_root/systemctl.log"
# Intentional literals form the isolated systemctl recording shim.
# shellcheck disable=SC2016
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'printf "%s\n" "$*" >>"$SIGIL_TEST_SYSTEMCTL_LOG"' \
  'exit 0' >"$fake_bin/systemctl"
printf '%s\n' '#!/usr/bin/env bash' 'exit 0' >"$fake_bin/systemd-analyze"
chmod 0755 "$fake_bin/systemctl" "$fake_bin/systemd-analyze"

test_home="$temp_root/home"
install -d -m 0700 "$test_home"
run_installer() {
  env HOME="$test_home" PATH="$fake_bin:$PATH" \
    SIGIL_TEST_SYSTEMCTL_LOG="$systemctl_log" \
    "$installer" --payload-dir "$1"
}

run_installer "$payload_a" >"$temp_root/install-a.log"
install_root="$test_home/.local/libexec/sigil-spark"
[[ "$(basename -- "$(readlink -f "$install_root/current")")" == "$release_a" ]]
[[ ! -e "$install_root/previous" && ! -L "$install_root/previous" ]]
grep -Fqx '# Activation fixture v0.1.0' \
  "$install_root/current/docs/sigil-host-activation.md"
grep -Fq "activation_guide=$install_root/current/docs/sigil-host-activation.md" \
  "$temp_root/install-a.log"
if grep -Fq 'fresh-bazzite-host.md' "$temp_root/install-a.log"; then
  printf 'FAIL: package installer delegated activation to the lab runbook\n' >&2
  exit 1
fi

run_installer "$payload_a" >"$temp_root/repeat-a.log"
[[ "$(basename -- "$(readlink -f "$install_root/current")")" == "$release_a" ]]
[[ ! -e "$install_root/previous" && ! -L "$install_root/previous" ]]

identity="$test_home/.local/share/sigil-spark/identity/host.key"
config="$test_home/.config/sigil-spark/host.toml"
install -d -m 0700 "$(dirname -- "$identity")" "$(dirname -- "$config")"
printf 'identity fixture\n' >"$identity"
printf 'config fixture\n' >"$config"
chmod 0600 "$identity" "$config"
identity_sha="$(sha256sum "$identity" | awk '{print $1}')"
config_sha="$(sha256sum "$config" | awk '{print $1}')"

run_installer "$payload_b" >"$temp_root/install-b.log"
[[ "$(basename -- "$(readlink -f "$install_root/current")")" == "$release_b" ]]
[[ "$(basename -- "$(readlink -f "$install_root/previous")")" == "$release_a" ]]
grep -Fqx '# Activation fixture v0.1.1' \
  "$install_root/current/docs/sigil-host-activation.md"
[[ "$(sha256sum "$identity" | awk '{print $1}')" == "$identity_sha" ]]
[[ "$(sha256sum "$config" | awk '{print $1}')" == "$config_sha" ]]

env HOME="$test_home" PATH="$fake_bin:$PATH" \
  SIGIL_TEST_SYSTEMCTL_LOG="$systemctl_log" \
  "$test_home/.local/bin/sigil-spark-host-rollback" >"$temp_root/rollback.log"
[[ "$(basename -- "$(readlink -f "$install_root/current")")" == "$release_a" ]]
[[ "$(basename -- "$(readlink -f "$install_root/previous")")" == "$release_b" ]]
[[ "$(sha256sum "$identity" | awk '{print $1}')" == "$identity_sha" ]]
[[ "$(sha256sum "$config" | awk '{print $1}')" == "$config_sha" ]]

if grep -Eq '(^|[[:space:]])(start|restart|enable)([[:space:]]|$)' "$systemctl_log"; then
  printf 'FAIL: installer or rollback implicitly changed service activation\n' >&2
  sed -n '1,120p' "$systemctl_log" >&2
  exit 1
fi
grep -Fq -- '--user daemon-reload' "$systemctl_log"

hostile_payload="$temp_root/hostile-payload"
cp -R "$payload_a" "$hostile_payload"
printf 'unexpected\n' >"$hostile_payload/unexpected"
hostile_home="$temp_root/hostile-home"
install -d -m 0700 "$hostile_home"
if env HOME="$hostile_home" PATH="$fake_bin:$PATH" \
  SIGIL_TEST_SYSTEMCTL_LOG="$systemctl_log" \
  "$installer" --payload-dir "$hostile_payload" >"$temp_root/hostile.log" 2>&1
then
  printf 'FAIL: unexpected payload file was accepted\n' >&2
  exit 1
fi
grep -Fq 'does not match the runtime allowlist' "$temp_root/hostile.log"
[[ ! -e "$hostile_home/.local/libexec/sigil-spark" ]]

unsafe_home="$temp_root/unsafe-home"
install -d -m 0700 "$unsafe_home" "$temp_root/outside-local"
ln -s "$temp_root/outside-local" "$unsafe_home/.local"
if env HOME="$unsafe_home" PATH="$fake_bin:$PATH" \
  SIGIL_TEST_SYSTEMCTL_LOG="$systemctl_log" \
  "$installer" --payload-dir "$payload_a" >"$temp_root/unsafe-home.log" 2>&1
then
  printf 'FAIL: symlink install root was accepted\n' >&2
  exit 1
fi
grep -Fq 'unsafe install directory' "$temp_root/unsafe-home.log"

printf 'bazzite_package_install_e2e=ok\n'
