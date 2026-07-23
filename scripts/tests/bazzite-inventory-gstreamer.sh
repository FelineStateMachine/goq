#!/usr/bin/env bash

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
temp_parent="${TMPDIR:-/tmp}"
temp_parent="${temp_parent%/}"
temp_root="$(mktemp -d "$temp_parent/sigil-gstreamer-inventory.XXXXXX")"

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  case "$temp_root" in
    "$temp_parent"/sigil-gstreamer-inventory.??????) rm -rf -- "$temp_root" ;;
  esac
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

export SIGIL_INVENTORY_SOURCE_ONLY=1
# shellcheck source=../bazzite-inventory.sh
# shellcheck disable=SC1091
source "$script_dir/../bazzite-inventory.sh"

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

install -d -m 0700 "$temp_root/bin"
cat >"$temp_root/bin/gst-inspect-1.0" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case "${1:-}" in
  appsink)
    [[ "${SIGIL_TEST_APPSINK:-present}" == present ]]
    ;;
  pipewiresrc)
    printf 'Factory Details: pipewiresrc fixture\n'
    ;;
  queue|videoconvert|videoscale|videorate|h264parse|fdsink|vah264enc)
    exit 0
    ;;
  '')
    printf 'va:  vah264enc: VA-API H.264 Encoder\n'
    ;;
  *)
    exit 1
    ;;
esac
EOF
chmod 0755 "$temp_root/bin/gst-inspect-1.0"
cat >"$temp_root/bin/pkg-config" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ "${1:-}" == --modversion && $# -eq 2 ]]
if [[ "${SIGIL_TEST_MISSING_MODULE:-}" == "$2" ]]; then
  exit 1
fi
printf '1.26.11\n'
EOF
chmod 0755 "$temp_root/bin/pkg-config"
cat >"$temp_root/bin/ffmpeg" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
while [[ $# -gt 0 ]]; do
  if [[ "$1" == -vaapi_device && $# -ge 2 ]]; then
    [[ "$2" == "${SIGIL_TEST_VAAPI_NODE:?}" ]]
    exit
  fi
  shift
done
exit 1
EOF
chmod 0755 "$temp_root/bin/ffmpeg"

PATH="$temp_root/bin:$PATH"
export PATH

present_output="$(SIGIL_TEST_APPSINK=present gstreamer_inventory)"
grep -qxF 'gstreamer_element=appsink status=present' <<<"$present_output" || \
  fail 'appsink presence was not recorded'
grep -qxF 'gstreamer_va_h264_factory=vah264enc' <<<"$present_output" || \
  fail 'GstVA factory was not recorded'

missing_output="$(SIGIL_TEST_APPSINK=missing gstreamer_inventory)"
grep -qxF 'gstreamer_element=appsink status=missing' <<<"$missing_output" || \
  fail 'appsink absence was not recorded'

install -d -m 0700 "$temp_root/dev/dri"
install -m 0600 /dev/null "$temp_root/dev/dri/renderD128"
install -m 0600 /dev/null "$temp_root/dev/dri/renderD129"
SIGIL_TEST_VAAPI_NODE="$temp_root/dev/dri/renderD129"
export SIGIL_TEST_VAAPI_NODE
render_node=''
select_h264_vaapi_render_node "$temp_root/dev/dri" false || \
  fail 'a capability-backed non-AMD render node was not selected'
[[ "$render_node" == "$SIGIL_TEST_VAAPI_NODE" ]] || \
  fail 'render-node selection used enumeration order instead of the encode probe'

development_output="$(SIGIL_TEST_MISSING_MODULE=gstreamer-video-1.0 \
  gstreamer_development_inventory)"
grep -qxF \
  'gstreamer_development_module=gstreamer-app-1.0 status=present version=1.26.11' \
  <<<"$development_output" || fail 'GStreamer app development version was not recorded'
grep -qxF \
  'gstreamer_development_module=gstreamer-video-1.0 status=missing' \
  <<<"$development_output" || fail 'missing GStreamer video development module was not recorded'

printf 'bazzite_inventory_gstreamer_tests=ok\n'
