#!/usr/bin/env bash

set -euo pipefail

FORMAT='goq-public-alpha-uat-v2'
EVIDENCE_SCHEMA='goq-public-alpha-evidence-v2'
MAX_SOURCE_BYTES=$((1024 * 1024))
DEFAULT_MAX_AGE_SECONDS=$((7 * 24 * 60 * 60))
FUTURE_SKEW_SECONDS=300
GITHUB_REPOSITORY='FelineStateMachine/goq'
required_kinds=(cold-boot controller mouse soak network-direct network-relay reconnect second-client)
all_kinds=("${required_kinds[@]}" loopback-preflight)
script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/.." && pwd -P)"
sigil_release_verifier="$script_dir/verify-sigil-release.sh"
portal_release_verifier="$script_dir/verify-portal-release.py"
portal_signature_verifier="$script_dir/verify-macos-portal-signature.sh"
sigil_public_key="$repo_dir/release/sigil-minisign.pub"
portal_apple_team_id_file="$repo_dir/release/portal-apple-team-id.txt"

usage() {
  cat <<'EOF'
Usage:
  public-alpha-uat.sh init --evidence-dir DIR --release-tag vVERSION --sigil-archive FILE --sigil-bin FILE --portal-assets DIR [--max-age-seconds N]
  public-alpha-uat.sh record --evidence-dir DIR --kind KIND --file FILE
  public-alpha-uat.sh verify --evidence-dir DIR --sigil-archive FILE --sigil-bin FILE --portal-assets DIR

The harness never runs hardware tests or invents attestations. `record` ingests
strict key=value evidence and `verify` succeeds only when every required gate is
present, current, bound to an exact signed release tag and verified release
assets, and internally valid. Hardware UAT initialization and verification must
run on macOS so Portal signature, notarization, and Gatekeeper checks can run.
EOF
}

die() {
  printf 'public-alpha UAT: %s\n' "$*" >&2
  exit 1
}

sha256_file() {
  local path="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{print $1}'
  else
    shasum -a 256 "$path" | awk '{print $1}'
  fi
}

file_mode() {
  local path="$1"
  if stat -f '%Lp' "$path" >/dev/null 2>&1; then
    stat -f '%Lp' "$path"
  else
    stat -c '%a' "$path"
  fi
}

file_size() {
  local path="$1"
  if stat -f '%z' "$path" >/dev/null 2>&1; then
    stat -f '%z' "$path"
  else
    stat -c '%s' "$path"
  fi
}

