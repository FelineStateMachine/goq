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
alternate_runtime_directory="$temp_root/runtime-alternate"
invalid_runtime_directory="$temp_root/invalid-runtime"
config_directory="$temp_root/config"
unsafe_config_directory="$temp_root/config-unsafe"
gamescope_probe_state="$temp_root/gamescope-probe-state"
identity_path="$identity_directory/host.key"
config_path="$config_directory/host.toml"
unsafe_config_path="$unsafe_config_directory/host.toml"
gamescope_probe_config="$config_directory/gamescope-probe.toml"
fake_pw_dump="$temp_root/fake-pw-dump"
fake_render_node="$temp_root/renderD999"
host_log="$temp_root/host.log"
second_log="$temp_root/second.log"
live_status="$temp_root/live-status.json"
legacy_status="$temp_root/legacy-status.json"
stopped_status="$temp_root/stopped-status.json"
reset_status="$temp_root/reset-status.json"
config_show="$temp_root/config-show.json"
config_request="$temp_root/config-request.json"
config_set="$temp_root/config-set.json"
candidate_status="$temp_root/candidate-status.json"

mkdir -m 0700 \
  "$identity_directory" \
  "$state_directory" \
  "$gamescope_probe_state" \
  "$runtime_directory" \
  "$alternate_runtime_directory"
mkdir -m 0755 "$config_directory"
mkdir -m 0775 "$unsafe_config_directory"
chmod 0775 "$unsafe_config_directory"
mkdir -m 0755 "$invalid_runtime_directory"
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

printf '%s\n' \
  '#!/usr/bin/env bash' \
  "printf '[]\\n'" \
  >"$fake_pw_dump"
chmod 0700 "$fake_pw_dump"
false_path="/usr/bin/false"
[[ -x "$false_path" ]]
printf '%s\n' \
  "identity_path = \"$identity_path\"" \
  "state_path = \"$gamescope_probe_state\"" \
  'source = "gamescope-pipewire"' \
  'width = 1280' \
  'height = 800' \
  'framerate = 60' \
  'codec = "h264"' \
  'input_mode = "disabled"' \
  "ffmpeg_path = \"$ffmpeg\"" \
  '' \
  '[gamescope_pipewire]' \
  'node_name = "gamescope"' \
  'media_class = "Video/Source"' \
  'xwayland_display = ":0"' \
  "pw_dump_path = \"$fake_pw_dump\"" \
  "gst_launch_path = \"$false_path\"" \
  "gst_inspect_path = \"$false_path\"" \
  'vaapi_encoder = "vah264enc"' \
  "vaapi_render_node = \"$fake_render_node\"" \
  'rate_control = "cqp"' \
  'quantizer = 24' \
  >"$gamescope_probe_config"
chmod 0600 "$gamescope_probe_config"

cp "$config_path" "$unsafe_config_path"
chmod 0600 "$unsafe_config_path"
unsafe_config_hash="$(python3 - "$unsafe_config_path" <<'PY'
import hashlib
import pathlib
import sys
print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())
PY
)"
if XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" config check --config "$unsafe_config_path" \
    >"$temp_root/unsafe-config-check.out" \
    2>"$temp_root/unsafe-config-check.error"; then
  printf 'config check accepted a group-writable config directory\n' >&2
  exit 1
fi
[[ "$unsafe_config_hash" == "$(python3 - "$unsafe_config_path" <<'PY'
import hashlib
import pathlib
import sys
print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())
PY
)" ]]

XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" serve \
    --config "$config_path" \
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
  "$sigil" appliance status --config "$config_path" --json --schema-version 2 >"$live_status"
XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" appliance status --config "$config_path" --json >"$legacy_status"
XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" appliance config show --config "$config_path" --json \
  >"$temp_root/live-config-show.json"
python3 - "$temp_root/live-config-show.json" "$temp_root/live-config-request.json" <<'PY'
import json
import pathlib
import sys
show = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
request = {
    "schema_version": 1,
    "expected_revision": show["revision"],
    "settings": {
        "resolution": {"mode": "native"},
        "framerate": 72,
        "rate_control": None,
    },
}
pathlib.Path(sys.argv[2]).write_text(json.dumps(request), encoding="utf-8")
PY
if XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" appliance config set --config "$config_path" --json \
  <"$temp_root/live-config-request.json" >"$temp_root/live-config-set.out" \
  2>"$temp_root/live-config-set.error"; then
  printf 'config set unexpectedly mutated a live Sigil daemon\n' >&2
  exit 1
