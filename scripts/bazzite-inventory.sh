#!/usr/bin/env bash
set -euo pipefail

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

cold_boot_failures=0
cold_boot_insufficient=0

cold_boot_failure() {
  printf 'cold_boot_failure=%s\n' "$1"
  ((cold_boot_failures += 1))
}

cold_boot_evidence_insufficient() {
  printf 'cold_boot_evidence_insufficient=%s\n' "$1"
  ((cold_boot_insufficient += 1))
}

summary_value() {
  local key="$1"
  sed -n "s/^${key}=//p" | head -n 1
}

monotonic_before() {
  local earlier="$1"
  local later="$2"
  awk -v earlier="$earlier" -v later="$later" \
    'BEGIN { exit !(earlier < later) }'
}

summarize_boot_journal() {
  awk '
    function monotonic(line, value) {
      value = line
      sub(/^[[:space:]]*\[/, "", value)
      sub(/\].*$/, "", value)
      return value
    }
    {
      timestamp = monotonic($0)
      if (timestamp !~ /^[0-9]+([.][0-9]+)?$/) {
        next
      }
      entries += 1
      if (first == "") {
        first = timestamp
      }
      line = tolower($0)
      if (gamescope == "" && (line ~ /gamescope-session(-plus)?\[[0-9]+\]/ || line ~ /started .*gamescope/)) {
        gamescope = timestamp
      }
      if (sigil_start == "" && line ~ /started sigil-host[.]service/) {
        sigil_start = timestamp
      }
      if (sigil_ready == "" && line ~ /sigil host ready/) {
        sigil_ready = timestamp
      }
      if (ssh_accept == "" && line ~ /accepted (publickey|password)/) {
        ssh_accept = timestamp
      }
    }
    END {
      printf "journal_entries=%d\n", entries
      printf "journal_first_monotonic_seconds=%s\n", first
      printf "gamescope_first_monotonic_seconds=%s\n", gamescope
      printf "sigil_unit_first_monotonic_seconds=%s\n", sigil_start
      printf "sigil_ready_first_monotonic_seconds=%s\n", sigil_ready
      printf "ssh_accept_first_monotonic_seconds=%s\n", ssh_accept
    }
  '
}

cold_boot_connector_evidence() {
  local connector_status
  local connector_name
  local status
  local found=false
  local connected=false

  for connector_status in /sys/class/drm/card*-*/status; do
    [[ -e "$connector_status" ]] || continue
    found=true
    connector_name="$(basename "$(dirname "$connector_status")")"
    status="$(tr -d '\n' < "$connector_status")"
    printf 'drm_connector=%s status=%s\n' "$connector_name" "$status"
    if [[ "$status" == connected ]]; then
      connected=true
      cold_boot_failure "physical_connector_connected:$connector_name"
    fi
  done

  if [[ "$found" == false ]]; then
    cold_boot_evidence_insufficient no_drm_connector_status_files
  elif [[ "$connected" == false ]]; then
    echo 'headless_connector_state=ok'
  fi
}

cold_boot_session_evidence() {
  local sessions
  local session_id
  local session_details
  local current_user_id
  local autologin_found=false

  current_user_id="$(id -u)"

  if ! sessions="$(loginctl list-sessions --no-legend 2>&1)"; then
    printf 'loginctl_error=%s\n' "$sessions"
    cold_boot_evidence_insufficient login_sessions_unavailable
    return
  fi

  while read -r session_id _; do
    [[ -n "$session_id" ]] || continue
    if ! session_details="$(loginctl show-session "$session_id" \
      -p Id -p User -p Remote -p Service -p Type -p Class -p State -p Timestamp 2>&1)"; then
      printf 'session_id=%s details=unavailable\n' "$session_id"
      cold_boot_evidence_insufficient "session_details_unavailable:$session_id"
      continue
    fi
    printf 'session_begin=%s\n%s\nsession_end=%s\n' \
      "$session_id" "$session_details" "$session_id"
    if grep -qx 'Service=sddm-autologin' <<<"$session_details" &&
      grep -qx "User=$current_user_id" <<<"$session_details" &&
      grep -qx 'Remote=no' <<<"$session_details" &&
      grep -qx 'State=active' <<<"$session_details"; then
      autologin_found=true
    fi
  done <<<"$sessions"

  if [[ "$autologin_found" == true ]]; then
    echo 'gaming_autologin_session=ok'
  else
    cold_boot_failure no_active_local_sddm_autologin_session
  fi
}