require_absolute_path() {
  [[ "$1" == /* ]] || die "path must be absolute: $1"
}

require_secure_directory() {
  local path="$1"
  local mode
  [[ -d "$path" && ! -L "$path" ]] || die "directory is missing or a symlink: $path"
  mode="$(file_mode "$path")"
  [[ "$mode" == 700 ]] || die "directory must have mode 0700: $path (mode $mode)"
}

require_safe_regular_file() {
  local path="$1"
  local mode
  local permissions
  [[ -f "$path" && ! -L "$path" ]] || die "file is missing, non-regular, or a symlink: $path"
  mode="$(file_mode "$path")"
  [[ "$mode" =~ ^[0-7]{3,4}$ ]] || die "could not determine safe permissions for $path"
  permissions=$((8#$mode))
  (( (permissions & 8#022) == 0 )) || die "file is writable by group or others: $path"
}

require_hash() {
  [[ "$1" =~ ^[0-9a-f]{64}$ ]] || die "invalid SHA256 value for $2"
}

require_commit() {
  [[ "$1" =~ ^[0-9a-f]{40}$ ]] || die "invalid git commit in $2"
}

require_release_tag() {
  [[ "$1" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$ ]] \
    || die "release tag must be an exact vVERSION tag"
}

require_safe_directory() {
  local path="$1"
  local mode
  local permissions
  [[ -d "$path" && ! -L "$path" ]] || die "directory is missing or a symlink: $path"
  mode="$(file_mode "$path")"
  [[ "$mode" =~ ^[0-7]{3,4}$ ]] || die "could not determine safe permissions for $path"
  permissions=$((8#$mode))
  (( (permissions & 8#022) == 0 )) || die "directory is writable by group or others: $path"
}

verified_commit=''
verified_portal_dmg=''
verified_portal_checksum=''
verified_portal_manifest=''

github_api() {
  local endpoint="$1"
  command -v gh > /dev/null 2>&1 || die "GitHub CLI (gh) is required to verify published release assets"
  GH_PROMPT_DISABLED=1 gh api "$endpoint" \
    || die "GitHub API request failed for $endpoint"
}

git_object_from_json() {
  PYTHONDONTWRITEBYTECODE=1 python3 -c '
import json
import re
import sys

try:
    obj = json.load(sys.stdin)["object"]
    kind = obj["type"]
    sha = obj["sha"]
except (KeyError, TypeError, ValueError, json.JSONDecodeError):
    raise SystemExit(1)
if kind not in {"commit", "tag"} or not isinstance(sha, str) or not re.fullmatch(r"[0-9a-f]{40}", sha):
    raise SystemExit(1)
print(f"{kind}\t{sha}")
'
}

resolve_published_tag_commit() {
  local release_tag="$1"
  local response
  local parsed
  local object_type
  local object_sha
  local depth=0

  response="$(github_api "/repos/$GITHUB_REPOSITORY/git/ref/tags/$release_tag")"
  parsed="$(printf '%s' "$response" | git_object_from_json)" \
    || die "GitHub tag ref is malformed for $release_tag"
  IFS=$'\t' read -r object_type object_sha <<< "$parsed"
  while [[ "$object_type" == tag ]]; do
    ((depth += 1))
    (( depth <= 8 )) || die "GitHub annotated tag chain is too deep for $release_tag"
    response="$(github_api "/repos/$GITHUB_REPOSITORY/git/tags/$object_sha")"
    parsed="$(printf '%s' "$response" | git_object_from_json)" \
      || die "GitHub annotated tag object is malformed for $release_tag"
    IFS=$'\t' read -r object_type object_sha <<< "$parsed"
  done
  [[ "$object_type" == commit ]] || die "GitHub tag does not resolve to a commit: $release_tag"
  printf '%s\n' "$object_sha"
}

verify_published_release() {
  local release_tag="$1"
  local expected_commit="$2"
  local archive="$3"
  local portal_dmg="$4"
  local portal_checksum="$5"
  local portal_manifest="$6"
  local remote_commit
  local release_json

  remote_commit="$(resolve_published_tag_commit "$release_tag")"
  require_equal "$remote_commit" "$expected_commit" "published GitHub tag commit"
  release_json="$(github_api "/repos/$GITHUB_REPOSITORY/releases/tags/$release_tag")"
  PYTHONDONTWRITEBYTECODE=1 python3 - \
    "$GITHUB_REPOSITORY" "$release_tag" \
    "$archive" "$archive.sha256" "$archive.minisig" \
    "$portal_dmg" "$portal_checksum" "$portal_manifest" \
    3<<< "$release_json" <<'PY' \
    || die "supplied assets do not match the published GitHub prerelease"
import hashlib
import json
import os
import pathlib
import re
import sys

repository = sys.argv[1]
release_tag = sys.argv[2]
paths = [pathlib.Path(value) for value in sys.argv[3:]]
try:
    with os.fdopen(3) as release_stream:
        release = json.load(release_stream)
except (TypeError, ValueError, json.JSONDecodeError):
    raise SystemExit(1)

if release.get("tag_name") != release_tag:
    raise SystemExit(1)
if release.get("draft") is not False or release.get("prerelease") is not True:
    raise SystemExit(1)
if not isinstance(release.get("id"), int) or release["id"] <= 0:
    raise SystemExit(1)
if not isinstance(release.get("published_at"), str) or not release["published_at"]:
    raise SystemExit(1)
if release.get("html_url") != f"https://github.com/{repository}/releases/tag/{release_tag}":
    raise SystemExit(1)

assets = release.get("assets")
if not isinstance(assets, list) or len(assets) != len(paths):
    raise SystemExit(1)
by_name = {}
for asset in assets:
    if not isinstance(asset, dict) or not isinstance(asset.get("name"), str):
        raise SystemExit(1)
    if asset["name"] in by_name:
        raise SystemExit(1)
    by_name[asset["name"]] = asset

expected_names = {path.name for path in paths}
if len(expected_names) != len(paths) or set(by_name) != expected_names:
    raise SystemExit(1)
for path in paths:
    asset = by_name[path.name]
    digest = "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()
    if asset.get("state") != "uploaded" or asset.get("digest") != digest:
        raise SystemExit(1)
    if asset.get("size") != path.stat().st_size:
        raise SystemExit(1)
    if not isinstance(asset.get("id"), int) or asset["id"] <= 0:
        raise SystemExit(1)
    api_url = asset.get("url")
    if not isinstance(api_url, str) or not re.fullmatch(
        rf"https://api\.github\.com/repos/{re.escape(repository)}/releases/assets/[1-9][0-9]*",
        api_url,
    ):
        raise SystemExit(1)
PY
}

verify_portal_attestations() {
  local release_tag="$1"
  local expected_commit="$2"
  shift 2
  local asset

  for asset in "$@"; do
    GH_PROMPT_DISABLED=1 gh attestation verify "$asset" \
      --repo "$GITHUB_REPOSITORY" \
      --signer-workflow "$GITHUB_REPOSITORY/.github/workflows/portal-release.yml" \
      --source-ref "refs/tags/$release_tag" \
      --source-digest "$expected_commit" \
      --deny-self-hosted-runners > /dev/null \
      || die "Portal GitHub build-provenance attestation verification failed: $(basename -- "$asset")"
  done
}

verify_release_inputs() {
  local release_tag="$1"
  local archive="$2"
  local sigil="$3"
  local portal_assets="$4"
  local version
  local head
  local worktree_status
  local expected_names
  local portal_apple_team_id

  require_release_tag "$release_tag"
  require_absolute_path "$archive"
  require_absolute_path "$sigil"
  require_absolute_path "$portal_assets"
  require_safe_regular_file "$archive"
  require_safe_regular_file "$archive.sha256"
  require_safe_regular_file "$archive.minisig"
  require_safe_regular_file "$sigil"
  require_safe_directory "$portal_assets"
  require_safe_regular_file "$sigil_release_verifier"
  require_safe_regular_file "$portal_release_verifier"
  require_safe_regular_file "$portal_signature_verifier"
  require_safe_regular_file "$sigil_public_key"
  require_safe_regular_file "$portal_apple_team_id_file"

  verified_commit="$(git -C "$repo_dir" rev-parse --verify "refs/tags/$release_tag^{commit}" 2>/dev/null)" \
    || die "release tag does not resolve through refs/tags/$release_tag"
  require_commit "$verified_commit" release
  head="$(git -C "$repo_dir" rev-parse --verify HEAD 2>/dev/null)" || die "repository HEAD is unavailable"
  require_equal "$head" "$verified_commit" "release tag commit"
  worktree_status="$(git -C "$repo_dir" status --porcelain=v1 --untracked-files=all)" \
    || die "release worktree status is unavailable"
  [[ -z "$worktree_status" ]] || die "hardware UAT must run from the clean exact release tag"

  version="${release_tag#v}"
  verified_portal_dmg="$portal_assets/Portal-$version-arm64.dmg"
  verified_portal_checksum="$verified_portal_dmg.sha256"
  verified_portal_manifest="$portal_assets/Portal-$version-arm64.json"
  expected_names="$(
    find "$portal_assets" -mindepth 1 -maxdepth 1 -print \
      | sed 's#.*/##' \
      | LC_ALL=C sort
  )"
  require_equal "$expected_names" "$(printf '%s\n' \
    "Portal-$version-arm64.dmg" \
    "Portal-$version-arm64.dmg.sha256" \
    "Portal-$version-arm64.json" | LC_ALL=C sort)" "Portal asset set"
  require_safe_regular_file "$verified_portal_dmg"
  require_safe_regular_file "$verified_portal_checksum"
  require_safe_regular_file "$verified_portal_manifest"

  "$sigil_release_verifier" \
    --tag "$release_tag" \
    --archive "$archive" \
    --source-commit "$verified_commit" \
    --public-key-file "$sigil_public_key" > /dev/null \
    || die "signed Sigil release verification failed"

  python3 - "$archive" "$sigil" <<'PY' || die "supplied Sigil executable does not equal payload/release/sigil"
import pathlib
import sys
import tarfile

archive_path = pathlib.Path(sys.argv[1])
sigil_path = pathlib.Path(sys.argv[2])
try:
    with tarfile.open(archive_path, "r:gz") as archive:
        member = archive.getmember("payload/release/sigil")
        if not member.isreg():
            raise ValueError("Sigil payload is not regular")
        stream = archive.extractfile(member)
        if stream is None or stream.read() != sigil_path.read_bytes():
            raise ValueError("Sigil payload differs")
except (KeyError, OSError, tarfile.TarError, ValueError):
    raise SystemExit(1)
PY

  PYTHONDONTWRITEBYTECODE=1 python3 "$portal_release_verifier" assets \
    --repo-dir "$repo_dir" \
    --release-tag "$release_tag" \
    --asset-dir "$portal_assets" > /dev/null \
    || die "Portal release asset verification failed"

  portal_apple_team_id="$(awk 'NF { count += 1; value = $0 } END { if (count != 1) exit 1; print value }' \
    "$portal_apple_team_id_file")" \
    || die "committed Portal Apple TeamIdentifier pin must contain exactly one non-empty line"
  [[ "$portal_apple_team_id" =~ ^[A-Z0-9]{10}$ ]] \
    || die "committed Portal Apple TeamIdentifier pin is invalid"

  [[ "$(uname -s)" == Darwin ]] || die "hardware UAT release verification must run on macOS"
  bash "$portal_signature_verifier" \
    --dmg "$verified_portal_dmg" \
    --expected-version "$version" \
    --expected-team-id "$portal_apple_team_id" > /dev/null \
    || die "Portal macOS signature verification failed"

  verify_published_release \
    "$release_tag" \
    "$verified_commit" \
    "$archive" \
    "$verified_portal_dmg" \
    "$verified_portal_checksum" \
    "$verified_portal_manifest"
  verify_portal_attestations \
    "$release_tag" \
    "$verified_commit" \
    "$verified_portal_dmg" \
    "$verified_portal_checksum" \
    "$verified_portal_manifest"
}

manifest_get() {
  local manifest="$1"
  local key="$2"
  awk -F '\t' -v wanted="$key" '
    $1 == wanted { count += 1; value = $2 }
    END { if (count != 1) exit 1; print value }
  ' "$manifest" || die "manifest key is missing or duplicated: $key"
}

source_get() {
  local source="$1"
  local key="$2"
  awk -v wanted="$key" '
    index($0, wanted "=") == 1 {
      count += 1
      value = substr($0, length(wanted) + 2)
    }
    END { if (count != 1 || value == "") exit 1; print value }
  ' "$source" || die "evidence key is missing, empty, or duplicated: $key"
}

kind_allowed() {
  local wanted="$1"
  local candidate
  for candidate in "${all_kinds[@]}"; do
    [[ "$candidate" == "$wanted" ]] && return 0
  done
  return 1
}

common_keys() {
  printf '%s\n' uat_schema evidence_kind observed_at_unix git_commit release_tag sigil_sha256 portal_sha256
}

kind_keys() {
  case "$1" in
    cold-boot)
      printf '%s\n' cold_boot_result cold_boot_failure_count cold_boot_insufficient_count \
        headless_connector_state gaming_autologin_session sigil_host_enabled sigil_host_active \
        gamescope_pipewire_node gamescope_before_first_ssh sigil_unit_before_first_ssh \
        sigil_ready_before_first_ssh
      ;;
    controller)
      printf '%s\n' physical_controller_attached_to_portal actual_game_controlled \
        controller_coverage neutral_release_on_disconnect neutral_buttons neutral_axes session_seconds
      ;;
    mouse)
      printf '%s\n' target_application left_click_consumed right_click_consumed \
        consumption_observed_in_target click_attempts
      ;;
    soak)
      printf '%s\n' duration_seconds samples capture_fps_p50 presentation_fps_p50 \
        frame_interval_p95_ms hitch_p99_ms video_queue_p95_frames decode_queue_p95_frames \
        audio_queue_p95_ms av_skew_p95_ms max_queue_age_p95_ms cpu_p95_percent \
        gpu_p95_percent rss_p95_mib transport_drops frontend_drops audio_drops \
        latency_first_window_p95_ms latency_last_window_p95_ms disconnects
      ;;
    network-direct|network-relay)
      printf '%s\n' path_mode nat_scenario session_seconds rtt_p50_ms rtt_p95_ms \
        input_ack_p95_ms presentation_latency_p95_ms packet_loss_percent
      ;;
    reconnect)
      printf '%s\n' reconnect_cycles reconnect_successes reconnect_failures state_preserved \
        keyframe_recovery_p95_ms
      ;;
    second-client)
      printf '%s\n' second_client_attempts second_client_rejections \
        authorized_primary_uninterrupted rejection_reason
      ;;
    loopback-preflight)
      printf '%s\n' loopback_proof profile host_sha256 active_client_rejection reconnect_cycles cleanup
      ;;
    *) return 1 ;;
  esac
}