fi
python3 - "$temp_root/live-config-set.error" <<'PY'
import json
import pathlib
import sys
error = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if error != {"schema_version": 1, "error": {"code": "lifecycle_busy"}}:
    raise SystemExit(f"unexpected live config error: {error}")
PY

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
grep -Fq 'another Sigil daemon or capture probe already owns this lifecycle scope' "$second_log"

if XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" capture probe \
    --source gamescope-pipewire \
    --config "$gamescope_probe_config" \
    --frames 1 \
    >"$temp_root/contended-capture.out" \
    2>"$temp_root/contended-capture.error"; then
  printf 'Gamescope capture probe overlapped a live Sigil daemon\n' >&2
  exit 1
fi
grep -Fq 'another Sigil daemon or capture probe already owns this lifecycle scope' \
  "$temp_root/contended-capture.error"
if grep -Fq 'inspecting VAAPI render node' "$temp_root/contended-capture.error"; then
  printf 'contended capture reached hardware preflight before acquiring the lifecycle lock\n' >&2
  exit 1
fi

python3 - "$live_status" "$host_node_id" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
raw = path.read_text(encoding="utf-8")
status = json.loads(raw)
host_node_id = sys.argv[2]

if status.get("schema_version") != 2:
    raise SystemExit("unexpected appliance status schema")
if status.get("overall") != "ready":
    raise SystemExit(f"unexpected live overall state: {status.get('overall')}")
if status.get("runtime", {}).get("daemon") != "ready":
    raise SystemExit("live daemon state is not ready")
if status.get("runtime", {}).get("session") != "inactive":
    raise SystemExit("live proof unexpectedly reports a session")
if not status.get("runtime", {}).get("loaded_config_revision", "").startswith("sha256:"):
    raise SystemExit("live runtime is not bound to an exact config revision")
if status.get("runtime", {}).get("reached_ready") is not True:
    raise SystemExit("live runtime did not retain its ready transition")
if status.get("enrollment", {}).get("state") != "none":
    raise SystemExit("direct proof unexpectedly reports an enrollment")
if host_node_id in raw:
    raise SystemExit("appliance status leaked the complete host node ID")
fingerprint = status.get("identity", {}).get("host_fingerprint", "")
if len(fingerprint) != 17 or "…" not in fingerprint:
    raise SystemExit("host fingerprint is not bounded and redacted")
PY

python3 - "$legacy_status" <<'PY'
import json
import pathlib
import sys

status = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if status.get("schema_version") != 1:
    raise SystemExit("default appliance status did not preserve schema v1")
if "revision" in status.get("config", {}) or "pending_transaction" in status.get("config", {}):
    raise SystemExit("status v1 leaked status-v2 config fields")
runtime = status.get("runtime", {})
if any(field in runtime for field in ("instance_id", "loaded_config_revision", "reached_ready")):
    raise SystemExit("status v1 leaked status-v2 runtime fields")
PY

host_fingerprint="$(python3 - "$live_status" <<'PY'
import json
import pathlib
import sys

print(json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))["identity"]["host_fingerprint"])
PY
)"

if env -u XDG_RUNTIME_DIR \
  "$sigil" appliance enrollment-reset \
    --config "$config_path" \
    --expected-host-fingerprint "$host_fingerprint" \
    --json \
    >"$temp_root/live-reset.log" 2>&1; then
  printf 'enrollment reset unexpectedly mutated a live Sigil daemon\n' >&2
  exit 1
fi
grep -Fq 'another Sigil daemon or capture probe already owns this lifecycle scope' \
  "$temp_root/live-reset.log"

if ! kill -TERM "$host_pid" 2>/dev/null; then
  printf 'Sigil exited before the appliance status proof requested shutdown\n' >&2
  sed -n '1,160p' "$host_log" >&2
  exit 1
