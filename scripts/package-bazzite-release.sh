#!/usr/bin/env bash

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/.." && pwd -P)"
host_binary=""
probe_binary=""
output_path=""
allow_dirty=false
allow_unsigned=false
minisign_secret_key=""
linux_target="x86_64-unknown-linux-gnu.2.17"
linux_output_target="x86_64-unknown-linux-gnu"

usage() {
  cat <<'EOF'
Usage: scripts/package-bazzite-release.sh --output /absolute/path/package.tar.gz [options]

Create an allowlisted, deterministic Bazzite host runtime package. Product mode
exports the clean source HEAD and builds both generic Linux release binaries in
an isolated Cargo target directory. The package includes atomic install,
upgrade/rollback support, the systemd user unit, PipeWire sink, staged udev
rule, complete checksums, and build provenance. It never includes the source
tree, host identity, hardware configuration, environment files, or evidence.

Options:
  --output PATH          New .tar.gz bundle path (required; must not exist)
  --host-binary PATH     Development-only prebuilt generic Linux sigil
  --probe-binary PATH    Development-only prebuilt generic Linux sigil-probe
  --minisign-key PATH    Create detached PACKAGE.minisig with this secret key
  --allow-dirty          Permit development packaging from a dirty worktree
  --allow-unsigned       Permit a package without a detached publisher signature
  --help                 Show this help

The two prebuilt-binary options are an all-or-none development escape hatch and
require both --allow-dirty and --allow-unsigned. Product packages never accept
caller-supplied binaries.
EOF
}

die() {
  printf 'package failed: %s\n' "$*" >&2
  exit 1
}

