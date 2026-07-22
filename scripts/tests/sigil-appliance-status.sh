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
reset_status="$temp_root/reset-status.json"

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

host_fingerprint="$(python3 - "$live_status" <<'PY'
import json
import pathlib
import sys

print(json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))["identity"]["host_fingerprint"])
PY
)"

if XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" appliance enrollment-reset \
    --config "$config_path" \
    --expected-host-fingerprint "$host_fingerprint" \
    --json \
    >"$temp_root/live-reset.log" 2>&1; then
  printf 'enrollment reset unexpectedly mutated a live Sigil daemon\n' >&2
  exit 1
fi
grep -Fq 'another Sigil daemon already owns this state directory' \
  "$temp_root/live-reset.log"

wait "$host_pid"
host_pid=''
[[ ! -e "$state_directory/authorization-v1.json" ]]
if XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" appliance enrollment-reset \
    --config "$config_path" \
    --expected-host-fingerprint '00000000…00000000' \
    --json \
    >"$temp_root/mismatch-reset.log" 2>&1; then
  printf 'enrollment reset accepted the wrong host fingerprint\n' >&2
  exit 1
fi
[[ ! -e "$state_directory/authorization-v1.json" ]]
XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" appliance enrollment-reset \
    --config "$config_path" \
    --expected-host-fingerprint "$host_fingerprint" \
    --json >"$reset_status"
XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" appliance status --config "$config_path" --json >"$stopped_status"

python3 - "$stopped_status" "$reset_status" "$host_node_id" <<'PY'
import json
import pathlib
import sys

status = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
reset_raw = pathlib.Path(sys.argv[2]).read_text(encoding="utf-8")
reset = json.loads(reset_raw)
host_node_id = sys.argv[3]
runtime = status.get("runtime", {})
if runtime.get("daemon") != "stopped":
    raise SystemExit(f"clean shutdown did not publish stopped: {runtime}")
if runtime.get("session") != "inactive":
    raise SystemExit("stopped daemon did not report an inactive session")
if runtime.get("uptime_ms") is not None:
    raise SystemExit("stopped daemon retained a live uptime")
if reset.get("schema_version") != 1 or reset.get("operation") != "enrollment_reset":
    raise SystemExit("unexpected enrollment reset schema")
if reset.get("had_enrollment") is not False:
    raise SystemExit("empty enrollment reset reported an enrolled Portal")
if reset.get("current_epoch") != reset.get("previous_epoch") + 1:
    raise SystemExit("enrollment reset did not advance exactly one epoch")
if reset.get("invitations_invalidated") is not True:
    raise SystemExit("enrollment reset did not report invitation invalidation")
if host_node_id in reset_raw:
    raise SystemExit("enrollment reset leaked the complete host node ID")
if status.get("enrollment", {}).get("epoch") != reset.get("current_epoch"):
    raise SystemExit("status does not reflect the enrollment reset epoch")
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