require_number() {
  [[ "$1" =~ ^[0-9]+([.][0-9]+)?$ ]] || die "$2 must be a non-negative decimal number"
}

number_ge() {
  awk -v value="$1" -v minimum="$2" 'BEGIN { exit !(value >= minimum) }'
}

number_le() {
  awk -v value="$1" -v maximum="$2" 'BEGIN { exit !(value <= maximum) }'
}

require_equal() {
  [[ "$1" == "$2" ]] || die "$3 must be $2 (got $1)"
}

validate_common_evidence() {
  local source="$1"
  local expected_kind="$2"
  local manifest="$3"
  local observed
  local now
  local max_age
  local value

  require_equal "$(source_get "$source" uat_schema)" "$EVIDENCE_SCHEMA" uat_schema
  require_equal "$(source_get "$source" evidence_kind)" "$expected_kind" evidence_kind
  value="$(source_get "$source" git_commit)"
  require_commit "$value" evidence
  require_equal "$value" "$(manifest_get "$manifest" git_commit)" git_commit
  value="$(source_get "$source" release_tag)"
  require_release_tag "$value"
  require_equal "$value" "$(manifest_get "$manifest" release_tag)" release_tag
  value="$(source_get "$source" sigil_sha256)"
  require_hash "$value" sigil_sha256
  require_equal "$value" "$(manifest_get "$manifest" sigil_sha256)" sigil_sha256
  value="$(source_get "$source" portal_sha256)"
  require_hash "$value" portal_sha256
  require_equal "$value" "$(manifest_get "$manifest" portal_sha256)" portal_sha256

  observed="$(source_get "$source" observed_at_unix)"
  [[ "$observed" =~ ^[0-9]{10}$ ]] || die "observed_at_unix must be a ten-digit Unix timestamp"
  now="$(date +%s)"
  max_age="$(manifest_get "$manifest" max_age_seconds)"
  (( observed <= now + FUTURE_SKEW_SECONDS )) || die "evidence timestamp is in the future"
  (( now - observed <= max_age )) || die "evidence is stale"
}

