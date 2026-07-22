#!/usr/bin/env bash

set -euo pipefail

GST_INSPECT_MAX_BYTES=1048576

run_bounded_gst_inspect() {
  local timeout_path="$1"
  local gst_inspect_path="$2"
  local output_path="$3"
  local output_size
  shift 3

  if ! "$timeout_path" --signal=TERM --kill-after=2s 5s \
    "$gst_inspect_path" "$@" 2>/dev/null \
    | head -c "$((GST_INSPECT_MAX_BYTES + 1))" >"$output_path"
  then
    return 1
  fi
  output_size="$(wc -c <"$output_path" | tr -d '[:space:]')"
  [[ "$output_size" =~ ^[0-9]+$ && "$output_size" -le "$GST_INSPECT_MAX_BYTES" ]]
}

gst_property_block_contains() {
  local property="$1"
  local needle="$2"

  awk -v property="$property" -v needle="$needle" '
    $0 ~ ("^  " property "[[:space:]]+:") {
      inside = 1
    }
    inside && index($0, needle) {
      found = 1
    }
    inside && $0 !~ ("^  " property "[[:space:]]+:") \
      && $0 ~ /^  [^[:space:]].*[[:space:]]+:/ {
      exit
    }
    END { exit !found }
  '
}

gst_property_block_is_writable() {
  local property="$1"

  awk -v property="$property" '
    $0 ~ ("^  " property "[[:space:]]+:") {
      inside = 1
    }
    inside && index(tolower($0), "writable") {
      found = 1
    }
    inside && $0 !~ ("^  " property "[[:space:]]+:") \
      && $0 ~ /^  [^[:space:]].*[[:space:]]+:/ {
      exit
    }
    END { exit !found }
  '
}

gstva_factory_supports_uat() {
  local inspection="$1"
  local property

  gst_property_block_contains rate-control cbr <<<"$inspection" || return 1
  gst_property_block_contains rate-control cqp <<<"$inspection" || return 1
  for property in \
    aud b-frames key-int-max rate-control ref-frames target-usage bitrate qpi qpp
  do
    gst_property_block_is_writable "$property" <<<"$inspection" || return 1
  done
}

select_amd_gstva_h264_encoder() {
  local gst_inspect_path="$1"
  local sysfs_root="${2:-/sys/class/drm}"
  local device_root="${3:-/dev/dri}"
  local require_character="${4:-true}"
  local override_node="${5:-}"
  local override_factory="${6:-}"
  local timeout_path="${7:?timeout command is required}"
  local output_root="${8:?bounded inspection output directory is required}"
  local factory
  local factories
  local factory_output
  local inspection
  local node_name
  local sysfs_node
  local driver
  local candidate
  local pair
  local candidate_count=0

  render_node=''
  va_encoder=''
  va_candidates=''
  [[ -d "$output_root" && ! -L "$output_root" ]] || return 1
  if ! LC_ALL=C run_bounded_gst_inspect \
    "$timeout_path" "$gst_inspect_path" "$output_root/gst-inspect.registry";
  then
    return 1
  fi
  factories="$(awk '
    $2 ~ /^va(renderD[0-9]+)?h264(lp)?enc:$/ {
      sub(/:$/, "", $2)
      print $2
    }
  ' "$output_root/gst-inspect.registry" | sort -u)" || return 1

  for sysfs_node in "$sysfs_root"/renderD*; do
    [[ -e "$sysfs_node/device/driver" ]] || continue
    node_name="$(basename "$sysfs_node")"
    [[ "$node_name" =~ ^renderD[0-9]+$ ]] || continue
    driver="$(basename "$(readlink -f "$sysfs_node/device/driver")")" || continue
    [[ "$driver" == amdgpu ]] || continue
    candidate="$device_root/$node_name"
    [[ -e "$candidate" && ! -L "$candidate" && -r "$candidate" && -w "$candidate" ]] \
      || continue
    if [[ "$require_character" == true && ! -c "$candidate" ]]; then
      continue
    fi

    while IFS= read -r factory; do
      [[ -n "$factory" ]] || continue
      factory_output="$output_root/gst-inspect.$factory"
      if ! LC_ALL=C run_bounded_gst_inspect \
        "$timeout_path" "$gst_inspect_path" "$factory_output" "$factory";
      then
        continue
      fi
      inspection="$(<"$factory_output")"
      gst_property_block_contains device-path "Default: \"$candidate\"" \
        <<<"$inspection" || continue
      gstva_factory_supports_uat "$inspection" || continue
      pair="$candidate $factory"
      if [[ -n "$va_candidates" ]]; then
        va_candidates="$va_candidates
$pair"
      else
        va_candidates="$pair"
      fi
      candidate_count=$((candidate_count + 1))
      if [[ -n "$override_node" && "$candidate" == "$override_node" \
        && "$factory" == "$override_factory" ]]; then
        render_node="$candidate"
        va_encoder="$factory"
      elif [[ -z "$override_node" && "$candidate_count" -eq 1 ]]; then
        render_node="$candidate"
        va_encoder="$factory"
      fi
    done <<<"$factories"
  done

  if [[ -n "$override_node" ]]; then
    [[ -n "$render_node" && -n "$va_encoder" ]]
    return
  fi
  if [[ "$candidate_count" -eq 1 ]]; then
    return 0
  fi
  render_node=''
  va_encoder=''
  [[ "$candidate_count" -eq 0 ]] && return 1
  return 2
}

if [[ "${SIGIL_HARDWARE_UAT_SOURCE_ONLY:-}" == 1 ]]; then
  if [[ "${BASH_SOURCE[0]}" != "$0" ]]; then
    return 0
  fi
  exit 0
fi

usage() {
  echo "usage: $0 UAT_ROOT UAT_COMMIT WORKFLOW_RUN_ID [--render-node PATH --va-encoder FACTORY]" >&2
}

