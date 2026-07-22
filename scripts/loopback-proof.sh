#!/usr/bin/env bash

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
workspace_dir="$(CDPATH='' cd -- "$script_dir/.." && pwd)"

profile="debug"
primary_frames=600
reconnect_cycles=3
reconnect_frames=30
probe_timeout_seconds=15
command_timeout_seconds=45
readiness_timeout_seconds=30
host_runtime_seconds=120
media_transport="iroh-moq"
media_alpn="moq-lite-04"
media_accept_log="authorized MoQ media attachment accepted"
media_release_log="MoQ control client released"
keyframe_request_log="accepted MoQ keyframe request"
session_gate_name="control"
keyframe_recovery="ok"
media_v3=false
media_v2=false
media_v1=false

usage() {
  cat <<'EOF'
Usage: scripts/loopback-proof.sh [options]

Build and exercise the exact sigil and sigil-probe binaries over real loopback
Iroh and upstream MoQ connections. The ordinary proof covers a configured IDR,
native group bounds and recovery, input acknowledgment, single-active-client
rejection, and clean reconnects.

Options:
  --profile debug|release       Cargo profile to build (default: debug)
  --primary-frames COUNT        Frames in the held primary session (default: 600)
  --reconnect-cycles COUNT      Fresh sessions after the primary (default: 3)
  --reconnect-frames COUNT      Frames per reconnect session (default: 30)
  --probe-timeout-seconds SEC   Probe per-operation timeout (default: 15)
  --media-v3                    Exercise custom grouped media v3 compatibility
                                instead of upstream MoQ
  --media-v2                    Exercise independent media v2 compatibility
                                instead of upstream MoQ
  --media-v1                    Exercise ordered media v1 compatibility
                                instead of upstream MoQ
  --help                        Show this help
EOF
}

die() {
  printf 'loopback proof failed: %s\n' "$*" >&2
  exit 1
}

require_positive_integer() {
  local name="$1"
  local value="$2"
  case "$value" in
    ''|*[!0-9]*) die "$name must be a positive integer" ;;
  esac
  if [[ "$value" -eq 0 ]]; then
    die "$name must be greater than zero"
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --profile)
      [[ $# -ge 2 ]] || die "--profile requires a value"
      profile="$2"
      shift 2
      ;;
    --primary-frames)
      [[ $# -ge 2 ]] || die "--primary-frames requires a value"
      primary_frames="$2"
      shift 2
      ;;
    --reconnect-cycles)
      [[ $# -ge 2 ]] || die "--reconnect-cycles requires a value"
      reconnect_cycles="$2"
      shift 2
      ;;
    --reconnect-frames)
      [[ $# -ge 2 ]] || die "--reconnect-frames requires a value"
      reconnect_frames="$2"
      shift 2
      ;;
    --probe-timeout-seconds)
      [[ $# -ge 2 ]] || die "--probe-timeout-seconds requires a value"
      probe_timeout_seconds="$2"
      shift 2
      ;;
    --media-v1)
      [[ "$media_v2" == false && "$media_v3" == false ]] \
        || die "media compatibility flags cannot be combined"
      media_transport="reliable-v1"
      media_alpn="sigil/media/1"
      media_accept_log="media client accepted"
      media_release_log="media client released"
      session_gate_name="media"
      keyframe_recovery="not-requested"
      media_v1=true
      shift
      ;;
    --media-v2)
      [[ "$media_v1" == false && "$media_v3" == false ]] \
        || die "media compatibility flags cannot be combined"
      media_transport="independent-v2"
      media_alpn="sigil/media/2"
      media_accept_log="media v2 client accepted"
      media_release_log="media v2 client released"
      session_gate_name="media"
      keyframe_recovery="not-requested"
      media_v2=true
      shift
      ;;
    --media-v3)
      [[ "$media_v1" == false && "$media_v2" == false ]] \
        || die "media compatibility flags cannot be combined"
      media_transport="grouped-v3"
      media_alpn="sigil/media/3"
      media_accept_log="media v3 client accepted"
      media_release_log="media v3 client released"
      keyframe_request_log="accepted media v3 keyframe request"
      session_gate_name="media"
      media_v3=true
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *) die "unknown argument: $1" ;;
  esac
done

case "$profile" in
  debug)
    cargo_profile="dev"
    profile_dir="debug"
    ;;
  release)
    cargo_profile="release"
    profile_dir="release"
    ;;
  *) die "--profile must be debug or release" ;;
