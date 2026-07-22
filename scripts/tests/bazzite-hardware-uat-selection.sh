#!/usr/bin/env bash

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
runner="$script_dir/../run-bazzite-hardware-uat.sh"
temp_parent="${TMPDIR:-/tmp}"
temp_parent="${temp_parent%/}"
temp_root="$(mktemp -d "$temp_parent/sigil-hardware-selection.XXXXXX")"

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  case "$temp_root" in
    "$temp_parent"/sigil-hardware-selection.??????) rm -rf -- "$temp_root" ;;
  esac
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

export SIGIL_HARDWARE_UAT_SOURCE_ONLY=1
# shellcheck source=../run-bazzite-hardware-uat.sh
# shellcheck disable=SC1091
source "$runner"
render_node=''
va_encoder=''
va_candidates=''

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

fixture="$temp_root/fixture"
sysfs_root="$fixture/sys/class/drm"
device_root="$fixture/dev/dri"
drivers="$fixture/drivers"
bin="$fixture/bin"
factory_fixture="$fixture/factories.tsv"
install -d -m 0700 \
  "$sysfs_root" "$device_root" "$drivers/amdgpu" "$drivers/i915" "$bin"

add_render_node() {
  local name="$1"
  local driver="$2"

  install -d -m 0700 "$sysfs_root/$name/device"
  ln -s "$drivers/$driver" "$sysfs_root/$name/device/driver"
  install -m 0600 /dev/null "$device_root/$name"
}

reset_render_nodes() {
  rm -rf -- "$sysfs_root"
  rm -rf -- "$device_root"
  install -d -m 0700 "$sysfs_root" "$device_root"
}

cat >"$bin/gst-inspect-1.0" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

fixture="${SIGIL_TEST_FACTORY_FIXTURE:?}"
if [[ $# -eq 0 ]]; then
  awk -F '\t' '{ printf "va:  %s: fixture encoder\n", $1 }' "$fixture"
  exit 0
fi

record="$(awk -F '\t' -v factory="$1" '$1 == factory { print; exit }' "$fixture")"
[[ -n "$record" ]] || exit 1
IFS=$'\t' read -r factory device modes completeness <<<"$record"
printf '%s\n' \
  '  device-path         : DRM render device' \
  '                        flags: readable' \
  "                        String. Default: \"$device\"" \
  '  rate-control        : Rate control mode' \
  '                        flags: readable, writable'
case ",$modes," in
  *,cqp,*) printf '%s\n' '                           (1): cqp' ;;
esac
case ",$modes," in
  *,cbr,*) printf '%s\n' '                           (2): cbr' ;;
esac
for property in aud b-frames key-int-max ref-frames target-usage bitrate qpi qpp; do
  [[ "$completeness" == "missing-$property" ]] && continue
  printf '  %-20s: fixture property\n' "$property"
  printf '%s\n' '                        flags: readable, writable'
done
EOF
chmod 0755 "$bin/gst-inspect-1.0"
export SIGIL_TEST_FACTORY_FIXTURE="$factory_fixture"

select_fixture() {
  select_amd_gstva_h264_encoder \
    "$bin/gst-inspect-1.0" "$sysfs_root" "$device_root" false \
    "${1:-}" "${2:-}"
}

reset_render_nodes
add_render_node renderD128 i915
add_render_node renderD129 amdgpu
printf '%s\t%s\t%s\t%s\n' \
  vah264enc "$device_root/renderD128" cqp,cbr complete \
  varenderD129h264lpenc "$device_root/renderD129" cqp complete \
  varenderD129h264enc "$device_root/renderD129" cqp,cbr complete \
  >"$factory_fixture"
select_fixture || fail 'a matching non-primary AMD encoder was not selected'
[[ "$render_node" == "$device_root/renderD129" ]] || \
  fail 'the Intel renderD128 node was selected instead of the AMD node'
[[ "$va_encoder" == varenderD129h264enc ]] || \
  fail 'the exact CBR+CQP per-device factory was not selected'

reset_render_nodes
add_render_node renderD130 amdgpu
printf '%s\t%s\t%s\t%s\n' \
  vah264enc "$device_root/renderD130" cqp,cbr complete \
  >"$factory_fixture"
select_fixture || fail 'a generic factory backed by renderD130 was not selected'
[[ "$render_node" == "$device_root/renderD130" && "$va_encoder" == vah264enc ]] || \
  fail 'selection retained a renderD128 or per-device factory assumption'

reset_render_nodes
add_render_node renderD128 amdgpu
add_render_node renderD129 amdgpu
printf '%s\t%s\t%s\t%s\n' \
  vah264enc "$device_root/renderD128" cqp,cbr complete \
  varenderD129h264enc "$device_root/renderD129" cqp,cbr complete \
  >"$factory_fixture"
selection_status=0
select_fixture || selection_status=$?
[[ "$selection_status" -eq 2 ]] || fail 'two viable AMD pairs were not ambiguous'
[[ -z "$render_node" && -z "$va_encoder" ]] || \
  fail 'ambiguous discovery retained a silent first selection'
grep -Fxq "$device_root/renderD128 vah264enc" <<<"$va_candidates" || \
  fail 'the first ambiguous pair was not reported'
