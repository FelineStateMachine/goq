#!/usr/bin/env bash

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/../.." && pwd -P)"
guide="$repo_dir/docs/sigil-host-activation.md"
installer="$repo_dir/scripts/install-bazzite-package.sh"
bootstrap="$repo_dir/website/install-sigil"
temp_parent="${TMPDIR:-/tmp}"
temp_parent="${temp_parent%/}"
temp_root="$(mktemp -d "$temp_parent/sigil-activation-guide.XXXXXX")"

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  case "$temp_root" in
    "$temp_parent"/sigil-activation-guide.??????) rm -rf -- "$temp_root" ;;
  esac
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

[[ -f "$guide" && ! -L "$guide" ]] || fail 'the activation guide is missing or unsafe'

awk '
  /^```bash$/ { inside = 1; next }
  /^```$/ && inside { inside = 0; print ""; next }
  inside { print }
' "$guide" >"$temp_root/guide-code.sh"
bash -n "$temp_root/guide-code.sh"

if grep -Eq 'tank@|/Users/dami|Developer/sigil-spark|scripts/' "$guide"; then
  fail 'the product activation guide contains a maintainer or source-checkout path'
fi
if grep -Eq '/dev/dri/renderD128|vaapi_encoder = "vah264enc"' "$guide"; then
  fail 'the activation guide pins the original test host GPU contract'
fi

grep -Fq 'refusing to replace existing host configuration' "$guide" \
  || fail 'the activation guide can overwrite an existing host configuration'
grep -Fq 'set -o noclobber' "$guide" \
  || fail 'the activation guide does not create host.toml exclusively'
grep -Fq -- '--kill-after=2s 5s' "$guide" \
  || fail 'the activation guide does not time-bound GStreamer inspection'
grep -Fq 'inspect_max_bytes=1048576' "$guide" \
  || fail 'the activation guide does not size-bound GStreamer inspection'
grep -Fq 'current/assets/72-sigil-uinput.rules' "$guide" \
  || fail 'the activation guide does not use its packaged early uinput rule'
grep -Fq 'current/assets/99-sigil-uinput.rules' "$guide" \
  || fail 'the activation guide does not use its packaged final uinput rule'
grep -Fq 'native Gamescope' "$guide" \
  || fail 'the activation guide does not preserve dynamic native resolution'

for output_script in "$installer" "$bootstrap"; do
  grep -Fq 'current/docs/sigil-host-activation.md' "$output_script" \
    || fail "$(basename "$output_script") does not print the persistent guide path"
  if grep -Fq 'fresh-bazzite-host.md' "$output_script"; then
    fail "$(basename "$output_script") still delegates activation to the lab runbook"
  fi
done

printf 'sigil_host_activation_guide_tests=ok\n'
