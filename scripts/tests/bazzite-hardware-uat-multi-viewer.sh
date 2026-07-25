#!/usr/bin/env bash

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
runner="$script_dir/../run-bazzite-hardware-uat.sh"

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

grep -Fq -- '--multi-viewer-evidence FILE' "$runner" \
  || fail 'hardware runner does not expose bounded multi-viewer evidence ingress'
grep -Fq 'validate_multi_viewer_evidence' "$runner" \
  || fail 'hardware runner does not validate multi-viewer evidence'
for contract in \
  'concurrent_viewers)" -eq 3' \
  'focus_handoff_p95_ms' \
  'slow_viewer_isolation' \
  'same_peer_replacement' \
  'live_view_revocation' \
  'survivors_uninterrupted' \
  'ordinary_service_restored'
do
  grep -Fq "$contract" "$runner" \
    || fail "hardware runner omits multi-viewer contract: $contract"
done
# The runner must emit this literal expansion site.
# shellcheck disable=SC2016
grep -Fq 'multi_viewer_uat=$multi_viewer_status' "$runner" \
  || fail 'hardware summary does not distinguish absent from passing multi-viewer evidence'

printf 'bazzite_hardware_uat_multi_viewer_tests=ok\n'