fi
if ! wait "$host_pid"; then
  printf 'Sigil did not shut down cleanly during the appliance status proof\n' >&2
  sed -n '1,160p' "$host_log" >&2
  exit 1
fi
host_pid=''
if XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" capture probe \
    --source gamescope-pipewire \
    --config "$gamescope_probe_config" \
    --frames 1 \
    >"$temp_root/released-capture.out" \
    2>"$temp_root/released-capture.error"; then
  printf 'fake Gamescope capture unexpectedly completed\n' >&2
  exit 1
fi
grep -Fq "inspecting VAAPI render node $fake_render_node" \
  "$temp_root/released-capture.error"
XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" serve \
    --identity "$identity_path" \
    --source test-pattern \
    --state-path "$temp_root/post-probe-state" \
    --ffmpeg "$ffmpeg" \
    --max-runtime-seconds 1 \
    >"$temp_root/post-probe-serve.out" 2>&1
grep -Fq 'status=ready' "$temp_root/post-probe-serve.out"
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
if XDG_RUNTIME_DIR="$invalid_runtime_directory" \
  "$sigil" appliance enrollment-reset \
    --config "$config_path" \
    --expected-host-fingerprint "$host_fingerprint" \
    --json \
    >"$temp_root/invalid-runtime-reset.log" 2>&1; then
  printf 'enrollment reset ignored an unsafe XDG runtime root\n' >&2
  exit 1
fi
[[ ! -e "$state_directory/authorization-v1.json" ]]
env -u XDG_RUNTIME_DIR \
  "$sigil" appliance enrollment-reset \
    --config "$config_path" \
    --expected-host-fingerprint "$host_fingerprint" \
    --json >"$reset_status"
XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" appliance status --config "$config_path" --json --schema-version 2 >"$stopped_status"

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
if not runtime.get("instance_id"):
    raise SystemExit("fresh stopped state omitted its daemon instance")
if runtime.get("reached_ready") is not True:
    raise SystemExit("fresh stopped state forgot that it reached ready")
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

XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" appliance config show --config "$config_path" --json >"$config_show"
python3 - "$config_show" "$config_request" <<'PY'
import json
import pathlib
import sys

show = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if show.get("schema_version") != 1 or show.get("pending_transaction") is not None:
    raise SystemExit("unexpected initial config projection")
request = {
    "schema_version": 1,
    "expected_revision": show["revision"],
    "settings": {
        "resolution": {"mode": "native"},
        "framerate": 72,
        "rate_control": None,
    },
}
pathlib.Path(sys.argv[2]).write_text(json.dumps(request), encoding="utf-8")
PY

base_hash="$(python3 - "$config_path" <<'PY'
import hashlib
import pathlib
import sys
print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())
PY
)"
XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" appliance config validate --config "$config_path" --json \
  <"$config_request" >"$temp_root/config-validate.json"
[[ "$base_hash" == "$(python3 - "$config_path" <<'PY'
import hashlib
import pathlib
import sys
print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())
PY
)" ]]
if env -u XDG_RUNTIME_DIR \
  "$sigil" appliance config set \
    --config "$config_path" --runtime-dir "$invalid_runtime_directory" --json \
    <"$config_request" >"$temp_root/invalid-runtime-set.out" \
    2>"$temp_root/invalid-runtime-set.error"; then
  printf 'config set accepted an unsafe explicit runtime root\n' >&2
  exit 1
fi
python3 - "$temp_root/invalid-runtime-set.error" <<'PY'
import json
import pathlib
import sys
error = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if error != {"schema_version": 1, "error": {"code": "unsafe_storage"}}:
    raise SystemExit(f"unexpected unsafe runtime-root error: {error}")
PY
[[ "$base_hash" == "$(python3 - "$config_path" <<'PY'
import hashlib
import pathlib
import sys
print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())
PY
)" ]]
[[ ! -e "$state_directory/config-transaction-v1.json" ]]

if env -u XDG_RUNTIME_DIR \
  "$sigil" appliance config set --config "$config_path" --json \
    <"$config_request" >"$temp_root/missing-runtime-set.out" \
    2>"$temp_root/missing-runtime-set.error"; then
  printf 'config set unexpectedly accepted no runtime authority\n' >&2
  exit 1
