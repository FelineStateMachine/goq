#!/usr/bin/env bash

set -euo pipefail
umask 077

usage() {
  cat <<'EOF'
Usage: scripts/package-macos-client.sh --release-tag vVERSION
       --output-dir /absolute/path

Build and verify a distributable macOS DMG. This gate requires:

  - a clean worktree;
  - a release tag resolving exactly to HEAD and matching every package version;
  - an Apple Silicon runner and the aarch64-apple-darwin Rust target;
  - APPLE_SIGNING_IDENTITY naming a Developer ID Application identity;
  - Tauri notarization credentials (App Store Connect API or Apple ID);
  - strict code-signing, hardened runtime, Gatekeeper, and stapling checks.

The normal release is built without optional Cargo features. Development
features, including demo-direct-node and
experimental-non-macos-pointer-capture, and ad-hoc signatures are
intentionally not accepted by this command.
EOF
}

die() {
  printf 'macOS package failed: %s\n' "$*" >&2
  exit 1
}

script_dir="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/.." && pwd -P)"
output_dir=""
release_tag=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output-dir)
      [[ $# -ge 2 ]] || die "--output-dir requires a path"
      output_dir="$2"
      shift 2
      ;;
    --release-tag)
      [[ $# -ge 2 ]] || die "--release-tag requires a tag"
      release_tag="$2"
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
[[ "$(uname -m)" == arm64 ]] || die "the first public Portal release is macOS arm64 only"
[[ -n "$output_dir" && "$output_dir" == /* ]] || die "--output-dir must be absolute"
[[ -n "$release_tag" ]] || die "--release-tag is required"
[[ -d "$output_dir" ]] || die "output directory does not exist"
output_dir="$(CDPATH='' cd -- "$output_dir" && pwd -P)"
case "$output_dir/" in
  "$repo_dir/"*) die "--output-dir must be outside the source repository" ;;
esac
if find "$output_dir" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
  die "output directory must be empty"
fi

for command in cargo codesign find git grep hdiutil lipo plutil python3 rustup security sed shasum spctl xcrun; do
  command -v "$command" >/dev/null 2>&1 || die "$command is required"
done

verifier="$script_dir/verify-portal-release.py"
[[ -f "$verifier" ]] || die "Portal release verifier is missing"
python3 "$verifier" source --repo-dir "$repo_dir" --release-tag "$release_tag" >/dev/null
rustup target list --installed | grep -Fxq aarch64-apple-darwin \
  || die "the aarch64-apple-darwin Rust target is required"

signing_identity="${APPLE_SIGNING_IDENTITY:-}"
[[ "$signing_identity" == Developer\ ID\ Application:* ]] \
  || die "APPLE_SIGNING_IDENTITY must name a Developer ID Application identity"
security find-identity -v -p codesigning | grep -Fq "\"$signing_identity\"" \
  || die "APPLE_SIGNING_IDENTITY is not available in the keychain"
expected_team_id_file="$repo_dir/release/portal-apple-team-id.txt"
[[ -f "$expected_team_id_file" && ! -L "$expected_team_id_file" ]] \
  || die "committed Portal Apple team identifier is missing or unsafe"
expected_team_id="$(<"$expected_team_id_file")"
[[ "$expected_team_id" =~ ^[A-Z0-9]{10}$ ]] \
  || die "release/portal-apple-team-id.txt must contain one Apple TeamIdentifier"
[[ "${APPLE_TEAM_ID:-}" == "$expected_team_id" ]] \
  || die "APPLE_TEAM_ID does not match the committed Portal signer"

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
  cargo tauri build --locked --ci --target aarch64-apple-darwin --bundles app,dmg
)

bundle_root="$repo_dir/target/aarch64-apple-darwin/release/bundle"
app_path="$bundle_root/macos/Portal.app"
[[ -d "$app_path" ]] || die "Tauri app bundle is missing"
dmg_count="$(find "$bundle_root/dmg" -maxdepth 1 -type f -name '*.dmg' -print | wc -l | tr -d ' ')"
[[ "$dmg_count" -eq 1 ]] || die "expected exactly one DMG artifact"
dmg_path="$(find "$bundle_root/dmg" -maxdepth 1 -type f -name '*.dmg' -print)"

hdiutil verify "$dmg_path"
codesign --verify --deep --strict --verbose=2 "$app_path"
signature_details="$(codesign -d --verbose=4 "$app_path" 2>&1)"
grep -Fq "Authority=$signing_identity" <<<"$signature_details" \
  || die "application is not signed by the configured Developer ID identity"
team_identifier="$(sed -n 's/^TeamIdentifier=//p' <<<"$signature_details")"
[[ "$team_identifier" == "$expected_team_id" ]] \
  || die "application TeamIdentifier does not match the committed Portal signer"
grep -Eq '^flags=.*runtime' <<<"$signature_details" \
  || die "application signature does not enable hardened runtime"
spctl --assess --type execute --verbose=4 "$app_path"
xcrun stapler validate "$app_path"
xcrun stapler validate "$dmg_path"
spctl --assess --type open --context context:primary-signature --verbose=4 "$dmg_path"

executable="$app_path/Contents/MacOS/portal"
[[ -x "$executable" ]] || die "client executable is missing"
architectures="$(lipo -archs "$executable")"
[[ "$architectures" == arm64 ]] || die "client executable must contain only arm64"
identifier="$(plutil -extract CFBundleIdentifier raw "$app_path/Contents/Info.plist")"
[[ "$identifier" == sh.goq.portal ]] || die "unexpected bundle identifier: $identifier"
if find "$app_path" -type f \( -name sigil -o -name sigil-host -o -name sigil-probe -o -name host.toml -o -name '*.key' \) \
  | grep -q .
then
  die "client bundle contains a forbidden host or credential artifact"
fi

version="$(plutil -extract CFBundleShortVersionString raw "$app_path/Contents/Info.plist")"
[[ "$release_tag" == "v$version" ]] || die "app version does not match release tag"
git_commit="$(git -C "$repo_dir" rev-parse --verify HEAD)"
artifact_name="Portal-${version}-arm64.dmg"
artifact_path="$output_dir/$artifact_name"
manifest_path="$output_dir/Portal-${version}-arm64.json"
[[ ! -e "$artifact_path" && ! -e "$manifest_path" && ! -e "$artifact_path.sha256" ]] \
  || die "release output already exists"
install -m 0644 "$dmg_path" "$artifact_path"
artifact_sha256="$(shasum -a 256 "$artifact_path" | awk '{print $1}')"
printf '%s  %s\n' "$artifact_sha256" "$artifact_name" >"$artifact_path.sha256"
python3 - "$manifest_path" "$release_tag" "$version" "$git_commit" "$artifact_name" "$artifact_sha256" <<'PY'
import json
import pathlib
import sys

path, release_tag, version, git_commit, asset, digest = sys.argv[1:]
manifest = {
    "architecture": "arm64",
    "architectures": ["arm64"],
    "asset": asset,
    "bundle_identifier": "sh.goq.portal",
    "checksum_asset": f"{asset}.sha256",
    "demo_direct_node": False,
    "developer_id_signed": True,
    "format": 1,
    "gatekeeper_verified": True,
    "git_commit": git_commit,
    "hardened_runtime": True,
    "notarized": True,
    "platform": "macos",
    "product": "portal-client",
    "release_tag": release_tag,
    "sha256": digest,
    "stapled": True,
    "version": version,
}
pathlib.Path(path).write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
PY
chmod 0644 "$manifest_path"
python3 "$verifier" assets \
  --repo-dir "$repo_dir" \
  --release-tag "$release_tag" \
  --asset-dir "$output_dir" >/dev/null

printf 'client_package=%s\n' "$artifact_path"
printf 'client_package_sha256=%s\n' "$artifact_sha256"
printf 'client_architectures=%s\n' "$architectures"
printf 'client_release_tag=%s\n' "$release_tag"
printf 'client_git_commit=%s\n' "$git_commit"
printf 'client_signing=developer-id-hardened-runtime\n'
printf 'client_notarization=stapled-and-validated\n'