validate_cold_boot() {
  local source="$1"
  require_equal "$(source_get "$source" cold_boot_result)" pass cold_boot_result
  require_equal "$(source_get "$source" cold_boot_failure_count)" 0 cold_boot_failure_count
  require_equal "$(source_get "$source" cold_boot_insufficient_count)" 0 cold_boot_insufficient_count
  require_equal "$(source_get "$source" headless_connector_state)" ok headless_connector_state
  require_equal "$(source_get "$source" gaming_autologin_session)" ok gaming_autologin_session
  require_equal "$(source_get "$source" sigil_host_enabled)" enabled sigil_host_enabled
  require_equal "$(source_get "$source" sigil_host_active)" active sigil_host_active
  require_equal "$(source_get "$source" gamescope_pipewire_node)" ok gamescope_pipewire_node
  require_equal "$(source_get "$source" gamescope_before_first_ssh)" ok gamescope_before_first_ssh
  require_equal "$(source_get "$source" sigil_unit_before_first_ssh)" ok sigil_unit_before_first_ssh
  require_equal "$(source_get "$source" sigil_ready_before_first_ssh)" ok sigil_ready_before_first_ssh
}

validate_controller() {
  local source="$1"
  local seconds
  require_equal "$(source_get "$source" physical_controller_attached_to_portal)" pass physical_controller_attached_to_portal
  require_equal "$(source_get "$source" actual_game_controlled)" pass actual_game_controlled
  require_equal "$(source_get "$source" controller_coverage)" abxy,dpad,sticks,triggers,shoulders,start-back controller_coverage
  require_equal "$(source_get "$source" neutral_release_on_disconnect)" pass neutral_release_on_disconnect
  require_equal "$(source_get "$source" neutral_buttons)" pass neutral_buttons
  require_equal "$(source_get "$source" neutral_axes)" pass neutral_axes
  seconds="$(source_get "$source" session_seconds)"; require_number "$seconds" session_seconds
  number_ge "$seconds" 300 || die "controller session must last at least 300 seconds"
}

validate_mouse() {
  local source="$1"
  local target
  local attempts
  target="$(source_get "$source" target_application)"
  [[ "$target" =~ ^[A-Za-z0-9._+-]{1,64}$ ]] || die "target_application must be a bounded slug"
  require_equal "$(source_get "$source" left_click_consumed)" pass left_click_consumed
  require_equal "$(source_get "$source" right_click_consumed)" pass right_click_consumed
  require_equal "$(source_get "$source" consumption_observed_in_target)" pass consumption_observed_in_target
  attempts="$(source_get "$source" click_attempts)"; require_number "$attempts" click_attempts
  number_ge "$attempts" 5 || die "mouse evidence requires at least five click attempts"
}

