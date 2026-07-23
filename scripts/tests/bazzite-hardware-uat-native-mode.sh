#!/usr/bin/env bash

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
runner="$script_dir/../run-bazzite-hardware-uat.sh"

export SIGIL_HARDWARE_UAT_SOURCE_ONLY=1
# shellcheck source=../run-bazzite-hardware-uat.sh
# shellcheck disable=SC1091
source "$runner"

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

native_mode_relation=stale
classify_native_uat_mode 1280x800 1280x800
[[ "$native_mode_relation" == identical-to-fixed ]] \
  || fail 'a native 1280x800 panel was not accepted as identical to the fixed leg'

classify_native_uat_mode 2560x1600 1280x800
[[ "$native_mode_relation" == distinct-from-fixed ]] \
  || fail 'a larger native panel was not classified as distinct from the fixed leg'

for invalid_native in '' 1280 0x800 1280x0 -1x800; do
  native_mode_relation=stale
  if classify_native_uat_mode "$invalid_native" 1280x800; then
    fail "an invalid native mode was accepted: $invalid_native"
  fi
  [[ -z "$native_mode_relation" ]] \
    || fail 'failed native classification retained stale relation state'
done

native_mode_relation=stale
if classify_native_uat_mode 1280x800 invalid; then
  fail 'an invalid fixed mode was accepted'
fi
[[ -z "$native_mode_relation" ]] \
  || fail 'failed fixed classification retained stale relation state'

grep -Fq "classify_native_uat_mode \"\$native_size\" 1280x800" "$runner" \
  || fail 'the resolved native mode does not use the tested classifier'
grep -Fq "echo \"native_mode_relation=\$native_mode_relation\"" "$runner" \
  || fail 'the evidence summary omits the native mode relation'
if grep -Fq 'native UAT resolved to the fixed 1280x800 mode' "$runner"; then
  fail 'the runner still rejects a native mode identical to the fixed leg'
fi
grep -Fq "run_capture_probe native \"\$native_config\" \"\$native_size\"" "$runner" \
  || fail 'the native capture leg was collapsed into the fixed leg'
grep -Fq "start_candidate native \"\$native_config\"" "$runner" \
  || fail 'the native daemon leg was collapsed into the fixed leg'
grep -Fq "run_probe_cycle native iroh-moq \"\$native_size\"" "$runner" \
  || fail 'the native session leg was collapsed into the fixed leg'

printf 'bazzite_hardware_uat_native_mode_tests=ok\n'