esac
require_positive_integer "--primary-frames" "$primary_frames"
require_positive_integer "--reconnect-cycles" "$reconnect_cycles"
require_positive_integer "--reconnect-frames" "$reconnect_frames"
require_positive_integer "--probe-timeout-seconds" "$probe_timeout_seconds"
if [[ "$media_v1" == false && "$media_v2" == false ]]; then
  [[ "$primary_frames" -ge 4 ]] \
    || die "--primary-frames must be at least 4 for keyframe recovery"
  [[ "$reconnect_frames" -ge 4 ]] \
    || die "--reconnect-frames must be at least 4 for keyframe recovery"
fi

# A fresh Iroh endpoint performs relay discovery and path establishment on
# every reconnect. Keep the default three-cycle proof fast, but scale the host
# watchdog for deliberate soak runs such as --reconnect-cycles 100. The bound
# remains finite and deliberately conservative at eight seconds per cycle.
estimated_primary_seconds=$(( (primary_frames + 59) / 60 ))
scaled_host_runtime_seconds=$((120 + estimated_primary_seconds + reconnect_cycles * 8))
if [[ "$scaled_host_runtime_seconds" -gt "$host_runtime_seconds" ]]; then
  host_runtime_seconds="$scaled_host_runtime_seconds"
fi

if [[ -f "${HOME}/.cargo/env" ]]; then
  # The project pins its Rust toolchain, but Cargo may not yet be on PATH in a
  # non-interactive shell.
  # shellcheck source=/dev/null
  source "${HOME}/.cargo/env"
fi

command -v cargo >/dev/null 2>&1 || die "cargo is required"
ffmpeg_bin="$(command -v ffmpeg || true)"
[[ -n "$ffmpeg_bin" ]] || die "ffmpeg is required"
if ! "$ffmpeg_bin" -hide_banner -encoders 2>/dev/null | grep -q '[[:space:]]libx264[[:space:]]'; then
  die "ffmpeg does not provide the required libx264 encoder"
fi

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/sigil-loopback.XXXXXX")"
case "$tmp_root" in
  */sigil-loopback.??????) ;;
  *) die "mktemp returned an unexpected path: $tmp_root" ;;
esac

host_pid=""
host_watchdog_pid=""
primary_pid=""
primary_watchdog_pid=""
bounded_pid=""
bounded_watchdog_pid=""
watchdog_pid=""

stop_pid() {
  local pid="$1"
  local killer_pid
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid" 2>/dev/null || true
    (
      for _ in {1..50}; do
        if ! kill -0 "$pid" 2>/dev/null; then
          exit 0
        fi
        sleep 0.1
      done
      kill -KILL "$pid" 2>/dev/null || true
    ) >/dev/null 2>&1 &
    killer_pid=$!
  else
    killer_pid=""
  fi
  if [[ -n "$pid" ]]; then
    wait "$pid" 2>/dev/null || true
  fi
  if [[ -n "$killer_pid" ]]; then
    wait "$killer_pid" 2>/dev/null || true
  fi
}

cleanup() {
  local exit_status=$?
  trap - EXIT INT TERM HUP

  stop_pid "$bounded_pid"
  [[ -z "$bounded_watchdog_pid" ]] || wait "$bounded_watchdog_pid" 2>/dev/null || true
  stop_pid "$primary_pid"
  [[ -z "$primary_watchdog_pid" ]] || wait "$primary_watchdog_pid" 2>/dev/null || true
  stop_pid "$host_pid"
  [[ -z "$host_watchdog_pid" ]] || wait "$host_watchdog_pid" 2>/dev/null || true

  case "$tmp_root" in
    */sigil-loopback.??????) rm -rf -- "$tmp_root" ;;
  esac
  exit "$exit_status"
}
trap cleanup EXIT INT TERM HUP

sha256_file() {
  local path="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{print $1}'
  else
    shasum -a 256 "$path" | awk '{print $1}'
  fi
}