grep -Fxq "$device_root/renderD129 varenderD129h264enc" <<<"$va_candidates" || \
  fail 'the second ambiguous pair was not reported'

select_fixture "$device_root/renderD129" varenderD129h264enc || \
  fail 'a valid explicit pair was rejected'
[[ "$render_node" == "$device_root/renderD129" \
  && "$va_encoder" == varenderD129h264enc ]] || \
  fail 'the explicit pair did not win ambiguous discovery'
if select_fixture "$device_root/renderD129" vah264enc; then
  fail 'a mismatched explicit node/factory pair was accepted'
fi

reset_render_nodes
add_render_node renderD130 amdgpu
printf '%s\t%s\t%s\t%s\n' \
  vah264enc "$device_root/renderD129" cqp,cbr complete \
  >"$factory_fixture"
if select_fixture; then
  fail 'a factory whose device-path mismatched the AMD node was accepted'
fi

for modes in cqp cbr; do
  printf '%s\t%s\t%s\t%s\n' \
    vah264enc "$device_root/renderD130" "$modes" complete \
    >"$factory_fixture"
  if select_fixture; then
    fail "a factory advertising only $modes was accepted"
  fi
done
printf '%s\t%s\t%s\t%s\n' \
  vah264enc "$device_root/renderD130" cqp,cbr missing-bitrate \
  >"$factory_fixture"
if select_fixture; then
  fail 'a factory missing a required writable property was accepted'
fi

printf '%s\t%s\t%s\t%s\n' \
  x264enc "$device_root/renderD130" cqp,cbr complete \
  vah264enc-malformed "$device_root/renderD130" cqp,cbr complete \
  >"$factory_fixture"
if select_fixture; then
  fail 'malformed or software factory names were accepted'
fi

printf '%s\t%s\t%s\t%s\n' \
  vah264enc "$device_root/renderD130" cqp,cbr complete \
  vah264enc "$device_root/renderD130" cqp,cbr complete \
  >"$factory_fixture"
select_fixture || fail 'duplicate listing of one factory became ambiguous'
[[ "$va_encoder" == vah264enc ]] || fail 'duplicate factory selection was unstable'

rm -f -- "$device_root/renderD130"
ln -s "$temp_root/missing-render-node" "$device_root/renderD130"
if select_fixture; then
  fail 'a symlink render node was accepted'
fi
rm -f -- "$device_root/renderD130"
install -m 0600 /dev/null "$device_root/renderD130"
if select_amd_gstva_h264_encoder \
  "$bin/gst-inspect-1.0" "$sysfs_root" "$device_root" true '' ''
then
  fail 'a regular file was accepted as a production DRM character device'
fi
[[ -z "$render_node" && -z "$va_encoder" ]] || \
  fail 'failed discovery retained stale selection state'

encoder_config_line="vaapi_encoder = \"\$va_encoder\""
render_node_config_line="vaapi_render_node = \"\$render_node\""
[[ "$(grep -Fc "$encoder_config_line" "$runner")" -eq 3 ]] || \
  fail 'not every generated UAT config uses the selected factory'
[[ "$(grep -Fc "$render_node_config_line" "$runner")" -eq 3 ]] || \
  fail 'not every generated UAT config uses the selected render node'
if grep -Eq 'render_node=/dev/dri/renderD128|va_encoder=vah264enc' "$runner"; then
  fail 'the hardware runner still pins the original render node or factory'
fi

fake_commit=0123456789abcdef0123456789abcdef01234567
parser_error="$temp_root/parser.error"
parser_status=0
SIGIL_HARDWARE_UAT_SOURCE_ONLY=0 bash "$runner" /invalid "$fake_commit" 1 \
  --render-node /dev/dri/renderD129 > /dev/null 2>"$parser_error" \
  || parser_status=$?
[[ "$parser_status" -eq 64 ]] || fail 'a partial override did not fail as usage error'
grep -Fxq -- '--render-node and --va-encoder must be provided together' \
  "$parser_error" || fail 'a partial override did not explain the paired requirement'

parser_status=0
SIGIL_HARDWARE_UAT_SOURCE_ONLY=0 bash "$runner" /invalid "$fake_commit" 1 \
  --unknown value > /dev/null 2>"$parser_error" || parser_status=$?
[[ "$parser_status" -eq 64 ]] || fail 'an unknown override did not fail as usage error'
grep -Fq 'usage:' "$parser_error" || fail 'an unknown override did not print usage'

parser_status=0
SIGIL_HARDWARE_UAT_SOURCE_ONLY=0 bash "$runner" /invalid "$fake_commit" 1 \
  --render-node /dev/dri/renderD128 --render-node /dev/dri/renderD129 \
  --va-encoder vah264enc > /dev/null 2>"$parser_error" || parser_status=$?
[[ "$parser_status" -eq 64 ]] || fail 'a duplicate override was accepted'

parser_status=0
SIGIL_HARDWARE_UAT_SOURCE_ONLY=0 bash "$runner" /invalid "$fake_commit" 1 \
  --render-node '' --va-encoder vah264enc \
  > /dev/null 2>"$parser_error" || parser_status=$?
[[ "$parser_status" -eq 64 ]] || fail 'an empty override value was accepted'

printf 'bazzite_hardware_uat_selection_tests=ok\n'
