#!/usr/bin/env bash

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/../.." && pwd -P)"
temp_parent="${TMPDIR:-/tmp}"
temp_parent="${temp_parent%/}"
temp_root="$(mktemp -d "$temp_parent/sigil-auth-probe.XXXXXX")"

host_pid=""
cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  if [[ -n "$host_pid" ]] && kill -0 "$host_pid" 2>/dev/null; then
    kill -TERM "$host_pid" 2>/dev/null || true
    wait "$host_pid" 2>/dev/null || true
  fi
  case "$temp_root" in
    "$temp_parent"/sigil-auth-probe.??????) rm -rf -- "$temp_root" ;;
  esac
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

die() {
  printf 'authenticated probe proof failed: %s\n' "$*" >&2
  exit 1
}

wait_for_ready() {
  local deadline=$((SECONDS + 30))
  while (( SECONDS < deadline )); do
    grep -Fxq 'status=ready' "$host_log" 2>/dev/null && return 0
    kill -0 "$host_pid" 2>/dev/null || return 1
    sleep 0.1
  done
  return 1
}

wait_for_log_count() {
  local literal="$1"
  local expected="$2"
  local deadline=$((SECONDS + 15))
  local observed
  while (( SECONDS < deadline )); do
    observed="$(grep -Fc -- "$literal" "$host_log" 2>/dev/null || true)"
    [[ "$observed" -ge "$expected" ]] && return 0
    kill -0 "$host_pid" 2>/dev/null || return 1
    sleep 0.1
  done
  return 1
}

cd "$repo_dir"
if [[ -f "${HOME}/.cargo/env" ]]; then
  # shellcheck source=/dev/null
  source "${HOME}/.cargo/env"
fi
cargo build --locked -p sigil-host --bins

target_root="${CARGO_TARGET_DIR:-$repo_dir/target}"
case "$target_root" in
  /*) ;;
  *) target_root="$repo_dir/$target_root" ;;
esac
host_bin="$target_root/debug/sigil"
probe_bin="$target_root/debug/sigil-probe"
ffmpeg_bin="$(command -v ffmpeg)"
[[ -x "$host_bin" && -x "$probe_bin" ]] || die 'host/probe binaries are missing'
[[ -x "$ffmpeg_bin" ]] || die 'ffmpeg is unavailable'

export XDG_RUNTIME_DIR="$temp_root/xdg"
install -d -m 0700 "$XDG_RUNTIME_DIR"
host_identity="$temp_root/host.key"
probe_identity="$temp_root/probe.key"
host_config="$temp_root/host.toml"
invitation="$temp_root/probe.goq-invite"
host_log="$temp_root/host.log"
first_log="$temp_root/first.log"
reconnect_log="$temp_root/reconnect.log"
compat_log="$temp_root/compat.log"
replay_log="$temp_root/replay.log"

"$host_bin" identity init --output "$host_identity" >"$temp_root/host-identity.log"
"$host_bin" identity init --output "$probe_identity" >"$temp_root/probe-identity.log"
host_node_id="$(sed -n 's/^node_id=//p' "$temp_root/host-identity.log")"
probe_node_id="$(sed -n 's/^node_id=//p' "$temp_root/probe-identity.log")"
[[ -n "$host_node_id" && -n "$probe_node_id" ]] || die 'identity output is incomplete'

printf '%s\n' \
  "identity_path = \"$host_identity\"" \
  "state_path = \"$temp_root/state\"" \
  'source = "test-pattern"' \
  'width = 1280' \
  'height = 800' \
  'framerate = 60' \
  'codec = "h264"' \
  'input_mode = "log"' \
  "ffmpeg_path = \"$ffmpeg_bin\"" \
  >"$host_config"
chmod 0600 "$host_config"
"$host_bin" config check --config "$host_config" >/dev/null

RUST_LOG='info,sigil::server=debug' "$host_bin" serve \
  --config "$host_config" \
  --max-runtime-seconds 90 \
  >"$host_log" 2>&1 &
host_pid=$!
wait_for_ready || {
  sed -n '1,160p' "$host_log" >&2 || true
  die 'configured host did not become ready'
}
"$host_bin" invitation create \
  --config "$host_config" \
  --peer "$probe_node_id" \
  --pointer-keyboard \
  --output "$invitation" \
  >/dev/null

"$probe_bin" \
  --node-id "$host_node_id" \
  --identity "$probe_identity" \
  --invitation "$invitation" \
  --frames 1 \
  >"$first_log"
wait_for_log_count 'MoQ control client released' 1 \
  || die 'first authenticated session did not drain'

"$probe_bin" \
  --node-id "$host_node_id" \
  --identity "$probe_identity" \
  --frames 1 \
  >"$reconnect_log"
wait_for_log_count 'MoQ control client released' 2 \
  || die 'ticket-free session did not drain'

"$probe_bin" \
  --node-id "$host_node_id" \
  --identity "$probe_identity" \
  --media-v3 \
  --frames 1 \
  >"$compat_log"
wait_for_log_count 'media v3 client released' 1 \
  || die 'grouped-v3 session did not drain'

for proof_log in "$first_log" "$reconnect_log" "$compat_log"; do
  grep -Fxq 'probe=ok' "$proof_log" || die "probe evidence is missing from $proof_log"
  grep -Fxq 'sequence_gaps=0' "$proof_log" || die "sequence gap in $proof_log"
  awk -F= '$1 == "input_ack_micros" && $2 ~ /^[0-9]+$/ { found=1 } END { exit !found }' \
    "$proof_log" || die "input acknowledgment is missing from $proof_log"
done
grep -Fxq 'transport=iroh-moq' "$first_log" || die 'first transport is not upstream MoQ'
grep -Fxq 'transport=iroh-moq' "$reconnect_log" || die 'reconnect transport is not upstream MoQ'
grep -Fxq 'transport=grouped-v3' "$compat_log" || die 'compatibility transport is not grouped-v3'

if "$probe_bin" \
  --node-id "$host_node_id" \
  --identity "$probe_identity" \
  --invitation "$invitation" \
  --frames 4 \
  >"$replay_log" 2>&1; then
  die 'redeemed invitation replay was accepted'
fi
grep -Fq 'host rejected control stream: Portal peer is not authorized' "$replay_log" \
  || {
    sed -n '1,80p' "$replay_log" >&2 || true
    die 'redeemed invitation replay returned the wrong rejection'
  }
enrollment="$($host_bin enrollment show --config "$host_config")"
grep -Fxq 'enrollment=active' <<<"$enrollment" || die 'probe enrollment is not active'
grep -Fxq "peer_node_id=$probe_node_id" <<<"$enrollment" \
  || die 'enrollment belongs to the wrong peer'
grep -Fxq 'grants=view,pointer-keyboard' <<<"$enrollment" \
  || die 'enrollment grants changed'

printf 'authenticated_probe_proof=ok\n'
printf 'invitation_redemption=ok\n'
printf 'ticket_free_reconnect=ok\n'
printf 'grouped_v3_reconnect=ok\n'
printf 'invitation_replay=rejected\n'
