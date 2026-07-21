#!/usr/bin/env bash

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/../.." && pwd -P)"
package_script="$repo_dir/scripts/package-bazzite-release.sh"
temp_parent="${TMPDIR:-/tmp}"
temp_parent="${temp_parent%/}"
temp_root="$(mktemp -d "$temp_parent/sigil-package-provenance.XXXXXX")"

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  case "$temp_root" in
    "$temp_parent"/sigil-package-provenance.??????) rm -rf -- "$temp_root" ;;
  esac
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

host_binary="$temp_root/sigil"
probe_binary="$temp_root/sigil-probe"
printf '#!/usr/bin/env bash\nexit 0\n' >"$host_binary"
printf '#!/usr/bin/env bash\nexit 0\n' >"$probe_binary"
chmod 0755 "$host_binary" "$probe_binary"

assert_rejected() {
  local case_name="$1"
  local expected="$2"
  local output_path="$temp_root/$case_name.tar.gz"
  local log_path="$temp_root/$case_name.log"
  shift 2

  if "$package_script" --output "$output_path" "$@" >"$log_path" 2>&1; then
    printf 'FAIL: %s unexpectedly succeeded\n' "$case_name" >&2
    exit 1
  fi
  grep -Fq -- "$expected" "$log_path" || {
    printf 'FAIL: %s did not report %s\n' "$case_name" "$expected" >&2
    sed -n '1,120p' "$log_path" >&2
    exit 1
  }
  [[ ! -e "$output_path" && ! -e "$output_path.sha256" && ! -e "$output_path.minisig" ]] || {
    printf 'FAIL: %s created package output before rejecting supplied binaries\n' "$case_name" >&2
    exit 1
  }
}

both_flags_message='caller-supplied binaries require both --allow-dirty and --allow-unsigned'
assert_rejected product-supplied "$both_flags_message" \
  --host-binary "$host_binary" --probe-binary "$probe_binary"
assert_rejected dirty-only-supplied "$both_flags_message" \
  --allow-dirty --host-binary "$host_binary" --probe-binary "$probe_binary"
assert_rejected unsigned-only-supplied "$both_flags_message" \
  --allow-unsigned --host-binary "$host_binary" --probe-binary "$probe_binary"
assert_rejected partial-development-pair \
  '--host-binary and --probe-binary must be supplied together' \
  --allow-dirty --allow-unsigned --host-binary "$host_binary"

development_output="$temp_root/development-supplied.tar.gz"
development_log="$temp_root/development-supplied.log"
"$package_script" \
  --output "$development_output" \
  --allow-dirty --allow-unsigned \
  --host-binary "$host_binary" --probe-binary "$probe_binary" \
  >"$development_log"
grep -Fq 'publisher_signature=absent-development' "$development_log"
python3 - "$development_output" <<'PY'
import json
import sys
import tarfile

with tarfile.open(sys.argv[1], "r:gz") as archive:
    manifest_file = archive.extractfile("payload/release/release-manifest.json")
    if manifest_file is None:
        raise SystemExit("release manifest is missing")
    manifest = json.load(manifest_file)
    primary = archive.extractfile("payload/release/sigil")
    compatibility = archive.extractfile("payload/release/sigil-host")
    service = archive.extractfile("payload/release/assets/sigil-host.service")
    if primary is None or compatibility is None:
        raise SystemExit("primary or compatibility host executable is missing")
    if service is None:
        raise SystemExit("service asset is missing")
    if primary.read() != compatibility.read():
        raise SystemExit("sigil-host compatibility executable differs from sigil")
    service_text = service.read().decode("utf-8")
    if "current/sigil-host serve" not in service_text:
        raise SystemExit("service does not retain the compatibility executable")
    if "current/sigil serve" in service_text:
        raise SystemExit("service bypasses the legacy compatibility executable")

if manifest.get("binary_provenance") != "caller-supplied-unverified":
    raise SystemExit("caller-supplied binary provenance is not marked unverified")
if manifest.get("binary_provenance_verified") is not False:
    raise SystemExit("caller-supplied binary provenance must not be verified")
if manifest.get("primary_executable") != "sigil":
    raise SystemExit("primary executable is not sigil")
if manifest.get("compatibility_executable") != "sigil-host":
    raise SystemExit("compatibility executable is not sigil-host")
PY

printf 'package_bazzite_provenance_tests=ok\n'