validate_soak() {
  local source="$1"
  local key
  local value
  for key in $(kind_keys soak); do
    value="$(source_get "$source" "$key")"
    require_number "$value" "$key"
  done
  number_ge "$(source_get "$source" duration_seconds)" 3600 || die "soak must last at least 3600 seconds"
  number_ge "$(source_get "$source" samples)" 60 || die "soak requires at least 60 samples"
  number_ge "$(source_get "$source" capture_fps_p50)" 55 || die "capture_fps_p50 is below 55"
  number_ge "$(source_get "$source" presentation_fps_p50)" 55 || die "presentation_fps_p50 is below 55"
  number_le "$(source_get "$source" frame_interval_p95_ms)" 25 || die "frame_interval_p95_ms exceeds 25"
  number_le "$(source_get "$source" hitch_p99_ms)" 50 || die "hitch_p99_ms exceeds 50"
  number_le "$(source_get "$source" video_queue_p95_frames)" 2 || die "video queue exceeds two frames"
  number_le "$(source_get "$source" decode_queue_p95_frames)" 2 || die "decode queue exceeds two frames"
  number_le "$(source_get "$source" audio_queue_p95_ms)" 100 || die "audio queue exceeds 100 ms"
  number_le "$(source_get "$source" av_skew_p95_ms)" 50 || die "A/V skew exceeds 50 ms"
  number_le "$(source_get "$source" max_queue_age_p95_ms)" 100 || die "queue age exceeds 100 ms"
  number_le "$(source_get "$source" cpu_p95_percent)" 90 || die "CPU p95 exceeds 90 percent"
  number_le "$(source_get "$source" gpu_p95_percent)" 95 || die "GPU p95 exceeds 95 percent"
  number_le "$(source_get "$source" rss_p95_mib)" 2048 || die "RSS p95 exceeds 2048 MiB"
  require_equal "$(source_get "$source" disconnects)" 0 disconnects
  awk -v first="$(source_get "$source" latency_first_window_p95_ms)" \
      -v last="$(source_get "$source" latency_last_window_p95_ms)" \
      'BEGIN { growth = last - first; exit !(last <= first * 1.20 && growth <= 5.0) }' || \
    die "latency grew by more than both bounded thresholds"
}

validate_network() {
  local source="$1"
  local kind="$2"
  local expected_mode=direct
  local expected_nat=ordinary
  local key
  if [[ "$kind" == network-relay ]]; then
    expected_mode=relay
    expected_nat=difficult
  fi
  require_equal "$(source_get "$source" path_mode)" "$expected_mode" path_mode
  require_equal "$(source_get "$source" nat_scenario)" "$expected_nat" nat_scenario
  for key in session_seconds rtt_p50_ms rtt_p95_ms input_ack_p95_ms presentation_latency_p95_ms packet_loss_percent; do
    require_number "$(source_get "$source" "$key")" "$key"
  done
  number_ge "$(source_get "$source" session_seconds)" 600 || die "$kind session must last at least 600 seconds"
  number_le "$(source_get "$source" packet_loss_percent)" 5 || die "$kind packet loss exceeds 5 percent"
}

validate_reconnect() {
  local source="$1"
  local cycles successes failures recovery
  cycles="$(source_get "$source" reconnect_cycles)"; require_number "$cycles" reconnect_cycles
  successes="$(source_get "$source" reconnect_successes)"; require_number "$successes" reconnect_successes
  failures="$(source_get "$source" reconnect_failures)"; require_number "$failures" reconnect_failures
  recovery="$(source_get "$source" keyframe_recovery_p95_ms)"; require_number "$recovery" keyframe_recovery_p95_ms
  number_ge "$cycles" 10 || die "reconnect evidence requires at least ten cycles"
  require_equal "$successes" "$cycles" reconnect_successes
  require_equal "$failures" 0 reconnect_failures
  require_equal "$(source_get "$source" state_preserved)" pass state_preserved
  number_le "$recovery" 2000 || die "keyframe recovery p95 exceeds 2000 ms"
}

validate_second_client() {
  local source="$1"
  local attempts rejections
  attempts="$(source_get "$source" second_client_attempts)"; require_number "$attempts" second_client_attempts
  rejections="$(source_get "$source" second_client_rejections)"; require_number "$rejections" second_client_rejections
  number_ge "$attempts" 3 || die "second-client evidence requires at least three attempts"
  require_equal "$rejections" "$attempts" second_client_rejections
  require_equal "$(source_get "$source" authorized_primary_uninterrupted)" pass authorized_primary_uninterrupted
  require_equal "$(source_get "$source" rejection_reason)" active-client rejection_reason
}

validate_loopback() {
  local source="$1"
  local cycles
  require_equal "$(source_get "$source" loopback_proof)" ok loopback_proof
  require_equal "$(source_get "$source" profile)" release profile
  require_equal "$(source_get "$source" host_sha256)" "$(source_get "$source" sigil_sha256)" host_sha256
  require_equal "$(source_get "$source" active_client_rejection)" ok active_client_rejection
  require_equal "$(source_get "$source" cleanup)" ok cleanup
  cycles="$(source_get "$source" reconnect_cycles)"; require_number "$cycles" reconnect_cycles
  number_ge "$cycles" 3 || die "loopback preflight requires at least three reconnect cycles"
}

validate_kind() {
  case "$2" in
    cold-boot) validate_cold_boot "$1" ;;
    controller) validate_controller "$1" ;;
    mouse) validate_mouse "$1" ;;
    soak) validate_soak "$1" ;;
    network-direct|network-relay) validate_network "$1" "$2" ;;
    reconnect) validate_reconnect "$1" ;;
    second-client) validate_second_client "$1" ;;
    loopback-preflight) validate_loopback "$1" ;;
    *) die "unsupported evidence kind: $2" ;;
  esac
}