if [[ $# -lt 3 ]]; then
  usage
  exit 64
fi

uat_root="$1"
uat_commit="$2"
workflow_run_id="$3"
shift 3
render_node_override=''
va_encoder_override=''
render_node_override_set=false
va_encoder_override_set=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --render-node)
      [[ $# -ge 2 && "$render_node_override_set" == false && -n "$2" ]] \
        || { usage; exit 64; }
      render_node_override="$2"
      render_node_override_set=true
      shift 2
      ;;
    --va-encoder)
      [[ $# -ge 2 && "$va_encoder_override_set" == false && -n "$2" ]] \
        || { usage; exit 64; }
      va_encoder_override="$2"
      va_encoder_override_set=true
      shift 2
      ;;
    *)
      usage
      exit 64
      ;;
  esac
done
if [[ "$render_node_override_set" == true || "$va_encoder_override_set" == true ]]; then
  [[ "$render_node_override_set" == true && "$va_encoder_override_set" == true ]] || {
    echo "--render-node and --va-encoder must be provided together" >&2
    exit 64
  }
fi
short_commit="${uat_commit:0:12}"

[[ "$uat_commit" =~ ^[0-9a-f]{40}$ ]] || {
  echo "invalid UAT commit" >&2
  exit 64
}
[[ "$workflow_run_id" =~ ^[0-9]+$ ]] || {
  echo "invalid workflow run ID" >&2
  exit 64
}
case "$uat_root" in
  "$HOME"/.local/state/goq-hardware-uat."$short_commit".??????) ;;
  *) echo "unsafe UAT root: $uat_root" >&2; exit 64 ;;
esac
for trusted_path in "$HOME/.local" "$HOME/.local/state" "$uat_root" "$uat_root/incoming"; do
  [[ -d "$trusted_path" && ! -L "$trusted_path" ]] || {
    echo "UAT path is missing or symlinked: $trusted_path" >&2
    exit 64
  }
done
[[ "$(stat -c '%a:%U' "$uat_root")" == "700:$(id -un)" ]] || {
  echo "UAT root is not owner-only" >&2
  exit 64
}

umask 077
XDG_RUNTIME_DIR="/run/user/$(id -u)"
export XDG_RUNTIME_DIR

lock_file="$HOME/.local/state/goq-hardware-uat.lock"
[[ ! -L "$lock_file" ]] || {
  echo "unsafe UAT lock path: $lock_file" >&2
  exit 64
}
exec {uat_lock_fd}>"$lock_file"
if ! flock -n "$uat_lock_fd"; then
  echo "another hardware UAT owns the host-wide lock" >&2
  exit 75
fi

root_suffix="${uat_root##*.}"
[[ "$root_suffix" =~ ^[[:alnum:]]{6}$ ]] || {
  echo "invalid UAT root suffix" >&2
  exit 64
}
invocation_id="$short_commit-$workflow_run_id-$root_suffix"

incoming="$uat_root/incoming"
archive="$incoming/sigil-hardware-uat-$short_commit-bazzite-x86_64.tar.gz"
payload_root="$uat_root/candidate"
private="$uat_root/private"
evidence="$uat_root/evidence"
raw="$private/raw"
for new_path in "$payload_root" "$private" "$evidence"; do
  [[ ! -e "$new_path" && ! -L "$new_path" ]] || {
    echo "UAT output path already exists: $new_path" >&2
    exit 64
  }
done
install -d -m 0700 "$payload_root" "$private" "$evidence" "$raw"
for new_path in "$payload_root" "$private" "$evidence" "$raw"; do
  [[ -d "$new_path" && ! -L "$new_path" ]]
  [[ "$(stat -c '%a:%U' "$new_path")" == "700:$(id -un)" ]]
done

real_config="$HOME/.config/sigil-spark/host.toml"
real_sigil="$HOME/.local/libexec/sigil-spark/current/sigil"
real_identity="$(sed -n 's/^identity_path = "\(.*\)"$/\1/p' "$real_config")"
uinput_block="$(awk '
  /^\[uinput\]$/ { emit = 1 }
  emit && /^\[/ && $0 != "[uinput]" { exit }
  emit { print }
' "$real_config")"
[[ -n "$real_identity" && -f "$real_identity" ]]
[[ "$uinput_block" == '[uinput]'$'\n'* ]]
[[ -x "$real_sigil" ]]
original_ready() {
  local invocation pid
  systemctl --user is-active --quiet sigil-host.service || return 1
  pid="$(systemctl --user show -p MainPID --value sigil-host.service)"
  invocation="$(systemctl --user show -p InvocationID --value sigil-host.service)"
  [[ "$pid" =~ ^[0-9]+$ && "$pid" -gt 1 ]] || return 1
  [[ "$invocation" =~ ^[0-9a-f]{32}$ ]] || return 1
  cmp -s "$real_sigil" "/proc/$pid/exe" || return 1
  journalctl --user _SYSTEMD_INVOCATION_ID="$invocation" \
    _SYSTEMD_USER_UNIT=sigil-host.service _PID="$pid" --no-pager -o cat \
    | grep -Fx status=ready >/dev/null
}
original_ready
original_enabled="$(systemctl --user is-enabled sigil-host.service)"
original_main_pid="$(systemctl --user show -p MainPID --value sigil-host.service)"
[[ "$original_main_pid" =~ ^[0-9]+$ && "$original_main_pid" -gt 1 ]]
sha256sum "$real_config" >"$private/original-config.sha256"
readlink "$HOME/.local/libexec/sigil-spark/current" >"$private/original-current-link"
stat -c '%i:%s:%Y:%a:%U:%G' "$real_identity" >"$private/original-identity.stat"
sha256sum "$real_identity" >"$private/original-identity.sha256"

[[ -f "$archive" && -f "$archive.sha256" ]]
[[ -f "$incoming/release-manifest.json" && -f "$incoming/uat-provenance.txt" ]]
(
  cd "$incoming"
  sha256sum -c "$(basename -- "$archive.sha256")"
)
grep -Fxq "source_commit=$uat_commit" "$incoming/uat-provenance.txt"
grep -Fxq "workflow_run_id=$workflow_run_id" "$incoming/uat-provenance.txt"

tar -tzf "$archive" >"$private/archive-members.txt"
if grep -Eq '(^/|(^|/)\.\.(/|$))' "$private/archive-members.txt"; then
  echo "archive contains an unsafe member" >&2
  exit 1
fi
tar --no-same-owner -xzf "$archive" -C "$payload_root"
(
  cd "$payload_root/payload"
  sha256sum -c PACKAGE-SHA256SUMS
)
(
  cd "$payload_root/payload/release"
  sha256sum -c SHA256SUMS
)

sigil="$payload_root/payload/release/sigil"
probe="$payload_root/payload/release/sigil-probe"
manifest="$payload_root/payload/release/release-manifest.json"
cmp "$manifest" "$incoming/release-manifest.json"
jq -e --arg commit "$uat_commit" '
  .git_commit == $commit
  and .git_dirty == false
  and .binary_provenance == "self-built-clean-head"
  and .binary_provenance_verified == true
  and .release_kind == "development"
  and .release_tag == "development"
  and .target == "x86_64-unknown-linux-gnu.2.17"
  and .features == ["default", "in-process-gstreamer"]
  and .demo_direct_node == false
' "$manifest" >/dev/null
[[ -x "$sigil" && -x "$probe" ]]

readelf -d "$sigil" >"$private/sigil.dynamic.txt"
if grep -Eq '\((RPATH|RUNPATH)\)' "$private/sigil.dynamic.txt"; then
  echo "candidate contains an unexpected runtime library path" >&2
  exit 1
fi
for soname in \
  libgstreamer-1.0.so \
  libgstapp-1.0.so \
  libgstbase-1.0.so \
  libgstvideo-1.0.so \
  libgobject-2.0.so \
  libglib-2.0.so
do
  grep -Fq "Shared library: [$soname" "$private/sigil.dynamic.txt"
done
readelf --version-info "$sigil" >"$private/sigil.versions.txt"
max_glibc="$({
  sed -n 's/.*Name: GLIBC_\([0-9][0-9.]*\).*/\1/p' "$private/sigil.versions.txt"
  echo 0
} | sort -V | tail -n 1)"
[[ "$(printf '%s\n' "$max_glibc" 2.17 | sort -V | tail -n 1)" == 2.17 ]] || {
  echo "candidate requires GLIBC_$max_glibc, above 2.17" >&2
  exit 1
}
ldd "$sigil" >"$private/sigil.ldd"
ldd "$probe" >"$private/sigil-probe.ldd"
if grep -q 'not found' "$private/sigil.ldd"; then
  echo "candidate host has an unresolved dynamic library" >&2
  exit 1