wait_for_log_line() {
  local log_path="$1"
  local literal="$2"
  local watched_pid="$3"
  local label="$4"
  local deadline=$((SECONDS + readiness_timeout_seconds))

  while (( SECONDS < deadline )); do
    if grep -Fq -- "$literal" "$log_path" 2>/dev/null; then
      return 0
    fi
    if ! kill -0 "$watched_pid" 2>/dev/null; then
      printf '%s exited before %s readiness. Log follows:\n' "$label" "$literal" >&2
      sed -n '1,240p' "$log_path" >&2 || true
      return 1
    fi
    sleep 0.1
  done
  printf 'timed out waiting for %s readiness marker %s. Log follows:\n' "$label" "$literal" >&2
  sed -n '1,240p' "$log_path" >&2 || true
  return 1
}

wait_for_log_count() {
  local log_path="$1"
  local literal="$2"
  local expected_count="$3"
  local watched_pid="$4"
  local label="$5"
  local deadline=$((SECONDS + readiness_timeout_seconds))
  local actual_count

  while (( SECONDS < deadline )); do
    actual_count="$(grep -Fc -- "$literal" "$log_path" 2>/dev/null || true)"
    if [[ "$actual_count" -ge "$expected_count" ]]; then
      return 0
    fi
    if ! kill -0 "$watched_pid" 2>/dev/null; then
      printf '%s exited while waiting for %s occurrence %s. Log follows:\n' \
        "$label" "$literal" "$expected_count" >&2
      sed -n '1,240p' "$log_path" >&2 || true
      return 1
    fi
    sleep 0.1
  done
  printf 'timed out waiting for %s occurrence %s in %s. Log follows:\n' \
    "$literal" "$expected_count" "$label" >&2
  sed -n '1,240p' "$log_path" >&2 || true
  return 1
}

host_accepted_keyframe_request() {
  local log_path="$1"
  local request_id="$2"
  sed $'s/\033\\[[0-9;]*m//g' "$log_path" 2>/dev/null \
    | awk -v request_id="$request_id" -v request_log="$keyframe_request_log" '
    index($0, request_log) \
      && $0 ~ ("request_id=" request_id "([^[:digit:]]|$)") \
      && index($0, "coalesced=false") { found=1 }
    END { exit !found }
  '
}

host_keyframe_request_count() {
  local log_path="$1"
  local request_id="$2"
  sed $'s/\033\\[[0-9;]*m//g' "$log_path" 2>/dev/null \
    | awk -v request_id="$request_id" -v request_log="$keyframe_request_log" '
    index($0, request_log) \
      && $0 ~ ("request_id=" request_id "([^[:digit:]]|$)") \
      && index($0, "coalesced=false") { count++ }
    END { print count + 0 }
  '
}

wait_for_keyframe_request() {
  local log_path="$1"
  local request_id="$2"
  local watched_pid="$3"
  local label="$4"
  local deadline=$((SECONDS + readiness_timeout_seconds))

  while (( SECONDS < deadline )); do
    if host_accepted_keyframe_request "$log_path" "$request_id"; then
      return 0
    fi
    if ! kill -0 "$watched_pid" 2>/dev/null; then
      printf '%s exited before accepting keyframe request %s. Log follows:\n' \
        "$label" "$request_id" >&2
      sed -n '1,240p' "$log_path" >&2 || true
      return 1
    fi
    sleep 0.1
  done
  printf 'timed out waiting for %s to accept keyframe request %s. Log follows:\n' \
    "$label" "$request_id" >&2
  sed -n '1,240p' "$log_path" >&2 || true
  return 1
}

start_watchdog() {
  local pid="$1"
  local seconds="$2"
  local timeout_marker="$3"
  (
    local deadline=$((SECONDS + seconds))
    while kill -0 "$pid" 2>/dev/null; do
      if (( SECONDS >= deadline )); then
        : > "$timeout_marker"
        kill -TERM "$pid" 2>/dev/null || true
        for _ in {1..20}; do
          if ! kill -0 "$pid" 2>/dev/null; then
            exit 0
          fi
          sleep 0.1
        done
        kill -KILL "$pid" 2>/dev/null || true
        exit 0
      fi
      sleep 0.1
    done
  ) >/dev/null 2>&1 &
  watchdog_pid=$!
}

