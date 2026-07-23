#!/usr/bin/env bash

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/../.." && pwd -P)"
verifier="$repo_dir/scripts/verify-sigil-release.sh"
temp_parent="${TMPDIR:-/tmp}"
temp_parent="${temp_parent%/}"
temp_root="$(mktemp -d "$temp_parent/sigil-release-verifier.XXXXXX")"
release_tag=v0.1.0
source_commit=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  case "$temp_root" in
    "$temp_parent"/sigil-release-verifier.??????) rm -rf -- "$temp_root" ;;
  esac
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

make_fixture() {
  local output_dir="$1"
  local variant="${2:-valid}"
  install -d -m 0700 "$output_dir"
  python3 - "$output_dir" "$release_tag" "$source_commit" "$variant" <<'PY'
import gzip
import hashlib
import io
import json
import pathlib
import sys
import tarfile

root = pathlib.Path(sys.argv[1])
tag = sys.argv[2]
commit = sys.argv[3]
variant = sys.argv[4]
asset = f"sigil-{tag}-bazzite-x86_64.tar.gz"
archive_path = root / asset

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
if variant == "default-features":
    manifest["features"] = ["default"]
binary = b"fixture executable\n"
release = {
    "sigil": binary,
    "sigil-host": binary,
    "sigil-probe": b"fixture probe\n",
    "assets/50-sigil-spark-audio.conf": b"audio\n",
    "assets/70-sigil-remote-input.rules": b"udev\n",
    "assets/72-sigil-uinput.rules": b"early uinput\n",
    "assets/99-sigil-uinput.rules": b"final uinput\n",
    "assets/sigil-host.service": b"[Service]\nExecStart=/fixture\n",
    "docs/sigil-host-activation.md": b"# Activation fixture\n",
    "tools/rollback-bazzite-release.sh": b"#!/usr/bin/env bash\nexit 0\n",
    "LICENSE": b"MIT\n",
    "release-manifest.json": (json.dumps(manifest, sort_keys=True, indent=2) + "\n").encode(),
}

def sums(files):
    return "".join(f"{hashlib.sha256(data).hexdigest()}  {name}\n" for name, data in files.items()).encode()

release_sums = sums(release)
release_id = hashlib.sha256(release_sums).hexdigest().encode() + b"\n"
payload = {
    "release-id": release_id,
    "install-bazzite-package.sh": b"#!/usr/bin/env bash\nexit 0\n",
    "stage-this-release.sh": b"#!/usr/bin/env bash\nexit 0\n",
    "release/SHA256SUMS": release_sums,
}
package_sums = sums(payload)
files = {"payload/PACKAGE-SHA256SUMS": package_sums}
files.update({f"payload/{name}": data for name, data in payload.items()})
files.update({f"payload/release/{name}": data for name, data in release.items()})
dirs = [
    "payload/release",
    "payload/release/assets",
    "payload/release/docs",
    "payload/release/tools",
]
if variant == "unexpected":
    files["payload/unexpected"] = b"unexpected\n"

with archive_path.open("wb") as raw:
    with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
            for name in dirs:
                info = tarfile.TarInfo(name)
                info.type = tarfile.DIRTYPE
                info.mode = 0o700
                info.uid = info.gid = info.mtime = 0
                archive.addfile(info)
            for name in sorted(files):
                data = files[name]
                info = tarfile.TarInfo(name)
                info.mode = 0o755 if name.endswith((".sh", "/sigil", "/sigil-host", "/sigil-probe")) else 0o644
                info.uid = info.gid = info.mtime = 0
                if variant == "symlink" and name == "payload/release/sigil":
                    info.type = tarfile.SYMTYPE
                    info.linkname = "sigil-host"
                    archive.addfile(info)
                else:
                    info.size = len(data)
                    archive.addfile(info, io.BytesIO(data))
                if variant == "duplicate" and name == "payload/release/sigil-host":
                    archive.addfile(info, io.BytesIO(data))

digest = hashlib.sha256(archive_path.read_bytes()).hexdigest()
(root / f"{asset}.sha256").write_text(f"{digest}  {asset}\n")
PY
}

assert_rejected() {
  local name="$1"
  local expected="$2"
  shift 2
  local log="$temp_root/$name.log"
  if "$@" >"$log" 2>&1; then
    printf 'FAIL: %s unexpectedly succeeded\n' "$name" >&2
    exit 1
  fi
  grep -Fq "$expected" "$log" || {
    printf 'FAIL: %s did not report %s\n' "$name" "$expected" >&2
    sed -n '1,120p' "$log" >&2
    exit 1
  }
}

valid_dir="$temp_root/valid"
make_fixture "$valid_dir"
archive="$valid_dir/sigil-$release_tag-bazzite-x86_64.tar.gz"
"$verifier" --tag "$release_tag" --archive "$archive" \
  --source-commit "$source_commit" --candidate | grep -Fq 'sigil_release_verification=ok'

printf 'not a signature\n' >"$archive.minisig"
key_file="$temp_root/test.pub"
printf '%s\n%s\n' 'untrusted comment: test key' \
  'RWAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA' >"$key_file"
fake_bin="$temp_root/bin"
install -d -m 0700 "$fake_bin"
printf '%s\n' '#!/usr/bin/env bash' 'exit 0' >"$fake_bin/minisign"
chmod 0755 "$fake_bin/minisign"
PATH="$fake_bin:$PATH" "$verifier" --tag "$release_tag" --archive "$archive" \
  --source-commit "$source_commit" --public-key-file "$key_file" \
  | grep -Fq 'publisher_verification=published'

assert_rejected wrong-commit 'commit does not match' \
  env PATH="$fake_bin:$PATH" "$verifier" --tag "$release_tag" --archive "$archive" \
    --source-commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb --public-key-file "$key_file"

bad_checksum_dir="$temp_root/bad-checksum"
make_fixture "$bad_checksum_dir"
bad_archive="$bad_checksum_dir/sigil-$release_tag-bazzite-x86_64.tar.gz"
printf '%064d  %s\n' 0 "$(basename -- "$bad_archive")" >"$bad_archive.sha256"
assert_rejected bad-checksum 'checksum declaration does not exactly match' \
  "$verifier" --tag "$release_tag" --archive "$bad_archive" --candidate

for variant in symlink duplicate unexpected default-features; do
  fixture_dir="$temp_root/$variant"
  make_fixture "$fixture_dir" "$variant"
  fixture_archive="$fixture_dir/sigil-$release_tag-bazzite-x86_64.tar.gz"
  case "$variant" in
    symlink) expected='unsafe type' ;;
    duplicate) expected='duplicate members' ;;
    unexpected) expected='do not match the package allowlist' ;;
    default-features) expected='field features does not match the product contract' ;;
  esac
  assert_rejected "$variant" "$expected" \
    "$verifier" --tag "$release_tag" --archive "$fixture_archive" --candidate
done

unconfigured="$temp_root/unconfigured.pub"
printf 'unconfigured\n' >"$unconfigured"
assert_rejected unconfigured-key 'unconfigured or malformed' \
  env PATH="$fake_bin:$PATH" "$verifier" --tag "$release_tag" --archive "$archive" \
    --public-key-file "$unconfigured"

printf 'sigil_release_verifier_tests=ok\n'