fi
if grep -q 'not found' "$private/sigil-probe.ldd"; then
  echo "candidate probe has an unresolved dynamic library" >&2
  exit 1
fi
timeout --signal=TERM --kill-after=2s 5s "$sigil" --version
timeout --signal=TERM --kill-after=2s 5s "$probe" --version

host_identity="$private/host.key"
probe_identity="$private/probe.key"
state="$private/state"
external_cqp_state="$private/external-cqp-state"
fixed_config="$private/host-1280x800.toml"
external_cqp_config="$private/host-1280x800-external-cqp.toml"
native_config="$private/host-native.toml"
invitation="$private/probe.goq-invite"

"$sigil" identity init --output "$host_identity" >"$private/host-identity.log"
"$sigil" identity init --output "$probe_identity" >"$private/probe-identity.log"
host_node_id="$(sed -n 's/^node_id=//p' "$private/host-identity.log")"
probe_node_id="$(sed -n 's/^node_id=//p' "$private/probe-identity.log")"
[[ -n "$host_node_id" && -n "$probe_node_id" ]]

pw_dump="$(command -v pw-dump)"
pw_link="$(command -v pw-link)"
xprop="$(command -v xprop)"
gst_launch="$(command -v gst-launch-1.0)"
gst_inspect="$(command -v gst-inspect-1.0)"
timeout_command="$(command -v timeout)"
ffmpeg="$(command -v ffmpeg)"
[[ -S "$XDG_RUNTIME_DIR/pipewire-0" ]]
selection_status=0
select_amd_gstva_h264_encoder \
  "$gst_inspect" /sys/class/drm /dev/dri true \
  "$render_node_override" "$va_encoder_override" \
  "$timeout_command" "$private" || selection_status=$?
if [[ "$selection_status" -ne 0 ]]; then
  if [[ -n "$render_node_override" ]]; then
    echo "the requested render-node and VA-encoder pair is not eligible for this UAT" >&2
  elif [[ "$selection_status" -eq 2 ]]; then
    echo "multiple AMD GstVA H.264 encoder pairs are eligible; select one exact pair with --render-node and --va-encoder" >&2
  else
    echo "no accessible AMD render node has a GstVA H.264 factory satisfying the CBR and CQP UAT contract" >&2
  fi
  if [[ -n "$va_candidates" ]]; then
    while IFS= read -r pair; do
      printf 'eligible_pair=%s\n' "$pair" >&2
    done <<<"$va_candidates"
  fi
  exit 1
fi
if [[ -n "$render_node_override" ]]; then
  vaapi_selection_mode=explicit
else
  vaapi_selection_mode=automatic-unique
fi
{
  echo "vaapi_render_node=$render_node"
  echo "vaapi_encoder=$va_encoder"
  echo "vaapi_selection_mode=$vaapi_selection_mode"
} >"$private/vaapi-selection.env"

cat >"$fixed_config" <<EOF
identity_path = "$host_identity"
state_path = "$state"
source = "gamescope-pipewire"
width = 1280
height = 800
framerate = 60
codec = "h264"
input_mode = "uinput"
ffmpeg_path = "$ffmpeg"

$uinput_block