cold_boot_service_evidence() {
  local enabled
  local active
  local service_details

  enabled="$(systemctl --user is-enabled sigil-host.service 2>&1 || true)"
  active="$(systemctl --user is-active sigil-host.service 2>&1 || true)"
  printf 'sigil_host_enabled=%s\n' "$enabled"
  printf 'sigil_host_active=%s\n' "$active"
  [[ "$enabled" == enabled ]] || cold_boot_failure sigil_host_not_enabled
  [[ "$active" == active ]] || cold_boot_failure sigil_host_not_active

  if service_details="$(systemctl --user show sigil-host.service \
    -p ActiveEnterTimestamp -p ActiveEnterTimestampMonotonic \
    -p ExecMainStartTimestamp -p FragmentPath -p NRestarts 2>&1)"; then
    printf '%s\n' "$service_details"
  else
    printf 'sigil_host_show_error=%s\n' "$service_details"
    cold_boot_evidence_insufficient sigil_host_service_details_unavailable
  fi
}

cold_boot_pipewire_evidence() {
  local pipewire_nodes
  local default_sink

  if ! command -v pw-dump >/dev/null 2>&1 || ! command -v jq >/dev/null 2>&1; then
    cold_boot_evidence_insufficient pipewire_inventory_tools_missing
    return
  fi

  if ! pipewire_nodes="$(pw-dump 2>/dev/null | jq -r '
    .[]
    | select(.type == "PipeWire:Interface:Node")
    | if (
        .info.props["node.name"] == "gamescope"
        and .info.props["media.class"] == "Video/Source"
      ) then "gamescope_video"
      elif (
        .info.props["node.name"] == "sigil_spark"
        and .info.props["media.class"] == "Audio/Sink"
      ) then "sigil_audio"
      else empty
      end
  ' | sort -u)"; then
    cold_boot_evidence_insufficient pipewire_nodes_unavailable
    return
  fi

  if grep -qx gamescope_video <<<"$pipewire_nodes"; then
    echo 'gamescope_pipewire_node=ok'
  else
    cold_boot_failure gamescope_pipewire_node_missing
  fi
  if grep -qx sigil_audio <<<"$pipewire_nodes"; then
    echo 'sigil_audio_pipewire_node=ok'
  else
    cold_boot_failure sigil_audio_pipewire_node_missing
  fi

  if ! command -v pactl >/dev/null 2>&1; then
    cold_boot_evidence_insufficient pactl_missing
  elif default_sink="$(pactl get-default-sink 2>&1)"; then
    printf 'default_audio_sink=%s\n' "$default_sink"
    [[ "$default_sink" == sigil_spark ]] || cold_boot_failure default_audio_sink_not_sigil_spark
  else
    printf 'pactl_error=%s\n' "$default_sink"
    cold_boot_evidence_insufficient default_audio_sink_unavailable
  fi
}

