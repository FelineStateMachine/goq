#!/usr/bin/env bash

set -euo pipefail

die() {
  printf 'Portal signature verification failed: %s\n' "$*" >&2
  exit 1
}

usage() {
  printf 'Usage: %s --dmg /absolute/path/Portal-VERSION-arm64.dmg --expected-team-id TEAMID [--expected-version VERSION]\n' "$0"
}

dmg=''
expected_version=''
expected_team_id=''
while (($#)); do
  case "$1" in
    --dmg) [[ $# -ge 2 ]] || die "$1 requires a value"; dmg="$2"; shift 2 ;;
    --expected-version) [[ $# -ge 2 ]] || die "$1 requires a value"; expected_version="$2"; shift 2 ;;
    --expected-team-id) [[ $# -ge 2 ]] || die "$1 requires a value"; expected_team_id="$2"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ "$(uname -s)" == Darwin ]] || die 'verification must run on macOS'
[[ "$dmg" == /* && -f "$dmg" && ! -L "$dmg" ]] || die 'DMG must be an absolute regular file'
[[ "$expected_team_id" =~ ^[A-Z0-9]{10}$ ]] || die 'expected Apple TeamIdentifier is required'
for command_name in codesign find grep hdiutil lipo plutil sed spctl xcrun; do
  command -v "$command_name" >/dev/null 2>&1 || die "$command_name is required"
done

temporary_base="${TMPDIR:-/tmp}"
verification_root="$(mktemp -d "${temporary_base%/}/goq-portal-verify.XXXXXX")"
mount_path="$verification_root/mount"
mkdir -m 0700 "$mount_path"
mounted=false
cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  if $mounted; then
    hdiutil detach "$mount_path" -force >/dev/null 2>&1 || true
  fi
  rmdir "$mount_path" "$verification_root" >/dev/null 2>&1 || true
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

hdiutil verify "$dmg" >/dev/null
xcrun stapler validate "$dmg"
spctl --assess --type open --context context:primary-signature --verbose=4 "$dmg"
hdiutil attach "$dmg" -readonly -nobrowse -mountpoint "$mount_path" >/dev/null
mounted=true

app_count="$(find "$mount_path" -maxdepth 2 -type d -name 'Portal.app' -print | wc -l | tr -d ' ')"
[[ "$app_count" == 1 ]] || die 'DMG must contain exactly one Portal.app'
app_path="$(find "$mount_path" -maxdepth 2 -type d -name 'Portal.app' -print)"
codesign --verify --deep --strict --verbose=4 "$app_path"
signature_details="$(codesign -d --verbose=4 "$app_path" 2>&1)"
grep -Eq '^Authority=Developer ID Application:' <<<"$signature_details" \
  || die 'application is not signed by a Developer ID Application identity'
team_identifier="$(sed -n 's/^TeamIdentifier=//p' <<<"$signature_details")"
[[ "$team_identifier" == "$expected_team_id" ]] \
  || die 'application TeamIdentifier does not match the pinned Portal signer'
grep -Eq '^flags=.*runtime' <<<"$signature_details" \
  || die 'application signature does not enable hardened runtime'
spctl --assess --type execute --verbose=4 "$app_path"
xcrun stapler validate "$app_path"

identifier="$(plutil -extract CFBundleIdentifier raw "$app_path/Contents/Info.plist")"
[[ "$identifier" == sh.goq.portal ]] || die "unexpected bundle identifier: $identifier"
version="$(plutil -extract CFBundleShortVersionString raw "$app_path/Contents/Info.plist")"
[[ -z "$expected_version" || "$version" == "$expected_version" ]] \
  || die "app version $version does not match $expected_version"
executable="$app_path/Contents/MacOS/portal"
[[ -x "$executable" ]] || die 'Portal executable is missing'
[[ "$(lipo -archs "$executable")" == arm64 ]] || die 'Portal executable is not arm64-only'
if find "$app_path" -type f \( -name sigil -o -name sigil-host -o -name sigil-probe \
  -o -name host.toml -o -name '*.key' \) | grep -q .
then
  die 'Portal contains a forbidden host or credential artifact'
fi

printf 'portal_signature_verification=ok\nportal_version=%s\nportal_architecture=arm64\n' "$version"