[gamescope_pipewire]
node_name = "gamescope"
media_class = "Video/Source"
xwayland_display = ":0"
pw_dump_path = "$pw_dump"
gst_launch_path = "$gst_launch"
gst_inspect_path = "$gst_inspect"
encoder_backend = "in-process-gstreamer"
vaapi_encoder = "$va_encoder"
vaapi_render_node = "$render_node"
rate_control = "cbr"
bitrate_kbps = 12000
EOF

cat >"$native_config" <<EOF
identity_path = "$host_identity"
state_path = "$state"
source = "gamescope-pipewire"
framerate = 60
codec = "h264"
input_mode = "uinput"
ffmpeg_path = "$ffmpeg"

$uinput_block

[gamescope_pipewire]
node_name = "gamescope"
media_class = "Video/Source"
xwayland_display = ":0"
pw_dump_path = "$pw_dump"
gst_launch_path = "$gst_launch"
gst_inspect_path = "$gst_inspect"
encoder_backend = "in-process-gstreamer"
vaapi_encoder = "$va_encoder"
vaapi_render_node = "$render_node"
rate_control = "cbr"
bitrate_kbps = 12000
EOF

cat >"$external_cqp_config" <<EOF
identity_path = "$host_identity"
state_path = "$external_cqp_state"
source = "gamescope-pipewire"
width = 1280
height = 800
framerate = 60
codec = "h264"
input_mode = "log"
ffmpeg_path = "$ffmpeg"

[gamescope_pipewire]
node_name = "gamescope"
media_class = "Video/Source"
xwayland_display = ":0"
pw_dump_path = "$pw_dump"
gst_launch_path = "$gst_launch"
gst_inspect_path = "$gst_inspect"
encoder_backend = "external-gst-launch"
vaapi_encoder = "$va_encoder"
vaapi_render_node = "$render_node"
rate_control = "cqp"
quantizer = 24
EOF
chmod 0600 "$fixed_config" "$external_cqp_config" "$native_config"

"$sigil" config check --config "$fixed_config" >"$private/fixed-config-check.log"
"$sigil" config check --config "$external_cqp_config" \
  >"$private/external-cqp-config-check.log"
"$sigil" config check --config "$native_config" >"$private/native-config-check.log"
grep -Fxq config=ok "$private/fixed-config-check.log"
grep -Fxq capture_preflight=ok "$private/fixed-config-check.log"
grep -Fxq encoded_mode=1280x800@60 "$private/fixed-config-check.log"
grep -Fxq config=ok "$private/external-cqp-config-check.log"
grep -Fxq capture_preflight=ok "$private/external-cqp-config-check.log"
grep -Fxq encoded_mode=1280x800@60 "$private/external-cqp-config-check.log"
grep -Fxq config=ok "$private/native-config-check.log"
grep -Fxq capture_preflight=ok "$private/native-config-check.log"
native_size="$(sed -n 's/^encoded_mode=\([0-9][0-9]*x[0-9][0-9]*\)@60$/\1/p' "$private/native-config-check.log")"
[[ -n "$native_size" ]]
[[ "$native_size" != 1280x800 ]] || {
  echo "native UAT resolved to the fixed 1280x800 mode" >&2
  exit 1
}

"$sigil" invitation create \
  --config "$fixed_config" \
  --peer "$probe_node_id" \
  --expires-in-seconds 900 \
  --pointer-keyboard \
  --output "$invitation" \
  >"$private/invitation-create.log"
[[ "$(stat -c %a "$invitation")" == 600 ]]

restore_required=false
capture_probe_pid=''
rollback_units=()
rollback_helper="$private/rollback-original.sh"
fixed_candidate_unit="goq-uat-fixed-candidate-$invocation_id"
native_candidate_unit="goq-uat-native-candidate-$invocation_id"
candidate_units=("$fixed_candidate_unit" "$native_candidate_unit")
active_candidate_unit=""

cat >"$rollback_helper" <<EOF
#!/usr/bin/env bash
set -u
XDG_RUNTIME_DIR='$XDG_RUNTIME_DIR'
export XDG_RUNTIME_DIR
real_sigil='$real_sigil'
real_config='$real_config'
for candidate_unit in '$fixed_candidate_unit' '$native_candidate_unit'; do
  systemctl --user stop "\$candidate_unit.service" >/dev/null 2>&1 || true
  systemctl --user reset-failed "\$candidate_unit.service" >/dev/null 2>&1 || true
done
original_ready() {
  local invocation pid
  systemctl --user is-active --quiet sigil-host.service || return 1
  pid="\$(systemctl --user show -p MainPID --value sigil-host.service)"
  invocation="\$(systemctl --user show -p InvocationID --value sigil-host.service)"
  [[ "\$pid" =~ ^[0-9]+\$ && "\$pid" -gt 1 ]] || return 1
  [[ "\$invocation" =~ ^[0-9a-f]{32}\$ ]] || return 1
  cmp -s "\$real_sigil" "/proc/\$pid/exe" || return 1
  journalctl --user _SYSTEMD_INVOCATION_ID="\$invocation" \
    _SYSTEMD_USER_UNIT=sigil-host.service _PID="\$pid" --no-pager -o cat \
    | grep -Fx status=ready >/dev/null
}
for _ in \$(seq 1 60); do
  systemctl --user reset-failed sigil-host.service >/dev/null 2>&1 || true
  systemctl --user start sigil-host.service >/dev/null 2>&1 || true
  sleep 5
  if systemctl --user is-active --quiet sigil-host.service && original_ready; then
    exit 0
  fi
done
exit 1
EOF
chmod 0700 "$rollback_helper"

stop_candidate() {
  local candidate_unit
  for candidate_unit in "${candidate_units[@]}"; do
    systemctl --user stop "$candidate_unit.service" >/dev/null 2>&1 || true
    systemctl --user reset-failed "$candidate_unit.service" >/dev/null 2>&1 || true
  done
  active_candidate_unit=""
}

disarm_rollbacks() {
  local unit
  for unit in "${rollback_units[@]}"; do
    systemctl --user stop "$unit.timer" "$unit.service" 2>/dev/null || true
  done
}