fi
python3 - "$temp_root/missing-runtime-set.error" <<'PY'
import json
import pathlib
import sys
error = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if error != {"schema_version": 1, "error": {"code": "unsafe_storage"}}:
    raise SystemExit(f"unexpected missing runtime-root error: {error}")
PY
[[ "$base_hash" == "$(python3 - "$config_path" <<'PY'
import hashlib
import pathlib
import sys
print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())
PY
)" ]]
[[ ! -e "$state_directory/config-transaction-v1.json" ]]

env -u XDG_RUNTIME_DIR \
  "$sigil" appliance config set \
    --config "$config_path" --runtime-dir "$runtime_directory" --json \
  <"$config_request" >"$config_set"

transaction="$(python3 - "$config_set" <<'PY'
import json
import pathlib
import sys
result = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if result.get("changed") is not True or result.get("restart_required") is not True:
    raise SystemExit("config set did not require candidate validation")
print(result["transaction"])
PY
)"

XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" serve --config "$config_path" --max-runtime-seconds 1 \
  >"$temp_root/candidate.log" 2>&1
XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" appliance status --config "$config_path" --json --schema-version 2 >"$candidate_status"
candidate_instance="$(python3 - "$candidate_status" "$config_set" <<'PY'
import json
import pathlib
import sys
status = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
staged = json.loads(pathlib.Path(sys.argv[2]).read_text(encoding="utf-8"))
runtime = status["runtime"]
if runtime.get("daemon") != "stopped" or runtime.get("reached_ready") is not True:
    raise SystemExit("candidate did not stop cleanly after reaching ready")
if runtime.get("loaded_config_revision") != staged.get("candidate_revision"):
    raise SystemExit("candidate runtime revision does not match staged config")
if status.get("config", {}).get("pending_transaction", {}).get("transaction") != staged.get("transaction"):
    raise SystemExit("status omitted the pending config transaction")
print(runtime["instance_id"])
PY
)"
mkdir -m 0700 "$alternate_runtime_directory/sigil-spark"
cp "$runtime_directory/sigil-spark/daemon-status-v1.json" \
  "$alternate_runtime_directory/sigil-spark/daemon-status-v1.json"
chmod 0600 "$alternate_runtime_directory/sigil-spark/daemon-status-v1.json"
cp "$config_path" "$temp_root/pre-mismatch-config.toml"
cp "$state_directory/config-transaction-v1.json" "$temp_root/pre-mismatch-journal.json"
cp "$state_directory/config-base-v1.toml" "$temp_root/pre-mismatch-base.toml"
cp "$state_directory/config-candidate-v1.toml" "$temp_root/pre-mismatch-candidate.toml"
if env -u XDG_RUNTIME_DIR \
  "$sigil" appliance config commit \
    --config "$config_path" \
    --runtime-dir "$alternate_runtime_directory" \
    --transaction "$transaction" \
    --expected-instance "$candidate_instance" \
    --json >"$temp_root/mismatched-runtime-commit.out" \
    2>"$temp_root/mismatched-runtime-commit.error"; then
  printf 'config commit accepted candidate evidence from a different runtime namespace\n' >&2
  exit 1
fi
python3 - "$temp_root/mismatched-runtime-commit.error" <<'PY'
import json
import pathlib
import sys
error = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if error != {"schema_version": 1, "error": {"code": "health_not_proven"}}:
    raise SystemExit(f"unexpected mismatched runtime error: {error}")
PY
cmp "$config_path" "$temp_root/pre-mismatch-config.toml"
cmp "$state_directory/config-transaction-v1.json" "$temp_root/pre-mismatch-journal.json"
cmp "$state_directory/config-base-v1.toml" "$temp_root/pre-mismatch-base.toml"
cmp "$state_directory/config-candidate-v1.toml" "$temp_root/pre-mismatch-candidate.toml"
env -u XDG_RUNTIME_DIR \
  "$sigil" appliance config commit \
    --config "$config_path" \
    --runtime-dir "$runtime_directory" \
    --transaction "$transaction" \
    --expected-instance "$candidate_instance" \
    --json >"$temp_root/config-commit.json"
XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" appliance config show --config "$config_path" --json \
  >"$temp_root/committed-show.json"

python3 - "$temp_root/committed-show.json" "$temp_root/rollback-request.json" <<'PY'
import json
import pathlib
import sys
show = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if show.get("pending_transaction") is not None or show["settings"]["framerate"] != 72:
    raise SystemExit("committed config state is incorrect")
request = {
    "schema_version": 1,
    "expected_revision": show["revision"],
    "settings": {
        "resolution": {"mode": "fixed", "width": 1280, "height": 800},
        "framerate": 60,
        "rate_control": None,
    },
}
pathlib.Path(sys.argv[2]).write_text(json.dumps(request), encoding="utf-8")
PY
committed_hash="$(python3 - "$config_path" <<'PY'
import hashlib
import pathlib
import sys
print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())
PY
)"
XDG_RUNTIME_DIR="$runtime_directory" \
  "$sigil" appliance config set --config "$config_path" --json \
  <"$temp_root/rollback-request.json" >"$temp_root/rollback-set.json"
rollback_transaction="$(python3 - "$temp_root/rollback-set.json" <<'PY'
import json
import pathlib
import sys
print(json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))["transaction"])
PY
)"
python3 - "$state_directory/config-transaction-v1.json" <<'PY'
import json
import pathlib
import sys
path = pathlib.Path(sys.argv[1])
journal = json.loads(path.read_text(encoding="utf-8"))
journal.pop("runtime_directory_id", None)
path.write_text(json.dumps(journal) + "\n", encoding="utf-8")
PY
cp "$config_path" "$temp_root/pre-legacy-commit-config.toml"
cp "$state_directory/config-transaction-v1.json" "$temp_root/pre-legacy-commit-journal.json"
cp "$state_directory/config-base-v1.toml" "$temp_root/pre-legacy-commit-base.toml"
cp "$state_directory/config-candidate-v1.toml" "$temp_root/pre-legacy-commit-candidate.toml"
if env -u XDG_RUNTIME_DIR \
  "$sigil" appliance config commit \
    --config "$config_path" \
    --runtime-dir "$runtime_directory" \
    --transaction "$rollback_transaction" \
    --expected-instance "$candidate_instance" \
    --json >"$temp_root/legacy-commit.out" \
    2>"$temp_root/legacy-commit.error"; then
  printf 'config commit accepted a legacy transaction without runtime binding\n' >&2
  exit 1
fi
python3 - "$temp_root/legacy-commit.error" <<'PY'
import json
import pathlib
import sys
error = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if error != {"schema_version": 1, "error": {"code": "health_not_proven"}}:
    raise SystemExit(f"unexpected legacy transaction commit error: {error}")
PY
cmp "$config_path" "$temp_root/pre-legacy-commit-config.toml"
cmp "$state_directory/config-transaction-v1.json" "$temp_root/pre-legacy-commit-journal.json"
cmp "$state_directory/config-base-v1.toml" "$temp_root/pre-legacy-commit-base.toml"
cmp "$state_directory/config-candidate-v1.toml" "$temp_root/pre-legacy-commit-candidate.toml"
env -u XDG_RUNTIME_DIR \
  "$sigil" appliance config rollback \
    --config "$config_path" --transaction "$rollback_transaction" --json \
    >"$temp_root/config-rollback.json"
[[ "$committed_hash" == "$(python3 - "$config_path" <<'PY'
import hashlib
import pathlib
import sys
print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())
PY
)" ]]
[[ ! -e "$state_directory/config-transaction-v1.json" ]]
[[ ! -e "$state_directory/config-base-v1.toml" ]]
[[ ! -e "$state_directory/config-candidate-v1.toml" ]]

if env -u XDG_RUNTIME_DIR \
  "$sigil" serve --config "$config_path" --max-runtime-seconds 1 \
  >"$temp_root/missing-runtime.log" 2>&1; then
  printf 'configured Sigil unexpectedly started without XDG_RUNTIME_DIR\n' >&2
  exit 1
fi
grep -Fq 'configured Sigil service requires a valid XDG_RUNTIME_DIR' \
  "$temp_root/missing-runtime.log"

printf 'sigil appliance status integration proof passed\n'
