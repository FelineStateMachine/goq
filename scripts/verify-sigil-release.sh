#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/verify-sigil-release.sh \
  --tag vVERSION --archive /absolute/path/sigil-vVERSION-bazzite-x86_64.tar.gz \
  [--source-commit HEX] [--candidate | --public-key-file PATH]

Verify the exact Sigil Bazzite release asset contract without extracting it.
Candidate mode verifies a clean, unsigned product candidate before it crosses
the offline signing boundary. Published mode additionally requires and verifies
ARCHIVE.minisig with the reviewed Minisign public key file.
EOF
}

die() {
  printf 'release verification failed: %s\n' "$*" >&2
  exit 1
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

release_tag=""
archive_path=""
source_commit=""
public_key_file=""
candidate=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tag)
      [[ $# -ge 2 ]] || die "--tag requires a value"
      release_tag="$2"
      shift 2
      ;;
    --archive)
      [[ $# -ge 2 ]] || die "--archive requires a path"
      archive_path="$2"
      shift 2
      ;;
    --source-commit)
      [[ $# -ge 2 ]] || die "--source-commit requires a value"
      source_commit="$2"
      shift 2
      ;;
    --public-key-file)
      [[ $# -ge 2 ]] || die "--public-key-file requires a path"
      public_key_file="$2"
      shift 2
      ;;
    --candidate)
      candidate=true
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ "$release_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z][0-9A-Za-z.-]*)?$ ]] \
  || die "--tag must be vVERSION"
[[ -z "$source_commit" || "$source_commit" =~ ^[0-9a-f]{40}$ ]] \
  || die "--source-commit must be a 40-character lowercase commit"
[[ "$archive_path" == /* ]] || die "--archive must be absolute"
[[ -f "$archive_path" && ! -L "$archive_path" ]] || die "archive is missing or unsafe"

asset_name="sigil-$release_tag-bazzite-x86_64.tar.gz"
[[ "$(basename -- "$archive_path")" == "$asset_name" ]] \
  || die "archive must be named $asset_name"
checksum_path="$archive_path.sha256"
signature_path="$archive_path.minisig"
[[ -f "$checksum_path" && ! -L "$checksum_path" ]] || die "checksum asset is missing or unsafe"

for command_name in awk python3 wc; do
  command -v "$command_name" >/dev/null 2>&1 || die "$command_name is required"
done
if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
  die "sha256sum or shasum is required"
fi

archive_bytes="$(wc -c <"$archive_path" | tr -d ' ')"
[[ "$archive_bytes" =~ ^[0-9]+$ && "$archive_bytes" -gt 0 \
  && "$archive_bytes" -le 268435456 ]] || die "archive size is outside the 256 MiB release limit"
[[ "$(wc -l <"$checksum_path" | tr -d ' ')" == 1 ]] \
  || die "checksum asset must contain exactly one line"
actual_sha256="$(sha256_file "$archive_path")"
checksum_declaration="$(<"$checksum_path")"
[[ "$checksum_declaration" == "$actual_sha256  $asset_name" ]] \
  || die "checksum declaration does not exactly match the archive"

if $candidate; then
  [[ -z "$public_key_file" ]] || die "--candidate and --public-key-file are mutually exclusive"
  [[ ! -e "$signature_path" ]] || die "unsigned candidate directory already contains a signature"
  verification_mode="candidate"
else
  [[ -n "$public_key_file" ]] || die "published verification requires --public-key-file"
  [[ "$public_key_file" == /* && -f "$public_key_file" && ! -L "$public_key_file" ]] \
    || die "public key file is missing or unsafe"
  [[ -f "$signature_path" && ! -L "$signature_path" ]] || die "detached signature is missing or unsafe"
  command -v minisign >/dev/null 2>&1 || die "minisign is required for published verification"
  publisher_key="$(awk '/^RW[A-Za-z0-9+\/=]+$/ { key=$0 } END { print key }' "$public_key_file")"
  [[ "$publisher_key" =~ ^RW[A-Za-z0-9+/=]{40,}$ ]] \
    || die "public key file is unconfigured or malformed"
  minisign -Vm "$archive_path" -x "$signature_path" -P "$publisher_key" >/dev/null \
    || die "detached publisher signature is invalid"
  verification_mode="published"
fi

python3 - "$archive_path" "$release_tag" "$asset_name" "$source_commit" <<'PY'
import hashlib
import json
import pathlib
import re
import sys
import tarfile

archive_path = pathlib.Path(sys.argv[1])
release_tag = sys.argv[2]
asset_name = sys.argv[3]
source_commit = sys.argv[4]

expected_dirs = {
    "payload/release",
    "payload/release/assets",
    "payload/release/tools",
}
release_files = {
    "LICENSE",
    "SHA256SUMS",
    "assets/50-sigil-spark-audio.conf",
    "assets/70-sigil-remote-input.rules",
    "assets/sigil-host.service",
    "release-manifest.json",
    "sigil",
    "sigil-host",
    "sigil-probe",
    "tools/rollback-bazzite-release.sh",
}
expected_files = {
    "payload/PACKAGE-SHA256SUMS",
    "payload/install-bazzite-package.sh",
    "payload/release-id",
    "payload/stage-this-release.sh",
    *{f"payload/release/{name}" for name in release_files},
}

def fail(message: str) -> None:
    raise SystemExit(f"release verification failed: {message}")

def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()

def parse_sums(data: bytes, expected: set[str], label: str) -> dict[str, str]:
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        fail(f"{label} is not UTF-8")
    lines = text.splitlines()
    if not lines or not text.endswith("\n"):
        fail(f"{label} is empty or lacks its final newline")
    parsed: dict[str, str] = {}
    for line in lines:
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9._/-]+)", line)
        if not match:
            fail(f"{label} contains a malformed line")
        sha, name = match.groups()
        if name in parsed:
            fail(f"{label} contains duplicate path {name}")
        parsed[name] = sha
    if set(parsed) != expected:
        fail(f"{label} paths do not match the release allowlist")
    return parsed

try:
    archive = tarfile.open(archive_path, "r:gz")
except (tarfile.TarError, OSError) as error:
    fail(f"archive is unreadable: {error}")

with archive:
    members = archive.getmembers()
    names = [member.name.rstrip("/") for member in members]
    if len(names) != len(set(names)):
        fail("archive contains duplicate members")
    if set(names) != expected_dirs | expected_files:
        fail("archive members do not match the package allowlist")
    files: dict[str, bytes] = {}
    for member, name in zip(members, names, strict=True):
        path = pathlib.PurePosixPath(name)
        if path.is_absolute() or not path.parts or any(part in {"", ".", ".."} for part in path.parts):
            fail(f"archive contains unsafe path {name!r}")
        if member.uid != 0 or member.gid != 0 or member.mtime != 0:
            fail(f"archive member metadata is not deterministic: {name}")
        if name in expected_dirs:
            if not member.isdir():
                fail(f"archive directory has unsafe type: {name}")
            continue
        if not member.isreg():
            fail(f"archive file has unsafe type: {name}")
        if member.size > 134217728:
            fail(f"archive member exceeds 128 MiB: {name}")
        stream = archive.extractfile(member)
        if stream is None:
            fail(f"archive member cannot be read: {name}")
        files[name] = stream.read()

package_expected = {
    "release-id",
    "install-bazzite-package.sh",
    "stage-this-release.sh",
    "release/SHA256SUMS",
}
package_sums = parse_sums(files["payload/PACKAGE-SHA256SUMS"], package_expected, "PACKAGE-SHA256SUMS")
for name, sha in package_sums.items():
    if digest(files[f"payload/{name}"]) != sha:
        fail(f"package checksum mismatch for {name}")

release_expected = release_files - {"SHA256SUMS"}
release_sums = parse_sums(files["payload/release/SHA256SUMS"], release_expected, "release SHA256SUMS")
for name, sha in release_sums.items():
    if digest(files[f"payload/release/{name}"]) != sha:
        fail(f"release checksum mismatch for {name}")
release_id = files["payload/release-id"].decode("ascii", errors="strict")
if release_id != digest(files["payload/release/SHA256SUMS"]) + "\n":
    fail("release-id does not bind the installed-file checksums")
if files["payload/release/sigil"] != files["payload/release/sigil-host"]:
    fail("sigil-host compatibility executable differs from sigil")

try:
    manifest = json.loads(files["payload/release/release-manifest.json"])
except (UnicodeDecodeError, json.JSONDecodeError) as error:
    fail(f"release manifest is invalid: {error}")
expected_values = {
    "format": 2,
    "product": "sigil-host",
    "primary_executable": "sigil",
    "compatibility_executable": "sigil-host",
    "version": release_tag[1:],
    "target": "x86_64-unknown-linux-gnu.2.17",
    "profile": "release",
    "features": ["default"],
    "demo_direct_node": False,
    "git_dirty": False,
    "binary_provenance": "self-built-clean-head",
    "binary_provenance_verified": True,
    "release_tag": release_tag,
    "asset_name": asset_name,
    "release_kind": "product-candidate",
}
for key, expected in expected_values.items():
    if manifest.get(key) != expected:
        fail(f"release manifest field {key} does not match the product contract")
commit = manifest.get("git_commit")
if not isinstance(commit, str) or re.fullmatch(r"[0-9a-f]{40}", commit) is None:
    fail("release manifest git_commit is malformed")
if source_commit and commit != source_commit:
    fail("release manifest commit does not match the release tag")
for field in ("cargo_lock_sha256", "rust_toolchain_sha256"):
    value = manifest.get(field)
    if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{64}", value) is None:
        fail(f"release manifest field {field} is malformed")
zigbuild = manifest.get("cargo_zigbuild")
if not isinstance(zigbuild, str) or not zigbuild.startswith("cargo-zigbuild "):
    fail("release manifest lacks the cargo-zigbuild version")

print(f"release_commit={commit}")
print(f"release_id={release_id.strip()}")
PY

printf 'release_tag=%s\n' "$release_tag"
printf 'asset_name=%s\n' "$asset_name"
printf 'archive_sha256=%s\n' "$actual_sha256"
printf 'publisher_verification=%s\n' "$verification_mode"
printf 'sigil_release_verification=ok\n'