restore_original() {
  stop_candidate
  if $restore_required || ! systemctl --user is-active --quiet sigil-host.service; then
    systemctl --user reset-failed sigil-host.service || true
    systemctl --user start sigil-host.service || return 1
    for _ in $(seq 1 60); do
      if systemctl --user is-active --quiet sigil-host.service && original_ready; then
        restore_required=false
        disarm_rollbacks
        return 0
      fi
      sleep 0.5
    done
    return 1
  fi
  disarm_rollbacks
}

cleanup() {
  local status=$?
  trap - EXIT
  trap '' INT TERM HUP
  if [[ -n "$capture_probe_pid" ]] && kill -0 "$capture_probe_pid" 2>/dev/null; then
    kill "$capture_probe_pid" 2>/dev/null || true
    wait "$capture_probe_pid" 2>/dev/null || true
    capture_probe_pid=''
  fi
  if ! restore_original; then
    echo "CRITICAL: original Sigil service did not return ready; rollback timer remains armed" >&2
    status=1
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

arm_rollback() {
  local unit="$1"
  systemd-run --user --unit="$unit" --on-active=20m "$rollback_helper" >/dev/null
  systemctl --user is-active --quiet "$unit.timer"
  rollback_units+=("$unit")
}

start_candidate() {
  local mode="$1"
  local config="$2"
  host_log="$raw/$mode-host.log"
  case "$mode" in
    fixed) active_candidate_unit="$fixed_candidate_unit" ;;
    native) active_candidate_unit="$native_candidate_unit" ;;
    *) echo "unknown candidate mode: $mode" >&2; return 1 ;;
  esac
  : >"$host_log"
  systemd-run --user --quiet --collect \
    --unit="$active_candidate_unit" \
    --service-type=exec \
    --property="StandardOutput=append:$host_log" \
    --property="StandardError=append:$host_log" \
    --property=TimeoutStopSec=10s \
    --setenv=RUST_LOG=info,sigil::server=debug \
    "$sigil" serve --config "$config" --max-runtime-seconds 1080
  for _ in $(seq 1 300); do
    grep -Fxq status=ready "$host_log" 2>/dev/null && break
    systemctl --user is-active --quiet "$active_candidate_unit.service"
    sleep 0.1
  done
  grep -Fxq status=ready "$host_log"
  grep -Fxq "node_id=$host_node_id" "$host_log"
}

validate_probe() {
  local log="$1"
  local transport="$2"
  local size="$3"
  local request_id="$4"
  local slow_consumer_ms="${5:-}"
  local expected_path_mode="${6:-direct}"
  grep -Fxq probe=ok "$log"
  grep -Fxq "dimensions=$size" "$log"
  grep -Fxq "transport=$transport" "$log"
  grep -Fxq sequence_gaps=0 "$log"
  grep -Fxq first_configured_idr=ok "$log"
  grep -Fxq frame_sequence=monotonic "$log"
  grep -Fxq recovery_barrier=configured-idr "$log"
  grep -Fxq keyframe_recovery=ok "$log"
  grep -Fxq "keyframe_request_id=$request_id" "$log"
  grep -Fxq "path_mode=$expected_path_mode" "$log"
  grep -Fxq "media_path_mode=$expected_path_mode" "$log"
  grep -Fxq "input_path_mode=$expected_path_mode" "$log"
  grep -Fxq "forced_relay=$([[ "$expected_path_mode" == relay ]] && printf ok || printf not-requested)" "$log"
  awk -F= '$1 == "keyframe_recovery_micros" && $2 ~ /^[0-9]+$/ && $2 <= 2000000 { ok=1 } END { exit !ok }' "$log"
  awk -F= '$1 == "input_ack_micros" && $2 ~ /^[0-9]+$/ { ok=1 } END { exit !ok }' "$log"
  if [[ "$transport" == iroh-moq ]]; then
    grep -Fxq "control_path_mode=$expected_path_mode" "$log"
    grep -Fxq control_alpn=sigil/control/1 "$log"
    grep -Fxq moq_group_capacity=1 "$log"
    grep -Fxq moq_unrecovered_group_gaps=0 "$log"
    grep -Fxq moq_historical_suffix_frames=0 "$log"
  fi
  if [[ -n "$slow_consumer_ms" ]]; then
    grep -Fxq slow_consumer=ok "$log"
    grep -Fxq "slow_consumer_stall_ms=$slow_consumer_ms" "$log"
    grep -Fxq slow_consumer_first_post_stall=configured-idr "$log"
    grep -Fxq slow_consumer_historical_suffix_frames=0 "$log"
    awk -F= '$1 == "slow_consumer_recovery_micros" && $2 ~ /^[0-9]+$/ && $2 <= 2000000 { ok=1 } END { exit !ok }' "$log"
    awk -F= '$1 == "slow_consumer_cancellation_delta" && $2 ~ /^[0-9]+$/ && $2 > 0 { ok=1 } END { exit !ok }' "$log"
    awk -F= '$1 == "slow_consumer_group_advance" && $2 ~ /^[0-9]+$/ && $2 > 0 { ok=1 } END { exit !ok }' "$log"
    awk -F= '$1 == "slow_consumer_sequence_advance" && $2 ~ /^[0-9]+$/ && $2 > 0 { ok=1 } END { exit !ok }' "$log"
    awk -F= -v minimum="$((slow_consumer_ms * 500))" '$1 == "slow_consumer_capture_advance_micros" && $2 ~ /^[0-9]+$/ && $2 >= minimum { ok=1 } END { exit !ok }' "$log"
    awk -F= '$1 == "slow_consumer_input_ack_micros" && $2 ~ /^[0-9]+$/ && $2 <= 2000000 { ok=1 } END { exit !ok }' "$log"
  else
    grep -Fxq slow_consumer=not-requested "$log"
  fi
}