run_bounded() {
  local seconds="$1"
  local output="$2"
  shift 2
  local status

  rm -f -- "${output}.timeout"
  "$@" >"$output" 2>&1 &
  bounded_pid=$!
  start_watchdog "$bounded_pid" "$seconds" "${output}.timeout"
  bounded_watchdog_pid="$watchdog_pid"
  if wait "$bounded_pid"; then
    status=0
  else
    status=$?
  fi
  bounded_pid=""
  wait "$bounded_watchdog_pid" 2>/dev/null || true
  bounded_watchdog_pid=""
  if [[ -e "${output}.timeout" ]]; then
    return 124
  fi
  return "$status"
}

validate_probe_log() {
  local log_path="$1"
  local expected_frames="$2"
  local expected_request_id="$3"
  if [[ "$media_v1" == true || "$media_v2" == true ]]; then
    expected_request_id="not-requested"
  fi
  grep -Fxq 'probe=ok' "$log_path" || return 1
  grep -Fxq "frames=${expected_frames}" "$log_path" || return 1
  grep -Fxq 'dimensions=1280x800' "$log_path" || return 1
  grep -Fxq 'sequence_gaps=0' "$log_path" || return 1
  grep -Fxq "transport=${media_transport}" "$log_path" || return 1
  grep -Fxq "transport_alpn=${media_alpn}" "$log_path" || return 1
  grep -Fxq 'first_configured_idr=ok' "$log_path" || return 1
  grep -Fxq 'frame_sequence=monotonic' "$log_path" || return 1
  grep -Fxq "keyframe_recovery=${keyframe_recovery}" "$log_path" || return 1
  grep -Fxq "keyframe_request_id=${expected_request_id}" "$log_path" || return 1
  awk -F= '$1 == "media_objects_dropped" && $2 ~ /^[0-9]+$/ { found=1 } END { exit !found }' \
    "$log_path" || return 1
  awk -F= '$1 == "media_objects_late" && $2 ~ /^[0-9]+$/ { found=1 } END { exit !found }' \
    "$log_path" || return 1
  awk -F= '$1 == "keyframes" && $2 ~ /^[0-9]+$/ && $2 > 0 { found=1 } END { exit !found }' \
    "$log_path" || return 1
  awk -F= '$1 == "input_ack_micros" && $2 ~ /^[0-9]+$/ { found=1 } END { exit !found }' \
    "$log_path" || return 1
  grep -Fxq 'path_mode=direct' "$log_path" || return 1
  if [[ "$media_transport" == "iroh-moq" ]]; then
    grep -Fxq 'control_alpn=sigil/control/1' "$log_path" || return 1
    grep -Fxq 'group_sequence=monotonic' "$log_path" || return 1
    grep -Fxq 'moq_group_capacity=1' "$log_path" || return 1
    grep -Fxq 'moq_unrecovered_group_gaps=0' "$log_path" || return 1
    grep -Fxq 'moq_historical_suffix_frames=0' "$log_path" || return 1
    grep -Fxq 'recovery_barrier=configured-idr' "$log_path" || return 1
    awk -F= '$1 == "moq_group_gaps" && $2 ~ /^[0-9]+$/ { found=1 } END { exit !found }' \
      "$log_path" || return 1
    awk -F= '$1 == "moq_cancelled_groups" && $2 ~ /^[0-9]+$/ && $2 > 0 { found=1 } END { exit !found }' \
      "$log_path" || return 1
    awk -F= '$1 == "moq_maximum_group_objects" && $2 ~ /^[0-9]+$/ && $2 > 0 && $2 <= 256 { found=1 } END { exit !found }' \
      "$log_path" || return 1
    awk -F= '$1 == "moq_maximum_group_bytes" && $2 ~ /^[0-9]+$/ && $2 > 0 && $2 <= 33554432 { found=1 } END { exit !found }' \
      "$log_path" || return 1
  fi
}

run_probe() {
  if [[ "$media_v1" == true ]]; then
    "$probe_bin" --media-v1 "$@"
  elif [[ "$media_v2" == true ]]; then
    "$probe_bin" --media-v2 "$@"
  elif [[ "$media_v3" == true ]]; then
    "$probe_bin" --media-v3 "$@"
  else
    "$probe_bin" "$@"
  fi
}

