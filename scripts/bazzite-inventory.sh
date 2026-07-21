#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Linux ]]; then
  echo "bazzite-inventory must run on the Linux host" >&2
  exit 1
fi

smoke=false
if [[ "${1:-}" == "--smoke" ]]; then
  smoke=true
elif [[ $# -ne 0 ]]; then
  echo "usage: $0 [--smoke]" >&2
  exit 2
fi

section() {
  printf '\n## %s\n' "$1"
}

optional() {
  local command_name="$1"
  shift
  if command -v "$command_name" >/dev/null 2>&1; then
    "$@" || printf 'command_failed=%s status=%s\n' "$command_name" "$?"
  else
    printf 'command_missing=%s\n' "$command_name"
  fi
}

section identity
date --iso-8601=ns
hostnamectl
uname -a
id

section image
sed -n -E '/^(NAME|VERSION|VERSION_ID|VARIANT|VARIANT_ID|IMAGE_ID|IMAGE_VERSION)=/p' /etc/os-release
optional rpm-ostree rpm-ostree status -v
optional bootc bootc status
optional mokutil mokutil --sb-state
optional getenforce getenforce

section storage
df -h / /var "$HOME"

section network_and_services
optional ss ss -lntup
systemctl list-unit-files --state=enabled --no-pager
loginctl list-sessions --no-legend

section versions
optional gamescope gamescope --version
optional pipewire pipewire --version
optional ffmpeg ffmpeg -version
optional gst-inspect-1.0 gst-inspect-1.0 --version
optional podman podman --version
optional rustc rustc --version
optional cargo cargo --version

section gpu
find /dev/dri -maxdepth 1 -mindepth 1 -printf '%M %u %g %p\n' 2>/dev/null | sort || true
render_node=""
for sysfs_node in /sys/class/drm/renderD*; do
  [[ -e "$sysfs_node/device/driver" ]] || continue
  driver="$(basename "$(readlink -f "$sysfs_node/device/driver")")"
  device="$(basename "$(readlink -f "$sysfs_node/device")")"
  printf 'render_node=/dev/dri/%s driver=%s device=%s\n' \
    "$(basename "$sysfs_node")" "$driver" "$device"
  if [[ -z "$render_node" && "$driver" == amdgpu ]]; then
    render_node="/dev/dri/$(basename "$sysfs_node")"
  fi
done
optional lspci lspci -nnk
optional vulkaninfo vulkaninfo --summary

if [[ -n "$render_node" ]]; then
  printf 'selected_render_node=%s\n' "$render_node"
  [[ -r "$render_node" && -w "$render_node" ]]
  optional udevadm udevadm info --query=property --name="$render_node"
  optional vainfo vainfo --display drm --device "$render_node"
else
  echo "selected_render_node=none" >&2
fi

section encoders
if command -v ffmpeg >/dev/null 2>&1; then
  ffmpeg -hide_banner -encoders 2>/dev/null | grep -E '(^|[[:space:]])(libx264|h264_(vaapi|amf|nvenc|videotoolbox))([[:space:]]|$)' || true
fi

section capture_backends
if command -v ffmpeg >/dev/null 2>&1; then
  ffmpeg -hide_banner -devices 2>/dev/null || true
fi
if command -v gst-inspect-1.0 >/dev/null 2>&1; then
  for element in pipewiresrc queue videoconvert videoscale videorate h264parse fdsink; do
    if gst-inspect-1.0 "$element" >/dev/null 2>&1; then
      printf 'gstreamer_element=%s status=present\n' "$element"
    else
      printf 'gstreamer_element=%s status=missing\n' "$element"
    fi
  done
  gst-inspect-1.0 pipewiresrc 2>/dev/null || true
  while IFS= read -r encoder; do
    printf 'gstreamer_va_h264_factory=%s\n' "$encoder"
    gst-inspect-1.0 "$encoder" 2>/dev/null || true
  done < <(
    gst-inspect-1.0 2>/dev/null | awk '
      $2 ~ /^va(renderD[0-9]+)?h264(lp)?enc:$/ {
        sub(/:$/, "", $2)
        print $2
      }
    '
  )
fi

section gamescope_pipewire
systemctl --user status pipewire.service pipewire.socket wireplumber.service --no-pager || true
pgrep -a gamescope || true
pgrep -a steam || true
if command -v pw-dump >/dev/null 2>&1 && command -v jq >/dev/null 2>&1; then
  pw-dump | jq '[
    .[]
    | select(.type == "PipeWire:Interface:Node")
    | select(
        .info.props["node.name"] == "gamescope"
        or .info.props["media.class"] == "Video/Source"
      )
    | {
        global_id: .id,
        object_serial: .info.props["object.serial"],
        node_id: .info.props["node.id"],
        node_name: .info.props["node.name"],
        media_class: .info.props["media.class"],
        description: .info.props["node.description"],
        object_path: .info.props["object.path"],
        client_id: .info.props["client.id"]
      }
  ]'
else
  echo "pipewire_node_inventory=unavailable"
fi

if [[ "$smoke" == true ]]; then
  section h264_smoke
  if [[ -z "$render_node" ]]; then
    echo "AMD render node is required for --smoke" >&2
    exit 1
  fi
  ffmpeg -hide_banner -loglevel warning \
    -vaapi_device "$render_node" \
    -f lavfi -i testsrc2=size=1280x800:rate=60 \
    -vf 'format=nv12,hwupload' \
    -c:v h264_vaapi \
    -frames:v 600 \
    -f null -
  echo "h264_vaapi_smoke=ok"
fi