wait_for_count() {
  local log="$1"
  local text="$2"
  local expected="$3"
  local deadline=$((SECONDS + 20))
  local count
  while ((SECONDS < deadline)); do
    count="$(grep -Fc -- "$text" "$log" 2>/dev/null || true)"
    [[ "$count" -ge "$expected" ]] && return 0
    [[ -n "$active_candidate_unit" ]] \
      && systemctl --user is-active --quiet "$active_candidate_unit.service" \
      || return 1
    sleep 0.1
  done
  return 1
}

run_probe_cycle() {
  local mode="$1"
  local transport="$2"
  local size="$3"
  local request_id="$4"
  local invitation_path="${5:-}"
  local slow_consumer_ms="${6:-}"
  local expected_path_mode="${7:-direct}"
  local pointer_feedback_smoke="${8:-false}"
  local log="$raw/$mode-$transport-$request_id.log"
  local command=(
    "$probe"
    --node-id "$host_node_id"
    --identity "$probe_identity"
    --frames 30
    --timeout-seconds 20
    --expect-size "$size"
    --keyframe-smoke
    --keyframe-request-id "$request_id"
  )
  [[ "$transport" == grouped-v3 ]] && command+=(--media-v3)
  [[ -n "$invitation_path" ]] && command+=(--invitation "$invitation_path")
  [[ -n "$slow_consumer_ms" ]] && command+=(--slow-consumer-ms "$slow_consumer_ms")
  [[ "$expected_path_mode" != relay ]] || command+=(--relay-only)
  if [[ "$pointer_feedback_smoke" == true ]]; then
    command+=(--pointer-smoke --pointer-feedback-smoke)
  fi
  timeout --signal=TERM --kill-after=5s 45s "${command[@]}" >"$log" 2>&1
  validate_probe "$log" "$transport" "$size" "$request_id" "$slow_consumer_ms" "$expected_path_mode"
  if [[ "$pointer_feedback_smoke" == true ]]; then
    grep -Fxq pointer_smoke=ok "$log"
    grep -Fxq pointer_feedback_smoke=ok "$log"
  fi
}

validate_host_recovery() {
  local log="$1"
  local plain
  plain="$private/$(basename -- "$log").plain"
  wait_for_count "$log" 'MoQ control client released' 10
  wait_for_count "$log" 'media v3 client released' 10
  wait_for_count "$log" 'input client released' 20
  sed -E $'s/\x1B\\[[0-9;]*[[:alpha:]]//g' "$log" >"$plain"
  [[ "$(grep -Fc 'MoQ control client released' "$plain")" -eq 10 ]]
  [[ "$(grep -Fc 'media v3 client released' "$plain")" -eq 10 ]]
  [[ "$(grep -Fc 'input client released' "$plain")" -eq 20 ]]
  [[ "$(grep -Fc 'forced-IDR recovery acknowledged' "$plain")" -eq 20 ]]
  [[ "$(grep -Fc 'forced_idr_disposition=Requested' "$plain")" -eq 20 ]]
  [[ "$(grep -Fc 'coalesced=false' "$plain")" -ge 20 ]]
  ! grep -Eq 'forced-IDR request failed|forced-IDR recovery was not acknowledged|forced-IDR acknowledgement task failed|forced-IDR acknowledgement task ended without a result' "$plain"
}

capture_inventory() {
  local phase="$1"
  ps -u "$(id -u)" -o pid=,comm=,args= >"$evidence/$phase-processes.txt"
  awk -v own_pid="$$" '
    $1 == own_pid { next }
    ($2 == "sigil" || $2 == "sigil-host") &&
      ($0 ~ / serve( |$)/ || $0 ~ / capture probe( |$)/) { print; next }
    $2 ~ /^gst-launch/ && $0 ~ /pipewiresrc/ { print }
  ' "$evidence/$phase-processes.txt" >"$evidence/$phase-conflicts.txt"
  if [[ -s "$evidence/$phase-conflicts.txt" ]]; then
    echo "a Sigil or GStreamer PipeWire capture process survived $phase" >&2
    cat "$evidence/$phase-conflicts.txt" >&2
    return 1
  fi
  "$pw_link" -l >"$evidence/$phase-pipewire-links.txt"
  if grep -Fiq gamescope "$evidence/$phase-pipewire-links.txt"; then
    echo "a Gamescope PipeWire consumer link survived $phase" >&2
    cat "$evidence/$phase-pipewire-links.txt" >&2
    return 1
  fi
  "$pw_dump" >"$evidence/$phase-pipewire.json"
}

validate_capture_log() {
  local log="$1"
  grep -Fxq probe=ok "$log"
  grep -Fxq dropped_after_encode_before_probe_consumer=0 "$log"
}

run_capture_probe() {
  local name="$1"
  local config="$2"
  local size="$3"
  local minimum_fps="${4:-}"
  local command=(
    "$sigil" capture probe --source gamescope-pipewire
    --config "$config" --frames 300 --expect-size "$size"
  )
  [[ -z "$minimum_fps" ]] || command+=(--minimum-fps "$minimum_fps")
  capture_inventory "pre-$name"
  timeout --signal=TERM --kill-after=5s 90s "${command[@]}" \
    >"$evidence/$name-capture.log"
  validate_capture_log "$evidence/$name-capture.log"
  capture_inventory "post-$name"
}

fixed_timer="goq-uat-fixed-$invocation_id"
arm_rollback "$fixed_timer"
restore_required=true
systemctl --user stop sigil-host.service
if systemctl --user is-active --quiet sigil-host.service; then
  echo "original Sigil service did not stop" >&2
  exit 1
fi
[[ "$(systemctl --user show -p MainPID --value sigil-host.service)" -eq 0 ]]

run_capture_probe fixed "$fixed_config" 1280x800 55

capture_inventory pre-external-cqp
timeout --signal=TERM --kill-after=5s 90s \
  "$sigil" capture probe --source gamescope-pipewire \
    --config "$external_cqp_config" --frames 300 --expect-size 1280x800 \
    --minimum-fps 55 >"$evidence/external-cqp-capture.log" &