run_proof_probe() {
  local request_id="$1"
  shift
  if [[ "$media_v1" == false && "$media_v2" == false ]]; then
    run_probe --keyframe-smoke --keyframe-request-id "$request_id" "$@"
  else
    run_probe "$@"
  fi
}

cd "$workspace_dir"
cargo build --locked --profile "$cargo_profile" -p sigil-host --bins

target_root="${CARGO_TARGET_DIR:-$workspace_dir/target}"
case "$target_root" in
  /*) ;;
  *) target_root="$workspace_dir/$target_root" ;;
esac
host_bin="$target_root/$profile_dir/sigil"
probe_bin="$target_root/$profile_dir/sigil-probe"
[[ -x "$host_bin" ]] || die "built host binary is missing: $host_bin"
[[ -x "$probe_bin" ]] || die "built probe binary is missing: $probe_bin"

identity_path="$tmp_root/host.key"
runtime_path="$tmp_root/runtime"
identity_log="$tmp_root/identity.log"
host_log="$tmp_root/host.log"
primary_log="$tmp_root/primary.log"
secondary_log="$tmp_root/secondary.log"

"$host_bin" identity init --output "$identity_path" >"$identity_log" 2>&1
node_id="$(sed -n 's/^node_id=//p' "$identity_log" | tail -n 1)"
[[ -n "$node_id" ]] || die "identity initialization did not print a node ID"

RUST_LOG='info,sigil::server=debug' "$host_bin" serve \
  --identity "$identity_path" \
  --source test-pattern \
  --state-path "$runtime_path" \
  --width 1280 \
  --height 800 \
  --framerate 60 \
  --ffmpeg "$ffmpeg_bin" \
  >"$host_log" 2>&1 &
host_pid=$!
start_watchdog "$host_pid" "$host_runtime_seconds" "$tmp_root/host.timeout"
host_watchdog_pid="$watchdog_pid"
wait_for_log_line "$host_log" 'status=ready' "$host_pid" 'sigil' || die "host did not become ready"
host_node_id="$(sed -n 's/^node_id=//p' "$host_log" | tail -n 1)"
[[ "$host_node_id" == "$node_id" ]] || die "ready host node ID does not match its identity"

run_proof_probe \
  1 \
  --node-id "$node_id" \
  --frames "$primary_frames" \
  --timeout-seconds "$probe_timeout_seconds" \
  --expect-size 1280x800 \
  >"$primary_log" 2>&1 &
primary_pid=$!
start_watchdog "$primary_pid" "$command_timeout_seconds" "$tmp_root/primary.timeout"
primary_watchdog_pid="$watchdog_pid"

wait_for_log_count "$host_log" "$media_accept_log" 1 "$primary_pid" 'primary probe' \
  || die "primary session did not become active"
if ! kill -0 "$primary_pid" 2>/dev/null; then
  die "primary probe ended before the concurrent rejection check"
fi

if run_bounded "$probe_timeout_seconds" "$secondary_log" \
  run_probe --node-id "$node_id" --frames 1 \
  --timeout-seconds "$probe_timeout_seconds" --expect-size 1280x800; then
  die "secondary client was accepted while the primary was active"
fi
if [[ -e "${secondary_log}.timeout" ]]; then
  die "secondary client timed out instead of receiving the active-client rejection"
fi
if ! grep -Fxq "Error: host rejected ${session_gate_name} stream: host already has an active client" "$secondary_log"; then
  printf 'secondary probe did not return the expected rejection. Log follows:\n' >&2
  sed -n '1,120p' "$secondary_log" >&2 || true
  die "unexpected secondary-client failure"
fi

if wait "$primary_pid"; then
  primary_status=0
else
  primary_status=$?
fi
primary_pid=""
wait "$primary_watchdog_pid" 2>/dev/null || true
primary_watchdog_pid=""
[[ ! -e "$tmp_root/primary.timeout" ]] || die "primary probe exceeded its deadline"
[[ "$primary_status" -eq 0 ]] || {
  sed -n '1,240p' "$primary_log" >&2 || true
  die "primary probe exited with status $primary_status"
}
validate_probe_log "$primary_log" "$primary_frames" 1 || {
  sed -n '1,240p' "$primary_log" >&2 || true
  die "primary probe evidence is incomplete"
}
if [[ "$media_v1" == false && "$media_v2" == false ]]; then
  wait_for_keyframe_request "$host_log" 1 "$host_pid" 'sigil' \
    || die "primary keyframe request was not accepted by the host"
fi

wait_for_log_count "$host_log" "$media_release_log" 1 "$host_pid" 'sigil' \
  || die "primary session was not released"
wait_for_log_count "$host_log" 'input client released' 1 "$host_pid" 'sigil' \
  || die "primary input session was not drained"

cycle=1
while [[ "$cycle" -le "$reconnect_cycles" ]]; do
  reconnect_log="$tmp_root/reconnect-${cycle}.log"
  reconnect_request_id=$((cycle + 1))
  if ! run_bounded "$command_timeout_seconds" "$reconnect_log" \
    run_proof_probe "$reconnect_request_id" --node-id "$node_id" --frames "$reconnect_frames" \
    --timeout-seconds "$probe_timeout_seconds" --expect-size 1280x800; then
    sed -n '1,240p' "$reconnect_log" >&2 || true
    die "reconnect cycle $cycle failed"
  fi
  validate_probe_log "$reconnect_log" "$reconnect_frames" "$reconnect_request_id" || {
    sed -n '1,240p' "$reconnect_log" >&2 || true
    die "reconnect cycle $cycle evidence is incomplete"
  }
  if [[ "$media_v1" == false && "$media_v2" == false ]]; then
    wait_for_keyframe_request "$host_log" "$reconnect_request_id" "$host_pid" 'sigil' \
      || die "reconnect cycle $cycle keyframe request was not accepted by the host"
  fi
  wait_for_log_count "$host_log" "$media_release_log" "$((cycle + 1))" "$host_pid" 'sigil' \
    || die "reconnect cycle $cycle was not released"
  wait_for_log_count "$host_log" 'input client released' "$((cycle + 1))" "$host_pid" 'sigil' \
    || die "reconnect input cycle $cycle was not drained"
  cycle=$((cycle + 1))
done

if [[ "$media_v1" == false && "$media_v2" == false ]]; then
  request_id=1
  while [[ "$request_id" -le $((reconnect_cycles + 1)) ]]; do
    request_count="$(host_keyframe_request_count "$host_log" "$request_id")"
    [[ "$request_count" -eq 1 ]] \
      || die "keyframe request $request_id was accepted $request_count times; expected exactly once"
    request_id=$((request_id + 1))
  done
fi

kill -TERM "$host_pid" 2>/dev/null || true
if wait "$host_pid"; then
  host_status=0
else
  host_status=$?
fi
host_pid=""
wait "$host_watchdog_pid" 2>/dev/null || true
host_watchdog_pid=""
[[ ! -e "$tmp_root/host.timeout" ]] || die "host exceeded its bounded runtime"
[[ "$host_status" -eq 0 ]] || {
  sed -n '1,240p' "$host_log" >&2 || true
  die "host exited with status $host_status"
}
grep -Fq 'shutdown signal received' "$host_log" || die "host did not record a graceful shutdown"

printf 'loopback_proof=ok\n'
printf 'profile=%s\n' "$profile"
printf 'host_binary=%s\n' "$host_bin"
printf 'host_sha256=%s\n' "$(sha256_file "$host_bin")"
printf 'probe_binary=%s\n' "$probe_bin"
printf 'probe_sha256=%s\n' "$(sha256_file "$probe_bin")"
printf 'node_id=%s\n' "$node_id"
printf 'primary_frames=%s\n' "$primary_frames"
grep -E '^(transport|control_alpn|transport_alpn|first_configured_idr|frame_sequence|group_sequence|keyframe_recovery|keyframe_request_id|keyframes|sequence_gaps|media_objects_dropped|media_objects_late|moq_group_capacity|moq_cancelled_groups|moq_group_gaps|moq_unrecovered_group_gaps|moq_maximum_group_objects|moq_maximum_group_bytes|moq_historical_suffix_frames|recovery_barrier|input_ack_micros|path_mode|path_rtt_ms)=' "$primary_log"
printf 'keyframe_request_correlation=%s\n' \
  "$([[ "$media_v1" == false && "$media_v2" == false ]] && printf unique || printf not-requested)"
printf 'active_client_rejection=ok\n'
printf 'reconnect_cycles=%s\n' "$reconnect_cycles"
printf 'cleanup=ok\n'