cold_boot_journal_evidence() {
  local journal_summary
  local journal_entries
  local journal_first
  local gamescope_first
  local sigil_unit_first
  local sigil_ready_first
  local ssh_accept_first

  if ! journal_summary="$(journalctl -b -o short-monotonic --no-pager 2>/dev/null | \
    summarize_boot_journal)"; then
    cold_boot_evidence_insufficient boot_journal_unavailable
    return
  fi
  printf '%s\n' "$journal_summary"

  journal_entries="$(summary_value journal_entries <<<"$journal_summary")"
  journal_first="$(summary_value journal_first_monotonic_seconds <<<"$journal_summary")"
  gamescope_first="$(summary_value gamescope_first_monotonic_seconds <<<"$journal_summary")"
  sigil_unit_first="$(summary_value sigil_unit_first_monotonic_seconds <<<"$journal_summary")"
  sigil_ready_first="$(summary_value sigil_ready_first_monotonic_seconds <<<"$journal_summary")"
  ssh_accept_first="$(summary_value ssh_accept_first_monotonic_seconds <<<"$journal_summary")"

  if [[ ! "$journal_entries" =~ ^[1-9][0-9]*$ ||
    ! "$journal_first" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
    cold_boot_evidence_insufficient boot_journal_empty_or_unparseable
    return
  fi
  if ! awk -v first="$journal_first" 'BEGIN { exit !(first <= 300) }'; then
    cold_boot_evidence_insufficient boot_journal_does_not_reach_startup
  fi
  if [[ ! "$ssh_accept_first" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
    cold_boot_evidence_insufficient first_ssh_acceptance_not_observable
    return
  fi

  if [[ ! "$gamescope_first" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
    cold_boot_failure gamescope_start_not_found_in_boot_journal
  elif ! monotonic_before "$gamescope_first" "$ssh_accept_first"; then
    cold_boot_failure gamescope_did_not_start_before_first_ssh
  else
    echo 'gamescope_before_first_ssh=ok'
  fi

  if [[ ! "$sigil_unit_first" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
    cold_boot_failure sigil_unit_start_not_found_in_boot_journal
  elif ! monotonic_before "$sigil_unit_first" "$ssh_accept_first"; then
    cold_boot_failure sigil_unit_did_not_start_before_first_ssh
  else
    echo 'sigil_unit_before_first_ssh=ok'
  fi

  if [[ ! "$sigil_ready_first" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
    cold_boot_failure sigil_ready_not_found_in_boot_journal
  elif ! monotonic_before "$sigil_ready_first" "$ssh_accept_first"; then
    cold_boot_failure sigil_was_not_ready_before_first_ssh
  else
    echo 'sigil_ready_before_first_ssh=ok'
  fi
}

cold_boot_audit() {
  local boot_id

  section cold_boot_evidence
  if boot_id="$(< /proc/sys/kernel/random/boot_id)" && [[ -n "$boot_id" ]]; then
    printf 'boot_id=%s\n' "$boot_id"
  else
    cold_boot_evidence_insufficient boot_id_unavailable
  fi

  cold_boot_connector_evidence
  cold_boot_session_evidence
  cold_boot_service_evidence
  cold_boot_pipewire_evidence
  cold_boot_journal_evidence

  printf 'cold_boot_failure_count=%d\n' "$cold_boot_failures"
  printf 'cold_boot_insufficient_count=%d\n' "$cold_boot_insufficient"
  if ((cold_boot_insufficient > 0)); then
    echo 'cold_boot_result=insufficient'
    return 3
  fi
  if ((cold_boot_failures > 0)); then
    echo 'cold_boot_result=fail'
    return 1
  fi
  echo 'cold_boot_result=pass'
}

if [[ "${SIGIL_INVENTORY_SOURCE_ONLY:-}" == 1 ]]; then
  if [[ "${BASH_SOURCE[0]}" != "$0" ]]; then
    return 0
  fi
  exit 0
fi

if [[ "$(uname -s)" != Linux ]]; then
  echo "bazzite-inventory must run on the Linux host" >&2
  exit 1
fi

mode=inventory
if [[ "${1:-}" == "--smoke" && $# -eq 1 ]]; then
  mode=smoke
elif [[ "${1:-}" == "--cold-boot" && $# -eq 1 ]]; then
  mode=cold-boot
elif [[ $# -ne 0 ]]; then
  echo "usage: $0 [--smoke | --cold-boot]" >&2
  exit 2
fi

if [[ "$mode" == cold-boot ]]; then
  cold_boot_status=0
  cold_boot_audit || cold_boot_status=$?
  exit "$cold_boot_status"
fi

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

if [[ "$mode" == smoke ]]; then
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