capture_probe_pid=$!
external_link_ready=false
for _ in $(seq 1 200); do
  if ! kill -0 "$capture_probe_pid" 2>/dev/null; then
    break
  fi
  if "$pw_link" -l | grep -Fiq gamescope; then
    external_link_ready=true
    break
  fi
  sleep 0.05
done
if [[ "$external_link_ready" != true ]]; then
  echo "external CQP probe never opened a Gamescope PipeWire consumer link" >&2
  exit 1
fi
if timeout --signal=TERM --kill-after=2s 10s \
  "$sigil" capture probe --source gamescope-pipewire \
    --config "$fixed_config" --frames 1 --expect-size 1280x800 \
    >"$evidence/contended-capture.out" 2>"$evidence/contended-capture.error"; then
  echo "a second Gamescope capture probe overlapped external CQP capture" >&2
  exit 1
fi
grep -Fq 'another Sigil daemon or capture probe already owns this lifecycle scope' \
  "$evidence/contended-capture.error"
wait "$capture_probe_pid"
capture_probe_pid=''
validate_capture_log "$evidence/external-cqp-capture.log"
capture_inventory post-external-cqp

start_candidate fixed "$fixed_config"
run_probe_cycle fixed iroh-moq 1280x800 1001 "$invitation" 1500 relay
wait_for_count "$raw/fixed-host.log" 'MoQ control client released' 1
wait_for_count "$raw/fixed-host.log" 'input client released' 1
enrollment="$("$sigil" enrollment show --config "$fixed_config")"
grep -Fxq enrollment=active <<<"$enrollment"
grep -Fxq "peer_node_id=$probe_node_id" <<<"$enrollment"
grep -Fxq grants=view,pointer-keyboard <<<"$enrollment"
if timeout --signal=TERM --kill-after=5s 20s \
  "$probe" --node-id "$host_node_id" --identity "$probe_identity" \
    --invitation "$invitation" --frames 4 >"$raw/invitation-replay.log" 2>&1; then
  echo "redeemed invitation replay was accepted" >&2
  exit 1
fi
grep -Fq 'host rejected control stream: Portal peer is not authorized' \
  "$raw/invitation-replay.log"
mv "$invitation" "$private/redeemed-probe.goq-invite"
for cycle in $(seq 2 10); do
  run_probe_cycle fixed iroh-moq 1280x800 "$((1000 + cycle))"
done
for cycle in $(seq 1 10); do
  run_probe_cycle fixed grouped-v3 1280x800 "$((1100 + cycle))"
done
validate_host_recovery "$raw/fixed-host.log"

fixed_candidate_pid="$(systemctl --user show -p MainPID --value \
  "$fixed_candidate_unit.service")"
[[ "$fixed_candidate_pid" =~ ^[0-9]+$ && "$fixed_candidate_pid" -gt 1 ]]
pointer_reconnects_before="$(grep -Fc \
  'Gamescope Xwayland pointer feedback reconnected' "$raw/fixed-host.log" || true)"
for cycle in 1 2; do
  systemctl --user restart gamescope-session-plus@steam.service
  gamescope_ready=false
  for _ in $(seq 1 600); do
    if systemctl --user is-active --quiet gamescope-session-plus@steam.service \
      && DISPLAY=:0 "$xprop" -root GAMESCOPE_MOUSE_FOCUS_DISPLAY >/dev/null 2>&1 \
      && "$pw_dump" | jq -e '
        .[]
        | select(.type == "PipeWire:Interface:Node")
        | select(.info.props["node.name"] == "gamescope")
        | select(.info.props["media.class"] == "Video/Source")
      ' >/dev/null
    then
      gamescope_ready=true
      break
    fi
    sleep 0.1
  done
  [[ "$gamescope_ready" == true ]]
  [[ "$(systemctl --user show -p MainPID --value \
    "$fixed_candidate_unit.service")" == "$fixed_candidate_pid" ]]
  expected_reconnects="$((pointer_reconnects_before + cycle))"
  for _ in $(seq 1 100); do
    reconnects="$(grep -Fc 'Gamescope Xwayland pointer feedback reconnected' \
      "$raw/fixed-host.log" || true)"
    [[ "$reconnects" -ge "$expected_reconnects" ]] && break
    sleep 0.1
  done
  [[ "$reconnects" -ge "$expected_reconnects" ]]
  run_probe_cycle "pointer-recovery-$cycle" iroh-moq 1280x800 \
    "$((1200 + cycle))" "" "" direct true
  sed -E $'s/\x1B\\[[0-9;]*[[:alpha:]]//g' \
    "$raw/pointer-recovery-$cycle-iroh-moq-$((1200 + cycle)).log" \
    >"$evidence/pointer-recovery-$cycle.log"
done
stop_candidate

native_timer="goq-uat-native-$invocation_id"
arm_rollback "$native_timer"
systemctl --user stop "$fixed_timer.timer" "$fixed_timer.service" 2>/dev/null || true
run_capture_probe native "$native_config" "$native_size"

start_candidate native "$native_config"
run_probe_cycle native iroh-moq "$native_size" 2001 "" 1500
for cycle in $(seq 2 10); do
  run_probe_cycle native iroh-moq "$native_size" "$((2000 + cycle))"
done
for cycle in $(seq 1 10); do
  run_probe_cycle native grouped-v3 "$native_size" "$((2100 + cycle))"
done
validate_host_recovery "$raw/native-host.log"

install -d -m 0700 "$evidence/probes"
for log in \
  "$raw"/fixed-iroh-moq-*.log \
  "$raw"/fixed-grouped-v3-*.log \
  "$raw"/native-iroh-moq-*.log \
  "$raw"/native-grouped-v3-*.log
do
  grep -vE '^session_id=' "$log" >"$evidence/probes/$(basename -- "$log")"
done

