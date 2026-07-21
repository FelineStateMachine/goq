#!/usr/bin/env bash

set -euo pipefail
umask 077

usage() {
  cat <<'EOF'
Usage: scripts/package-macos-client.sh --output-dir /absolute/path
       [--expected-arch arm64|x86_64]

Build and verify a distributable macOS DMG. This gate requires:

  - a clean worktree;
  - APPLE_SIGNING_IDENTITY naming a Developer ID Application identity;
  - Tauri notarization credentials (App Store Connect API or Apple ID);
  - strict code-signing, hardened runtime, Gatekeeper, and stapling checks.

The normal release is built without the demo-direct-node feature. Development
or ad-hoc signatures are intentionally not accepted by this command.
EOF
}

die() {
  printf 'macOS package failed: %s\n' "$*" >&2
  exit 1
}

script_dir="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/.." && pwd -P)"
output_dir=""
expected_arch="$(uname -m)"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output-dir)
      [[ $# -ge 2 ]] || die "--output-dir requires a path"
      output_dir="$2"
      shift 2
      ;;
    --expected-arch)
      [[ $# -ge 2 ]] || die "--expected-arch requires an architecture"
      expected_arch="$2"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ "$(uname -s)" == Darwin ]] || die "this package must be built on macOS"
[[ -n "$output_dir" && "$output_dir" == /* ]] || die "--output-dir must be absolute"
[[ "$expected_arch" == arm64 || "$expected_arch" == x86_64 ]] || die "unsupported expected architecture"
[[ -d "$output_dir" ]] || die "output directory does not exist"
[[ -z "$(git -C "$repo_dir" status --porcelain=v1)" ]] || die "worktree must be clean"

for command in cargo codesign hdiutil lipo plutil python3 security shasum spctl xcrun; do
  command -v "$command" >/dev/null 2>&1 || die "$command is required"
done

signing_identity="${APPLE_SIGNING_IDENTITY:-}"
[[ "$signing_identity" == Developer\ ID\ Application:* ]] \
  || die "APPLE_SIGNING_IDENTITY must name a Developer ID Application identity"
security find-identity -v -p codesigning | grep -Fq "\"$signing_identity\"" \
  || die "APPLE_SIGNING_IDENTITY is not available in the keychain"

api_credentials=false
apple_id_credentials=false
if [[ -n "${APPLE_API_ISSUER:-}" && -n "${APPLE_API_KEY:-}" && -n "${APPLE_API_KEY_PATH:-}" ]]; then
  [[ "$APPLE_API_KEY_PATH" == /* && -f "$APPLE_API_KEY_PATH" ]] \
    || die "APPLE_API_KEY_PATH must name an absolute private-key file"
  api_credentials=true
fi
if [[ -n "${APPLE_ID:-}" && -n "${APPLE_PASSWORD:-}" && -n "${APPLE_TEAM_ID:-}" ]]; then
  apple_id_credentials=true
fi
$api_credentials || $apple_id_credentials \
  || die "set complete App Store Connect API or Apple ID notarization credentials"

# shellcheck source=/dev/null
source "$HOME/.cargo/env"
(
  cd "$repo_dir"
  cargo tauri build --locked --bundles app,dmg
)

bundle_root="$repo_dir/target/release/bundle"
app_path="$bundle_root/macos/Sigil Spark.app"
[[ -d "$app_path" ]] || die "Tauri app bundle is missing"
dmg_count="$(find "$bundle_root/dmg" -maxdepth 1 -type f -name '*.dmg' -print | wc -l | tr -d ' ')"
[[ "$dmg_count" -eq 1 ]] || die "expected exactly one DMG artifact"
dmg_path="$(find "$bundle_root/dmg" -maxdepth 1 -type f -name '*.dmg' -print)"

hdiutil verify "$dmg_path"
codesign --verify --deep --strict --verbose=2 "$app_path"
signature_details="$(codesign -d --verbose=4 "$app_path" 2>&1)"
grep -Fq 'Authority=Developer ID Application:' <<<"$signature_details" \
  || die "application is not signed by Developer ID Application"
grep -Eq '^TeamIdentifier=[A-Z0-9]+$' <<<"$signature_details" \
  || die "application signature has no TeamIdentifier"
grep -Eq '^flags=.*runtime' <<<"$signature_details" \
  || die "application signature does not enable hardened runtime"
spctl --assess --type execute --verbose=4 "$app_path"
xcrun stapler validate "$app_path"
xcrun stapler validate "$dmg_path"
spctl --assess --type open --context context:primary-signature --verbose=4 "$dmg_path"

executable="$app_path/Contents/MacOS/sigil-spark"
[[ -x "$executable" ]] || die "client executable is missing"
architectures="$(lipo -archs "$executable")"
grep -Eq "(^| )$expected_arch( |$)" <<<"$architectures" \
  || die "client executable does not contain $expected_arch"
identifier="$(plutil -extract CFBundleIdentifier raw "$app_path/Contents/Info.plist")"
[[ "$identifier" == com.sigil.spark ]] || die "unexpected bundle identifier: $identifier"
if find "$app_path" -type f \( -name sigil-host -o -name sigil-probe -o -name host.toml -o -name '*.key' \) \
  | grep -q .
then
  die "client bundle contains a forbidden host or credential artifact"
fi

version="$(plutil -extract CFBundleShortVersionString raw "$app_path/Contents/Info.plist")"
artifact_name="Sigil-Spark-${version}-${expected_arch}.dmg"
artifact_path="$output_dir/$artifact_name"
manifest_path="$output_dir/Sigil-Spark-${version}-${expected_arch}.json"
[[ ! -e "$artifact_path" && ! -e "$manifest_path" && ! -e "$artifact_path.sha256" ]] \
  || die "release output already exists"
install -m 0644 "$dmg_path" "$artifact_path"
artifact_sha256="$(shasum -a 256 "$artifact_path" | awk '{print $1}')"
printf '%s  %s\n' "$artifact_sha256" "$artifact_name" >"$artifact_path.sha256"
python3 - "$manifest_path" "$version" "$expected_arch" "$architectures" "$artifact_sha256" <<'PY'
import json
import pathlib
import sys

path, version, expected_arch, architectures, digest = sys.argv[1:]
manifest = {
    "format": 1,
    "product": "sigil-spark-client",
    "version": version,
    "bundle_identifier": "com.sigil.spark",
    "expected_arch": expected_arch,
    "architectures": architectures.split(),
    "sha256": digest,
    "developer_id_signed": True,
    "hardened_runtime": True,
    "notarized_and_stapled": True,
    "demo_direct_node": False,
}
pathlib.Path(path).write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
PY
chmod 0644 "$manifest_path"

printf 'client_package=%s\n' "$artifact_path"
printf 'client_package_sha256=%s\n' "$artifact_sha256"
printf 'client_architectures=%s\n' "$architectures"
printf 'client_signing=developer-id-hardened-runtime\n'
printf 'client_notarization=stapled-and-validated\n'