reject_sensitive_source() {
  local source="$1"
  if LC_ALL=C grep -Eiq '(^|_)(node_id|host_node_id|peer_node_id|secret|secret_key|identity_seed|private_key)=|goq-invite-v1[.]|BEGIN [A-Z ]*PRIVATE KEY' "$source"; then
    die "evidence contains a node ID, invitation, or secret-bearing field"
  fi
}

validate_manifest() {
  local directory="$1"
  local manifest="$directory/manifest.tsv"
  local key
  local record_kind
  local record_field
  require_secure_directory "$directory"
  require_secure_directory "$directory/records"
  require_safe_regular_file "$manifest"
  while IFS=$'\t' read -r key _; do
    case "$key" in
      format|created_at_unix|release_tag|git_commit|sigil_archive_name|sigil_archive_sha256|sigil_checksum_sha256|sigil_signature_sha256|sigil_public_key_sha256|sigil_sha256|portal_dmg_name|portal_sha256|portal_checksum_sha256|portal_manifest_sha256|max_age_seconds) ;;
      *)
        if [[ "$key" =~ ^record\.([a-z-]+)\.(file|sha256|observed_at_unix)$ ]]; then
          record_kind="${BASH_REMATCH[1]}"
          record_field="${BASH_REMATCH[2]}"
          kind_allowed "$record_kind" || die "manifest contains an unknown evidence kind: $record_kind"
          [[ -n "$record_field" ]] || die "invalid manifest record field: $key"
        else
          die "unexpected manifest key: $key"
        fi
        ;;
    esac
  done < "$manifest"
  require_equal "$(manifest_get "$manifest" format)" "$FORMAT" format
  require_commit "$(manifest_get "$manifest" git_commit)" manifest
  require_release_tag "$(manifest_get "$manifest" release_tag)"
  [[ "$(manifest_get "$manifest" sigil_archive_name)" =~ ^sigil-v[0-9A-Za-z.-]+-bazzite-x86_64\.tar\.gz$ ]] \
    || die "invalid manifest Sigil archive name"
  [[ "$(manifest_get "$manifest" portal_dmg_name)" =~ ^Portal-[0-9A-Za-z.-]+-arm64\.dmg$ ]] \
    || die "invalid manifest Portal DMG name"
  require_hash "$(manifest_get "$manifest" sigil_archive_sha256)" sigil_archive_sha256
  require_hash "$(manifest_get "$manifest" sigil_checksum_sha256)" sigil_checksum_sha256
  require_hash "$(manifest_get "$manifest" sigil_signature_sha256)" sigil_signature_sha256
  require_hash "$(manifest_get "$manifest" sigil_public_key_sha256)" sigil_public_key_sha256
  require_hash "$(manifest_get "$manifest" sigil_sha256)" sigil_sha256
  require_hash "$(manifest_get "$manifest" portal_sha256)" portal_sha256
  require_hash "$(manifest_get "$manifest" portal_checksum_sha256)" portal_checksum_sha256
  require_hash "$(manifest_get "$manifest" portal_manifest_sha256)" portal_manifest_sha256
  [[ "$(manifest_get "$manifest" created_at_unix)" =~ ^[0-9]{10}$ ]] || die "invalid manifest created_at_unix"
  local max_age
  max_age="$(manifest_get "$manifest" max_age_seconds)"
  if [[ ! "$max_age" =~ ^[0-9]+$ ]] || (( max_age < 3600 || max_age > 2592000 )); then
    die "invalid manifest max_age_seconds"
  fi
}

atomic_manifest_record() {
  local directory="$1"
  local kind="$2"
  local record_sha="$3"
  local observed="$4"
  local manifest="$directory/manifest.tsv"
  local temporary
  temporary="$(mktemp "$directory/.manifest.XXXXXX")"
  chmod 600 "$temporary"
  awk -F '\t' -v prefix="record.$kind." 'index($1, prefix) != 1 { print }' "$manifest" > "$temporary"
  {
    printf 'record.%s.file\trecords/%s.evidence\n' "$kind" "$kind"
    printf 'record.%s.sha256\t%s\n' "$kind" "$record_sha"
    printf 'record.%s.observed_at_unix\t%s\n' "$kind" "$observed"
  } >> "$temporary"
  mv -f "$temporary" "$manifest"
}