summarize_group() {
  local prefix="$1"
  local values="$private/$prefix.recovery-values"
  local files=("$evidence/probes/$prefix"-*.log)
  local raw_files=("$raw/$prefix"-*.log)
  local count unique_sessions p50_index p95_index p50 p95 maximum gaps
  [[ "${#files[@]}" -eq 10 ]]
  [[ "${#raw_files[@]}" -eq 10 ]]
  awk -F= '$1 == "keyframe_recovery_micros" { print $2 }' "${files[@]}" | sort -n >"$values"
  count="$(wc -l <"$values" | tr -d ' ')"
  [[ "$count" -eq 10 ]]
  unique_sessions="$(awk -F= '$1 == "session_id" { print $2 }' "${raw_files[@]}" | sort -u | wc -l | tr -d ' ')"
  [[ "$unique_sessions" -eq 10 ]]
  p50_index=$(((count * 50 + 99) / 100))
  p95_index=$(((count * 95 + 99) / 100))
  p50="$(sed -n "${p50_index}p" "$values")"
  p95="$(sed -n "${p95_index}p" "$values")"
  maximum="$(tail -n 1 "$values")"
  gaps="$(awk -F= '$1 == "sequence_gaps" { sum += $2 } END { print sum + 0 }' "${files[@]}")"
  [[ "$gaps" -eq 0 && "$p95" -le 2000000 ]]
  printf '%s_cycles=%s\n' "$prefix" "$count"
  printf '%s_unique_sessions=%s\n' "$prefix" "$unique_sessions"
  printf '%s_sequence_gaps=%s\n' "$prefix" "$gaps"
  printf '%s_recovery_p50_micros=%s\n' "$prefix" "$p50"
  printf '%s_recovery_p95_micros=%s\n' "$prefix" "$p95"
  printf '%s_recovery_max_micros=%s\n' "$prefix" "$maximum"
}

all_raw_probes=(
  "$raw"/fixed-iroh-moq-*.log
  "$raw"/fixed-grouped-v3-*.log
  "$raw"/native-iroh-moq-*.log
  "$raw"/native-grouped-v3-*.log
)
[[ "${#all_raw_probes[@]}" -eq 40 ]]
fixed_unique_sessions="$(awk -F= '$1 == "session_id" { print $2 }' \
  "$raw"/fixed-iroh-moq-*.log "$raw"/fixed-grouped-v3-*.log \
  | sort -u | wc -l | tr -d ' ')"
native_unique_sessions="$(awk -F= '$1 == "session_id" { print $2 }' \
  "$raw"/native-iroh-moq-*.log "$raw"/native-grouped-v3-*.log \
  | sort -u | wc -l | tr -d ' ')"
[[ "$fixed_unique_sessions" -eq 20 && "$native_unique_sessions" -eq 20 ]]
total_sessions="${#all_raw_probes[@]}"
fixed_observed_fps="$(sed -n 's/^observed_fps=//p' "$evidence/fixed-capture.log")"
external_cqp_observed_fps="$(sed -n 's/^observed_fps=//p' \
  "$evidence/external-cqp-capture.log")"
native_observed_fps="$(sed -n 's/^observed_fps=//p' "$evidence/native-capture.log")"
[[ -n "$fixed_observed_fps" && -n "$external_cqp_observed_fps" \
  && -n "$native_observed_fps" ]]
if awk -v fps="$native_observed_fps" 'BEGIN { exit !(fps >= 55) }'; then
  native_55fps_status=met
else
  native_55fps_status=below
fi

{
  echo candidate_kind=unsigned-development-hardware-uat
  echo "source_commit=$uat_commit"
  echo "workflow_run_id=$workflow_run_id"
  echo fixed_size=1280x800
  echo fixed_minimum_fps=55
  echo "fixed_observed_fps=$fixed_observed_fps"
  echo external_cqp_minimum_fps=55
  echo "external_cqp_observed_fps=$external_cqp_observed_fps"
  echo "vaapi_render_node=$render_node"
  echo "vaapi_encoder=$va_encoder"
  echo "vaapi_selection_mode=$vaapi_selection_mode"
  echo cross_state_capture_contention=rejected
  echo gamescope_pointer_restart_cycles=2
  echo gamescope_pointer_feedback_recovery=pass
  echo gamescope_pointer_daemon_pid_stable=pass
  echo "native_size=$native_size"
  echo native_performance_gate=observational
  echo "native_observed_fps=$native_observed_fps"
  echo "native_55fps_status=$native_55fps_status"
  echo persistent_authenticated_identity=pass
  echo invitation_redemption=pass
  echo invitation_replay=rejected
  echo "maximum_required_glibc=$max_glibc"
  echo session_id_scope=daemon-invocation
  echo "fixed_daemon_unique_sessions=$fixed_unique_sessions"
  echo "native_daemon_unique_sessions=$native_unique_sessions"
  echo "total_sessions=$total_sessions"
  echo slow_media_consumer=pass
  echo slow_media_consumer_resolutions=1280x800,"$native_size"
  echo forced_relay=pass
  echo forced_relay_scope=probe-control-media-input
  summarize_group fixed-iroh-moq
  summarize_group fixed-grouped-v3
  summarize_group native-iroh-moq
  summarize_group native-grouped-v3
} >"$private/summary.pending"

restore_original
[[ "$(systemctl --user is-active sigil-host.service)" == active ]]
[[ "$(systemctl --user is-enabled sigil-host.service)" == "$original_enabled" ]]
sha256sum -c "$private/original-config.sha256"
sha256sum -c "$private/original-identity.sha256"
[[ "$(readlink "$HOME/.local/libexec/sigil-spark/current")" == "$(cat "$private/original-current-link")" ]]
[[ "$(stat -c '%i:%s:%Y:%a:%U:%G' "$real_identity")" == "$(cat "$private/original-identity.stat")" ]]

{
  echo hardware_uat=pass
  cat "$private/summary.pending"
  echo original_service_restored=pass
  echo original_config_preserved=pass
  echo original_identity_preserved=pass
  echo original_release_preserved=pass
} >"$evidence/summary.env"
chmod 0600 \
  "$evidence/summary.env" \
  "$evidence"/probes/*.log \
  "$evidence"/*-capture.log \
  "$evidence"/pointer-recovery-*.log
cat "$evidence/summary.env"
