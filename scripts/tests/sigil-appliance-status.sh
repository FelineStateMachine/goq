#!/usr/bin/env bash

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/../.." && pwd -P)"
sigil="${1:-$repo_dir/target/debug/sigil}"
ffmpeg="$(command -v ffmpeg || true)"

[[ -x "$sigil" ]] || {
  printf 'sigil executable is missing: %s\n' "$sigil" >&2
  exit 1
}
[[ -n "$ffmpeg" ]] || {
  printf 'ffmpeg is required for the appliance status integration proof\n' >&2
  exit 1
}

temp_parent="${TMPDIR:-/tmp}"
temp_parent="${temp_parent%/}"
temp_root="$(mktemp -d "$temp_parent/sigil-appliance-status.XXXXXX")"
host_pid=''

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  if [[ -n "$host_pid" ]] && kill -0 "$host_pid" 2>/dev/null; then
    kill "$host_pid" 2>/dev/null || true
    wait "$host_pid" 2>/dev/null || true
  fi
  case "$temp_root" in
    "$temp_parent"/sigil-appliance-status.??????) rm -rf -- "$temp_root" ;;
  esac
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

identity_directory="$temp_root/identity"
state_directory="$temp_root/state"
runtime_directory="$temp_root/runtime"
identity_path="$identity_directory/host.key"
config_path="$temp_root/host.toml"
host_log="$temp_root/host.log"
second_log="$temp_root/second.log"
live_status="$temp_root/live-status.json"
stopped_status="$temp_root/stopped-status.json"

mkdir -m 0700 "$identity_directory" "$state_directory" "$runtime_directory"
"$sigil" identity init --output "$identity_path" >"$temp_root/identity.log"
host_node_id="$(sed -n 's/^node_id=//p' "$temp_root/identity.log")"
[[ -n "$host_node_id" ]] || {
  printf 'identity initialization did not return a node ID\n' >&2
  exit 1
}

printf '%s\n' \
  "identity_path = \"$identity_path\"" \
  "state_path = \"$state_directory\"" \
  'source = "test-pattern"' \
  'framerate = 60' \
  'codec = "h264"' \
  'input_mode = "log"' \
  "ffmpeg_path = \"$ffmpeg\"" \
  >"$config_path"
chmod 0600 "$config_path"

XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" serve \
    --config "$config_path" \
    --max-runtime-seconds 4 \
    >"$host_log" 2>&1 &
host_pid=$!

ready='false'
for _ in $(seq 1 40); do
  if grep -Fq 'status=ready' "$host_log"; then
    ready='true'
    break
  fi
  if ! kill -0 "$host_pid" 2>/dev/null; then
    break
  fi
  sleep 0.5
done
if [[ "$ready" != 'true' ]]; then
  printf 'Sigil did not become ready during appliance status proof\n' >&2
  sed -n '1,160p' "$host_log" >&2
  exit 1
fi

XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" appliance status --config "$config_path" --json >"$live_status"

if XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" serve \
    --identity "$identity_path" \
    --source test-pattern \
    --state-path "$temp_root/other-state" \
    --ffmpeg "$ffmpeg" \
    --max-runtime-seconds 1 \
    >"$second_log" 2>&1; then
  printf 'a second per-user Sigil daemon unexpectedly started\n' >&2
  exit 1
fi
grep -Fq 'another Sigil daemon already owns this state directory' "$second_log"

python3 - "$live_status" "$host_node_id" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
raw = path.read_text(encoding="utf-8")
status = json.loads(raw)
host_node_id = sys.argv[2]

if status.get("schema_version") != 1:
    raise SystemExit("unexpected appliance status schema")
if status.get("overall") != "ready":
    raise SystemExit(f"unexpected live overall state: {status.get('overall')}")
if status.get("runtime", {}).get("daemon") != "ready":
    raise SystemExit("live daemon state is not ready")
if status.get("runtime", {}).get("session") != "inactive":
    raise SystemExit("live proof unexpectedly reports a session")
if status.get("enrollment", {}).get("state") != "none":
    raise SystemExit("direct proof unexpectedly reports an enrollment")
if host_node_id in raw:
    raise SystemExit("appliance status leaked the complete host node ID")
fingerprint = status.get("identity", {}).get("host_fingerprint", "")
if len(fingerprint) != 17 or "…" not in fingerprint:
    raise SystemExit("host fingerprint is not bounded and redacted")
PY

wait "$host_pid"
host_pid=''
XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" appliance status --config "$config_path" --json >"$stopped_status"

python3 - "$stopped_status" <<'PY'
import json
import pathlib
import sys

status = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
runtime = status.get("runtime", {})
if runtime.get("daemon") != "stopped":
    raise SystemExit(f"clean shutdown did not publish stopped: {runtime}")
if runtime.get("session") != "inactive":
    raise SystemExit("stopped daemon did not report an inactive session")
if runtime.get("uptime_ms") is not None:
    raise SystemExit("stopped daemon retained a live uptime")
PY

if env -u XDG_RUNTIME_DIR \
  "$sigil" serve --config "$config_path" --max-runtime-seconds 1 \
  >"$temp_root/missing-runtime.log" 2>&1; then
  printf 'configured Sigil unexpectedly started without XDG_RUNTIME_DIR\n' >&2
  exit 1
fi
grep -Fq 'configured Sigil service requires a valid XDG_RUNTIME_DIR' \
  "$temp_root/missing-runtime.log"

printf 'sigil appliance status integration proof passed\n'