command_init() {
  local directory=''
  local release_tag=''
  local archive=''
  local sigil=''
  local portal_assets=''
  local max_age="$DEFAULT_MAX_AGE_SECONDS"
  local parent
  while (($#)); do
    case "$1" in
      --evidence-dir) [[ $# -ge 2 ]] || die "$1 requires a value"; directory="$2"; shift 2 ;;
      --release-tag) [[ $# -ge 2 ]] || die "$1 requires a value"; release_tag="$2"; shift 2 ;;
      --sigil-archive) [[ $# -ge 2 ]] || die "$1 requires a value"; archive="$2"; shift 2 ;;
      --sigil-bin) [[ $# -ge 2 ]] || die "$1 requires a value"; sigil="$2"; shift 2 ;;
      --portal-assets) [[ $# -ge 2 ]] || die "$1 requires a value"; portal_assets="$2"; shift 2 ;;
      --max-age-seconds) [[ $# -ge 2 ]] || die "$1 requires a value"; max_age="$2"; shift 2 ;;
      *) die "unknown init argument: $1" ;;
    esac
  done
  [[ -n "$directory" && -n "$release_tag" && -n "$archive" && -n "$sigil" && -n "$portal_assets" ]] \
    || die "init requires evidence dir, release tag, Sigil archive/executable, and Portal asset directory"
  require_absolute_path "$directory"
  if [[ ! "$max_age" =~ ^[0-9]+$ ]] || (( max_age < 3600 || max_age > 2592000 )); then
    die "max age must be 3600..2592000 seconds"
  fi
  [[ ! -e "$directory" && ! -L "$directory" ]] || die "evidence directory already exists"
  parent="$(dirname "$directory")"; [[ -d "$parent" && ! -L "$parent" ]] || die "evidence parent is missing or a symlink"
  verify_release_inputs "$release_tag" "$archive" "$sigil" "$portal_assets"
  mkdir -m 700 "$directory"
  mkdir -m 700 "$directory/records"
  umask 077
  {
    printf 'format\t%s\n' "$FORMAT"
    printf 'created_at_unix\t%s\n' "$(date +%s)"
    printf 'release_tag\t%s\n' "$release_tag"
    printf 'git_commit\t%s\n' "$verified_commit"
    printf 'sigil_archive_name\t%s\n' "$(basename -- "$archive")"
    printf 'sigil_archive_sha256\t%s\n' "$(sha256_file "$archive")"
    printf 'sigil_checksum_sha256\t%s\n' "$(sha256_file "$archive.sha256")"
    printf 'sigil_signature_sha256\t%s\n' "$(sha256_file "$archive.minisig")"
    printf 'sigil_public_key_sha256\t%s\n' "$(sha256_file "$sigil_public_key")"
    printf 'sigil_sha256\t%s\n' "$(sha256_file "$sigil")"
    printf 'portal_dmg_name\t%s\n' "$(basename -- "$verified_portal_dmg")"
    printf 'portal_sha256\t%s\n' "$(sha256_file "$verified_portal_dmg")"
    printf 'portal_checksum_sha256\t%s\n' "$(sha256_file "$verified_portal_checksum")"
    printf 'portal_manifest_sha256\t%s\n' "$(sha256_file "$verified_portal_manifest")"
    printf 'max_age_seconds\t%s\n' "$max_age"
  } > "$directory/manifest.tsv"
  chmod 600 "$directory/manifest.tsv"
  printf 'public_alpha_uat=initialized\nevidence_dir=%s\nrelease_tag=%s\ngit_commit=%s\n' \
    "$directory" "$release_tag" "$verified_commit"
  printf 'sigil_archive_sha256=%s\nsigil_sha256=%s\nportal_sha256=%s\n' \
    "$(sha256_file "$archive")" "$(sha256_file "$sigil")" "$(sha256_file "$verified_portal_dmg")"
}

command_record() {
  local directory=''
  local kind=''
  local source=''
  local manifest
  local record
  local temporary
  local key
  local observed
  local size
  while (($#)); do
    case "$1" in
      --evidence-dir) [[ $# -ge 2 ]] || die "$1 requires a value"; directory="$2"; shift 2 ;;
      --kind) [[ $# -ge 2 ]] || die "$1 requires a value"; kind="$2"; shift 2 ;;
      --file) [[ $# -ge 2 ]] || die "$1 requires a value"; source="$2"; shift 2 ;;
      *) die "unknown record argument: $1" ;;
    esac
  done
  [[ -n "$directory" && -n "$kind" && -n "$source" ]] || die "record requires evidence dir, kind, and file"
  require_absolute_path "$directory"; require_absolute_path "$source"
  kind_allowed "$kind" || die "unknown evidence kind: $kind"
  validate_manifest "$directory"
  manifest="$directory/manifest.tsv"
  require_safe_regular_file "$source"
  size="$(file_size "$source")"; [[ "$size" =~ ^[0-9]+$ ]] || die "could not determine evidence size"
  (( size > 0 && size <= MAX_SOURCE_BYTES )) || die "evidence source is empty or exceeds $MAX_SOURCE_BYTES bytes"
  reject_sensitive_source "$source"
  validate_common_evidence "$source" "$kind" "$manifest"
  validate_kind "$source" "$kind"
  [[ ! -e "$directory/records/$kind.evidence" ]] || die "evidence kind is already recorded: $kind"

  temporary="$(mktemp "$directory/records/.$kind.XXXXXX")"
  chmod 600 "$temporary"
  while IFS= read -r key; do
    printf '%s=%s\n' "$key" "$(source_get "$source" "$key")" >> "$temporary"
  done < <({ common_keys; kind_keys "$kind"; } | awk '!seen[$0]++')
  record="$directory/records/$kind.evidence"
  mv "$temporary" "$record"
  observed="$(source_get "$record" observed_at_unix)"
  atomic_manifest_record "$directory" "$kind" "$(sha256_file "$record")" "$observed"
  printf 'public_alpha_uat=recorded\nevidence_kind=%s\nrecord_sha256=%s\n' "$kind" "$(sha256_file "$record")"
}

command_verify() {
  local directory=''
  local archive=''
  local sigil=''
  local portal_assets=''
  local manifest
  local kind
  local record
  local expected
  local actual
  local file
  local filename
  local found_kind
  while (($#)); do
    case "$1" in
      --evidence-dir) [[ $# -ge 2 ]] || die "$1 requires a value"; directory="$2"; shift 2 ;;
      --sigil-archive) [[ $# -ge 2 ]] || die "$1 requires a value"; archive="$2"; shift 2 ;;
      --sigil-bin) [[ $# -ge 2 ]] || die "$1 requires a value"; sigil="$2"; shift 2 ;;
      --portal-assets) [[ $# -ge 2 ]] || die "$1 requires a value"; portal_assets="$2"; shift 2 ;;
      *) die "unknown verify argument: $1" ;;
    esac
  done
  [[ -n "$directory" && -n "$archive" && -n "$sigil" && -n "$portal_assets" ]] \
    || die "verify requires evidence dir, Sigil archive/executable, and Portal asset directory"
  require_absolute_path "$directory"
  validate_manifest "$directory"; manifest="$directory/manifest.tsv"
  verify_release_inputs "$(manifest_get "$manifest" release_tag)" "$archive" "$sigil" "$portal_assets"
  require_equal "$verified_commit" "$(manifest_get "$manifest" git_commit)" "release tag commit"
  require_equal "$(basename -- "$archive")" "$(manifest_get "$manifest" sigil_archive_name)" "Sigil archive name"
  require_equal "$(sha256_file "$archive")" "$(manifest_get "$manifest" sigil_archive_sha256)" "Sigil archive hash"
  require_equal "$(sha256_file "$archive.sha256")" "$(manifest_get "$manifest" sigil_checksum_sha256)" "Sigil checksum asset hash"
  require_equal "$(sha256_file "$archive.minisig")" "$(manifest_get "$manifest" sigil_signature_sha256)" "Sigil signature asset hash"
  require_equal "$(sha256_file "$sigil_public_key")" "$(manifest_get "$manifest" sigil_public_key_sha256)" "Sigil public key hash"
  require_equal "$(sha256_file "$sigil")" "$(manifest_get "$manifest" sigil_sha256)" "Sigil release-candidate hash"
  require_equal "$(basename -- "$verified_portal_dmg")" "$(manifest_get "$manifest" portal_dmg_name)" "Portal DMG name"
  require_equal "$(sha256_file "$verified_portal_dmg")" "$(manifest_get "$manifest" portal_sha256)" "Portal release-candidate hash"
  require_equal "$(sha256_file "$verified_portal_checksum")" "$(manifest_get "$manifest" portal_checksum_sha256)" "Portal checksum asset hash"
  require_equal "$(sha256_file "$verified_portal_manifest")" "$(manifest_get "$manifest" portal_manifest_sha256)" "Portal manifest asset hash"

  while IFS= read -r file; do
    case "$file" in
      "$directory/manifest.tsv") [[ -f "$file" ]] || die "manifest is not a regular file" ;;
      "$directory/records") [[ -d "$file" ]] || die "records path is not a directory" ;;
      "$directory/records/"*.evidence)
        [[ -f "$file" ]] || die "evidence record is not a regular file: $file"
        filename="${file##*/}"
        found_kind="${filename%.evidence}"
        kind_allowed "$found_kind" || die "unexpected evidence kind in bundle: $found_kind"
        ;;
      *) die "unexpected file or directory in evidence bundle: $file" ;;
    esac
    [[ ! -L "$file" ]] || die "evidence bundle contains a symlink: $file"
  done < <(find "$directory" -mindepth 1 -maxdepth 2 -print)

  for kind in "${required_kinds[@]}"; do
    record="$directory/records/$kind.evidence"
    require_safe_regular_file "$record"
    expected="$(manifest_get "$manifest" "record.$kind.sha256")"
    actual="$(sha256_file "$record")"
    require_equal "$actual" "$expected" "$kind record hash"
    require_equal "$(manifest_get "$manifest" "record.$kind.file")" "records/$kind.evidence" "$kind record path"
    require_equal "$(manifest_get "$manifest" "record.$kind.observed_at_unix")" \
      "$(source_get "$record" observed_at_unix)" "$kind record timestamp"
    validate_common_evidence "$record" "$kind" "$manifest"
    validate_kind "$record" "$kind"
  done
  if grep -q '^record\.loopback-preflight\.' "$manifest" && \
      [[ ! -e "$directory/records/loopback-preflight.evidence" ]]; then
    die "loopback preflight manifest metadata exists without its record"
  fi
  if [[ -e "$directory/records/loopback-preflight.evidence" ]]; then
    record="$directory/records/loopback-preflight.evidence"
    require_safe_regular_file "$record"
    require_equal "$(sha256_file "$record")" \
      "$(manifest_get "$manifest" record.loopback-preflight.sha256)" "loopback preflight record hash"
    require_equal "$(manifest_get "$manifest" record.loopback-preflight.file)" \
      "records/loopback-preflight.evidence" "loopback preflight record path"
    require_equal "$(manifest_get "$manifest" record.loopback-preflight.observed_at_unix)" \
      "$(source_get "$record" observed_at_unix)" "loopback preflight record timestamp"
    validate_common_evidence "$record" loopback-preflight "$manifest"
    validate_loopback "$record"
  fi

  printf 'public_alpha_uat=pass\ngit_commit=%s\nrelease_tag=%s\n' \
    "$(manifest_get "$manifest" git_commit)" "$(manifest_get "$manifest" release_tag)"
  printf 'sigil_sha256=%s\nportal_sha256=%s\nrequired_gates=%s\n' \
    "$(manifest_get "$manifest" sigil_sha256)" "$(manifest_get "$manifest" portal_sha256)" "${#required_kinds[@]}"
}

main() {
  local command="${1:-}"
  [[ -n "$command" ]] || { usage; exit 2; }
  shift
  case "$command" in
    init) command_init "$@" ;;
    record) command_record "$@" ;;
    verify) command_verify "$@" ;;
    --help|-h|help) usage ;;
    *) usage >&2; die "unknown command: $command" ;;
  esac
}

main "$@"