sha256_file() {
  local path="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{print $1}'
  else
    shasum -a 256 "$path" | awk '{print $1}'
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output)
      [[ $# -ge 2 ]] || die "--output requires a value"
      output_path="$2"
      shift 2
      ;;
    --host-binary)
      [[ $# -ge 2 ]] || die "--host-binary requires a value"
      host_binary="$2"
      shift 2
      ;;
    --probe-binary)
      [[ $# -ge 2 ]] || die "--probe-binary requires a value"
      probe_binary="$2"
      shift 2
      ;;
    --minisign-key)
      [[ $# -ge 2 ]] || die "--minisign-key requires a path"
      minisign_secret_key="$2"
      shift 2
      ;;
    --allow-dirty)
      allow_dirty=true
      shift
      ;;
    --allow-unsigned)
      allow_unsigned=true
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ -n "$output_path" ]] || die "--output is required"
case "$output_path" in
  /*.tar.gz) ;;
  *) die "--output must be an absolute .tar.gz path" ;;
esac
[[ ! -e "$output_path" ]] || die "output already exists: $output_path"
[[ ! -e "$output_path.sha256" ]] || die "digest output already exists: $output_path.sha256"
[[ ! -e "$output_path.minisig" ]] || die "signature output already exists: $output_path.minisig"

caller_supplied_binaries=false
if [[ -n "$host_binary" || -n "$probe_binary" ]]; then
  [[ -n "$host_binary" && -n "$probe_binary" ]] \
    || die "--host-binary and --probe-binary must be supplied together"
  if ! $allow_dirty || ! $allow_unsigned; then
    die "caller-supplied binaries require both --allow-dirty and --allow-unsigned; product mode builds both binaries from clean HEAD"
  fi
  caller_supplied_binaries=true
fi

[[ -x "$script_dir/install-bazzite-package.sh" ]] || die "package installer is missing"
[[ -x "$script_dir/rollback-bazzite-release.sh" ]] || die "rollback helper is missing"
[[ -f "$script_dir/sigil-host.service" ]] || die "systemd user unit is missing"
[[ -f "$script_dir/50-sigil-spark-audio.conf" ]] || die "PipeWire audio drop-in is missing"
[[ -f "$script_dir/70-sigil-remote-input.rules" ]] || die "udev rule is missing"
command -v git >/dev/null 2>&1 || die "git is required"
command -v python3 >/dev/null 2>&1 || die "python3 is required for deterministic archives"
command -v tar >/dev/null 2>&1 || die "tar is required"

git_commit="$(git -C "$repo_dir" rev-parse --verify HEAD)"
[[ "$git_commit" =~ ^[0-9a-f]{40}$ ]] || die "could not resolve the source commit"
git_dirty=false
if [[ -n "$(git -C "$repo_dir" status --porcelain=v1)" ]]; then
  git_dirty=true
  $allow_dirty || die "worktree is dirty; commit the release or pass --allow-dirty for a development package"
fi
if [[ -z "$minisign_secret_key" ]]; then
  $allow_unsigned || die "publisher signature required; pass --minisign-key or explicit --allow-unsigned for development"
else
  [[ "$minisign_secret_key" == /* && -f "$minisign_secret_key" && ! -L "$minisign_secret_key" ]] \
    || die "minisign secret key must be an absolute regular file"
  command -v minisign >/dev/null 2>&1 || die "minisign is required for --minisign-key"
fi

output_parent="$(dirname -- "$output_path")"
[[ -d "$output_parent" ]] || die "output parent does not exist: $output_parent"
temp_root="$(mktemp -d "$output_parent/.sigil-bazzite-package.XXXXXX")"
case "$temp_root" in
  "$output_parent"/.sigil-bazzite-package.??????) ;;
  *) die "mktemp returned an unexpected path: $temp_root" ;;
esac

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  case "$temp_root" in
    "$output_parent"/.sigil-bazzite-package.??????) rm -rf -- "$temp_root" ;;
  esac
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

if $caller_supplied_binaries; then
  [[ -f "$host_binary" && -x "$host_binary" ]] \
    || die "host binary is not executable: $host_binary"
  [[ -f "$probe_binary" && -x "$probe_binary" ]] \
    || die "probe binary is not executable: $probe_binary"
  binary_provenance="caller-supplied-unverified"
  binary_provenance_verified=false
else
  command -v cargo >/dev/null 2>&1 || die "cargo is required to build product binaries"
  command -v cargo-zigbuild >/dev/null 2>&1 \
    || die "cargo-zigbuild is required to build product binaries"

  build_source="$temp_root/source"
  build_target="$temp_root/cargo-target"
  install -d -m 0700 "$build_source" "$build_target"
  git -C "$repo_dir" archive --format=tar "$git_commit" | tar -xf - -C "$build_source"
  (
    cd "$build_source"
    CARGO_TARGET_DIR="$build_target" cargo zigbuild --locked -p sigil-host --bins \
      --target "$linux_target" --release
  )
  host_binary="$build_target/$linux_output_target/release/sigil"
  probe_binary="$build_target/$linux_output_target/release/sigil-probe"
  [[ -f "$host_binary" && -x "$host_binary" ]] \
    || die "product build did not produce sigil"
  [[ -f "$probe_binary" && -x "$probe_binary" ]] \
    || die "product build did not produce sigil-probe"
  binary_provenance="self-built-clean-head"
  binary_provenance_verified=true
fi

payload="$temp_root/payload"
release_tree="$payload/release"
install -d -m 0700 "$payload" "$release_tree" "$release_tree/assets" "$release_tree/tools"
install -m 0755 "$host_binary" "$release_tree/sigil"
# Retain one byte-identical compatibility executable for existing automation.
install -m 0755 "$host_binary" "$release_tree/sigil-host"
install -m 0755 "$probe_binary" "$release_tree/sigil-probe"
install -m 0755 "$script_dir/rollback-bazzite-release.sh" \
  "$release_tree/tools/rollback-bazzite-release.sh"
install -m 0644 "$script_dir/sigil-host.service" "$release_tree/assets/sigil-host.service"
install -m 0600 "$script_dir/50-sigil-spark-audio.conf" \
  "$release_tree/assets/50-sigil-spark-audio.conf"
install -m 0644 "$script_dir/70-sigil-remote-input.rules" \
  "$release_tree/assets/70-sigil-remote-input.rules"
install -m 0644 "$repo_dir/LICENSE" "$release_tree/LICENSE"
install -m 0755 "$script_dir/install-bazzite-package.sh" "$payload/install-bazzite-package.sh"

product_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo_dir/src-tauri/Cargo.toml" | head -n 1)"
[[ -n "$product_version" ]] || die "could not read the product version"
lock_sha256="$(sha256_file "$repo_dir/Cargo.lock")"
toolchain_sha256="$(sha256_file "$repo_dir/rust-toolchain.toml")"
zigbuild_version="$(cargo-zigbuild --version 2>/dev/null | head -n 1 || true)"
[[ -n "$zigbuild_version" ]] || zigbuild_version="unavailable"
python3 - "$release_tree/release-manifest.json" "$product_version" "$git_commit" \
  "$git_dirty" "$lock_sha256" "$toolchain_sha256" "$zigbuild_version" \
  "$binary_provenance" "$binary_provenance_verified" "$linux_target" <<'PY'
import json
import pathlib
import sys

(
    path,
    version,
    commit,
    dirty,
    lock_sha,
    toolchain_sha,
    zigbuild,
    binary_provenance,
    binary_provenance_verified,
    target,
) = sys.argv[1:]
manifest = {
    "format": 2,
    "product": "sigil-host",
    "primary_executable": "sigil",
    "compatibility_executable": "sigil-host",
    "version": version,
    "target": target,
    "profile": "release",
    "features": ["default"],
    "demo_direct_node": False,
    "git_commit": commit,
    "git_dirty": dirty == "true",
    "cargo_lock_sha256": lock_sha,
    "rust_toolchain_sha256": toolchain_sha,
    "cargo_zigbuild": zigbuild,
    "binary_provenance": binary_provenance,
    "binary_provenance_verified": binary_provenance_verified == "true",
}
pathlib.Path(path).write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
PY
chmod 0644 "$release_tree/release-manifest.json"

release_sums="$release_tree/SHA256SUMS"
: >"$release_sums"
for relative in \
  sigil \
  sigil-host \
  sigil-probe \
  assets/50-sigil-spark-audio.conf \
  assets/70-sigil-remote-input.rules \
  assets/sigil-host.service \
  tools/rollback-bazzite-release.sh \
  LICENSE \
  release-manifest.json
do
  printf '%s  %s\n' "$(sha256_file "$release_tree/$relative")" "$relative" >>"$release_sums"
done
chmod 0644 "$release_sums"
release_id="$(sha256_file "$release_sums")"
printf '%s\n' "$release_id" >"$payload/release-id"

printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  "script_dir=\"\$(cd -- \"\$(dirname -- \"\${BASH_SOURCE[0]}\")\" && pwd -P)\"" \
  "exec \"\$script_dir/install-bazzite-package.sh\" --payload-dir \"\$script_dir\" \"\$@\"" \
  >"$payload/stage-this-release.sh"
chmod 0755 "$payload/stage-this-release.sh"

package_sums="$payload/PACKAGE-SHA256SUMS"
: >"$package_sums"
for relative in release-id install-bazzite-package.sh stage-this-release.sh release/SHA256SUMS
do
  printf '%s  %s\n' "$(sha256_file "$payload/$relative")" "$relative" >>"$package_sums"
done
chmod 0644 "$package_sums"

archive_tmp="$temp_root/package.tar.gz"
python3 - "$temp_root" "$archive_tmp" <<'PY'
import gzip
import pathlib
import sys
import tarfile

root = pathlib.Path(sys.argv[1])
output = pathlib.Path(sys.argv[2])
payload = root / "payload"
with output.open("wb") as raw:
    with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0, compresslevel=9) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
            for path in sorted(payload.rglob("*"), key=lambda item: item.as_posix()):
                if path.is_symlink():
                    raise SystemExit(f"refusing symlink in package: {path}")
                relative = path.relative_to(root).as_posix()
                info = archive.gettarinfo(str(path), arcname=relative)
                info.uid = 0
                info.gid = 0
                info.uname = "root"
                info.gname = "root"
                info.mtime = 0
                if path.is_file():
                    with path.open("rb") as stream:
                        archive.addfile(info, stream)
                else:
                    archive.addfile(info)
PY
tar -tzf "$archive_tmp" >/dev/null
mv -- "$archive_tmp" "$output_path"

package_sha256="$(sha256_file "$output_path")"
printf '%s  %s\n' "$package_sha256" "$(basename -- "$output_path")" >"$output_path.sha256"
if [[ -n "$minisign_secret_key" ]]; then
  minisign -S -s "$minisign_secret_key" -m "$output_path" -x "$output_path.minisig" \
    -t "Sigil host $product_version" \
    -c "release $release_id"
  signature_status="detached-minisign"
else
  signature_status="absent-development"
fi

printf 'package=%s\n' "$output_path"
printf 'package_sha256=%s\n' "$package_sha256"
printf 'release_id=%s\n' "$release_id"
printf 'git_commit=%s\n' "$git_commit"
printf 'git_dirty=%s\n' "$git_dirty"
printf 'publisher_signature=%s\n' "$signature_status"
