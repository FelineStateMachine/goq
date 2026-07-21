#!/usr/bin/env bash

set -euo pipefail

source_repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
production_harness="$source_repo/scripts/public-alpha-uat.sh"
temporary_root="$(mktemp -d)"
trap 'rm -rf "$temporary_root"' EXIT

passes=0
pass() {
  passes=$((passes + 1))
  printf 'ok %d - %s\n' "$passes" "$1"
}

expect_failure() {
  local description="$1"
  shift
  if "$@" > /dev/null 2>&1; then
    printf 'not ok - %s unexpectedly succeeded\n' "$description" >&2
    exit 1
  fi
  pass "$description"
}

fixture_repo="$temporary_root/repo"
mkdir -p "$fixture_repo/scripts" "$fixture_repo/release"
fixture_bin="$temporary_root/bin"
mkdir -m 700 "$fixture_bin"
# Exercise the Darwin-only production branch with fixture verifiers on any CI
# platform. The production harness itself has no test switch.
# shellcheck disable=SC2016
printf '%s\n' '#!/usr/bin/env bash' 'if [[ "${1:-}" == -s ]]; then printf "Darwin\n"; else exec /usr/bin/uname "$@"; fi' \
  > "$fixture_bin/uname"
chmod 755 "$fixture_bin/uname"
export PATH="$fixture_bin:$PATH"
cp "$production_harness" "$fixture_repo/scripts/public-alpha-uat.sh"
chmod 755 "$fixture_repo/scripts/public-alpha-uat.sh"
printf '%s\n' 'untrusted comment: fixture key' 'RWRmaXh0dXJlUHVibGljS2V5MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTI=' \
  > "$fixture_repo/release/sigil-minisign.pub"
chmod 644 "$fixture_repo/release/sigil-minisign.pub"

# The single-quoted fixture lines intentionally defer expansion to the generated verifier.
# shellcheck disable=SC2016
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'tag=""; archive=""; commit=""; key=""' \
  'while (($#)); do' \
  '  case "$1" in' \
  '    --tag) tag="$2"; shift 2 ;;' \
  '    --archive) archive="$2"; shift 2 ;;' \
  '    --source-commit) commit="$2"; shift 2 ;;' \
  '    --public-key-file) key="$2"; shift 2 ;;' \
  '    *) exit 1 ;;' \
  '  esac' \
  'done' \
  '[[ "$tag" == v1.2.3 ]]' \
  '[[ "$(basename -- "$archive")" == sigil-v1.2.3-bazzite-x86_64.tar.gz ]]' \
  '[[ "$commit" =~ ^[0-9a-f]{40}$ ]]' \
  '[[ -s "$archive.sha256" && "$(<"$archive.minisig")" == fixture-signature ]]' \
  'grep -q fixture "$key"' \
  > "$fixture_repo/scripts/verify-sigil-release.sh"
chmod 755 "$fixture_repo/scripts/verify-sigil-release.sh"

printf '%s\n' \
  '#!/usr/bin/env python3' \
  'import pathlib' \
  'import sys' \
  'args = sys.argv[1:]' \
  'def value(name):' \
  '    return args[args.index(name) + 1]' \
  'if not args or args[0] != "assets": raise SystemExit(1)' \
  'if value("--release-tag") != "v1.2.3": raise SystemExit(1)' \
  'asset_dir = pathlib.Path(value("--asset-dir"))' \
  'dmg = asset_dir / "Portal-1.2.3-arm64.dmg"' \
  'expected = {"Portal-1.2.3-arm64.dmg", "Portal-1.2.3-arm64.dmg.sha256", "Portal-1.2.3-arm64.json"}' \
  'if {entry.name for entry in asset_dir.iterdir()} != expected: raise SystemExit(1)' \
  'if dmg.read_text() != "verified portal dmg fixture\n": raise SystemExit(1)' \
  > "$fixture_repo/scripts/verify-portal-release.py"
chmod 755 "$fixture_repo/scripts/verify-portal-release.py"

# The single-quoted fixture lines intentionally defer expansion to the generated verifier.
# shellcheck disable=SC2016
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'if [[ -n "${UAT_SIGNATURE_MARKER:-}" && -e "$UAT_SIGNATURE_MARKER.fail" ]]; then exit 1; fi' \
  '[[ "$1" == --dmg && "$3" == --expected-version && "$4" == 1.2.3 ]]' \
  '[[ "$(<"$2")" == "verified portal dmg fixture" ]]' \
  'if [[ -n "${UAT_SIGNATURE_MARKER:-}" ]]; then printf invoked > "$UAT_SIGNATURE_MARKER"; fi' \
  'printf "portal_signature_verification=ok\\n"' \
  > "$fixture_repo/scripts/verify-macos-portal-signature.sh"
chmod 755 "$fixture_repo/scripts/verify-macos-portal-signature.sh"

release_tag=v1.2.3
sigil_bin="$temporary_root/sigil"
wrong_sigil_bin="$temporary_root/wrong-sigil"
printf '%s\n' 'exact Sigil release candidate' > "$sigil_bin"
printf '%s\n' 'arbitrary executable' > "$wrong_sigil_bin"
chmod 600 "$sigil_bin" "$wrong_sigil_bin"

archive_dir="$temporary_root/sigil-assets"
payload_root="$temporary_root/payload-root"
mkdir -p "$archive_dir" "$payload_root/payload/release"
cp "$sigil_bin" "$payload_root/payload/release/sigil"
sigil_archive="$archive_dir/sigil-v1.2.3-bazzite-x86_64.tar.gz"
tar -czf "$sigil_archive" -C "$payload_root" payload
printf '%s\n' 'fixture checksum' > "$sigil_archive.sha256"
printf '%s\n' 'fixture-signature' > "$sigil_archive.minisig"
chmod 600 "$sigil_archive" "$sigil_archive.sha256" "$sigil_archive.minisig"

portal_assets="$temporary_root/portal-assets"
mkdir -m 700 "$portal_assets"
printf '%s\n' 'verified portal dmg fixture' > "$portal_assets/Portal-1.2.3-arm64.dmg"
printf '%s\n' 'fixture checksum' > "$portal_assets/Portal-1.2.3-arm64.dmg.sha256"
printf '%s\n' '{}' > "$portal_assets/Portal-1.2.3-arm64.json"
chmod 600 "$portal_assets"/*

git -C "$fixture_repo" init -q
git -C "$fixture_repo" config user.name 'UAT Fixture'
git -C "$fixture_repo" config user.email 'uat-fixture@example.invalid'
git -C "$fixture_repo" add scripts release
git -C "$fixture_repo" commit -qm 'fixture release verifiers'
git -C "$fixture_repo" tag "$release_tag"

harness="$fixture_repo/scripts/public-alpha-uat.sh"
signature_marker="$temporary_root/signature-verifier-invoked"
export UAT_SIGNATURE_MARKER="$signature_marker"

manifest_value() {
  local bundle="$1"
  local key="$2"
  awk -F '\t' -v wanted="$key" '$1 == wanted { print $2 }' "$bundle/manifest.tsv"
}

new_bundle() {
  local name="$1"
  local max_age="${2:-604800}"
  local bundle="$temporary_root/$name"
  "$harness" init \
    --evidence-dir "$bundle" \
    --release-tag "$release_tag" \
    --sigil-archive "$sigil_archive" \
    --sigil-bin "$sigil_bin" \
    --portal-assets "$portal_assets" \
    --max-age-seconds "$max_age" > /dev/null
  printf '%s\n' "$bundle"
}

write_common() {
  local bundle="$1"
  local kind="$2"
  local observed="$3"
  local output="$4"
  {
    printf 'uat_schema=goq-public-alpha-evidence-v2\n'
    printf 'evidence_kind=%s\n' "$kind"
    printf 'observed_at_unix=%s\n' "$observed"
    printf 'git_commit=%s\n' "$(manifest_value "$bundle" git_commit)"
    printf 'release_tag=%s\n' "$(manifest_value "$bundle" release_tag)"
    printf 'sigil_sha256=%s\n' "$(manifest_value "$bundle" sigil_sha256)"
    printf 'portal_sha256=%s\n' "$(manifest_value "$bundle" portal_sha256)"
  } > "$output"
}

write_fixture() {
  local bundle="$1"
  local kind="$2"
  local output="$3"
  local observed="${4:-$(date +%s)}"
  local hitch_p99="${5:-40}"
  write_common "$bundle" "$kind" "$observed" "$output"
  case "$kind" in
    cold-boot)
      printf '%s\n' 'cold_boot_result=pass' 'cold_boot_failure_count=0' \
        'cold_boot_insufficient_count=0' 'headless_connector_state=ok' \
        'gaming_autologin_session=ok' 'sigil_host_enabled=enabled' \
        'sigil_host_active=active' 'gamescope_pipewire_node=ok' \
        'gamescope_before_first_ssh=ok' 'sigil_unit_before_first_ssh=ok' \
        'sigil_ready_before_first_ssh=ok' >> "$output"
      ;;
    controller)
      printf '%s\n' 'physical_controller_attached_to_portal=pass' \
        'actual_game_controlled=pass' \
        'controller_coverage=abxy,dpad,sticks,triggers,shoulders,start-back' \
        'neutral_release_on_disconnect=pass' 'neutral_buttons=pass' \
        'neutral_axes=pass' 'session_seconds=600' >> "$output"
      ;;
    mouse)
      printf '%s\n' 'target_application=actual-game' 'left_click_consumed=pass' \
        'right_click_consumed=pass' 'consumption_observed_in_target=pass' \
        'click_attempts=10' >> "$output"
      ;;
    soak)
      printf '%s\n' 'duration_seconds=7200' 'samples=120' 'capture_fps_p50=59.8' \
        'presentation_fps_p50=59.2' 'frame_interval_p95_ms=20' \
        "hitch_p99_ms=$hitch_p99" 'video_queue_p95_frames=1' \
        'decode_queue_p95_frames=1' 'audio_queue_p95_ms=50' 'av_skew_p95_ms=30' \
        'max_queue_age_p95_ms=45' 'cpu_p95_percent=50' 'gpu_p95_percent=70' \
        'rss_p95_mib=512' 'transport_drops=2' 'frontend_drops=3' 'audio_drops=0' \
        'latency_first_window_p95_ms=30' 'latency_last_window_p95_ms=33' \
        'disconnects=0' >> "$output"
      ;;
    network-direct)
      printf '%s\n' 'path_mode=direct' 'nat_scenario=ordinary' 'session_seconds=900' \
        'rtt_p50_ms=10' 'rtt_p95_ms=20' 'input_ack_p95_ms=30' \
        'presentation_latency_p95_ms=60' 'packet_loss_percent=0.1' >> "$output"
      ;;
    network-relay)
      printf '%s\n' 'path_mode=relay' 'nat_scenario=difficult' 'session_seconds=900' \
        'rtt_p50_ms=40' 'rtt_p95_ms=80' 'input_ack_p95_ms=90' \
        'presentation_latency_p95_ms=120' 'packet_loss_percent=1.0' >> "$output"
      ;;
    reconnect)
      printf '%s\n' 'reconnect_cycles=10' 'reconnect_successes=10' \
        'reconnect_failures=0' 'state_preserved=pass' \
        'keyframe_recovery_p95_ms=900' >> "$output"
      ;;
    second-client)
      printf '%s\n' 'second_client_attempts=3' 'second_client_rejections=3' \
        'authorized_primary_uninterrupted=pass' 'rejection_reason=active-client' >> "$output"
      ;;
    loopback-preflight)
      printf '%s\n' 'loopback_proof=ok' 'profile=release' \
        "host_sha256=$(manifest_value "$bundle" sigil_sha256)" \
        'active_client_rejection=ok' 'reconnect_cycles=3' 'cleanup=ok' >> "$output"
      ;;
    *) printf 'unknown fixture kind: %s\n' "$kind" >&2; exit 1 ;;
  esac
  chmod 600 "$output"
}

record_all_required() {
  local bundle="$1"
  local kind
  local evidence
  for kind in cold-boot controller mouse soak network-direct network-relay reconnect second-client; do
    evidence="$temporary_root/$kind-$RANDOM.evidence"
    write_fixture "$bundle" "$kind" "$evidence"
    "$harness" record --evidence-dir "$bundle" --kind "$kind" --file "$evidence" > /dev/null
  done
}

verify_bundle() {
  "$harness" verify --evidence-dir "$1" --sigil-archive "$sigil_archive" \
    --sigil-bin "$sigil_bin" --portal-assets "$portal_assets"
}

complete_bundle="$(new_bundle complete)"
[[ -f "$signature_marker" ]] || { printf 'macOS signature verifier was not invoked\n' >&2; exit 1; }
record_all_required "$complete_bundle"
loopback_source="$temporary_root/loopback.evidence"
write_fixture "$complete_bundle" loopback-preflight "$loopback_source"
"$harness" record --evidence-dir "$complete_bundle" --kind loopback-preflight \
  --file "$loopback_source" > /dev/null
verify_bundle "$complete_bundle" | grep '^public_alpha_uat=pass$' > /dev/null
pass 'complete bundle verifies only after all release verifiers run'

expect_failure 'verify rejects an executable different from the signed Sigil payload' \
  "$harness" verify --evidence-dir "$complete_bundle" --sigil-archive "$sigil_archive" \
    --sigil-bin "$wrong_sigil_bin" --portal-assets "$portal_assets"

printf failed > "$signature_marker.fail"
expect_failure 'UAT fails closed when macOS platform verification cannot run' \
  "$harness" init \
    --evidence-dir "$temporary_root/signature-failure" --release-tag "$release_tag" \
    --sigil-archive "$sigil_archive" --sigil-bin "$sigil_bin" --portal-assets "$portal_assets"
rm "$signature_marker.fail"

expect_failure 'HEAD is rejected in place of an immutable release tag' \
  "$harness" init --evidence-dir "$temporary_root/head" --release-tag HEAD \
    --sigil-archive "$sigil_archive" --sigil-bin "$sigil_bin" --portal-assets "$portal_assets"

printf '%s\n' dirty > "$fixture_repo/untracked-release-residue"
expect_failure 'a dirty exact-tag worktree is rejected' \
  "$harness" init --evidence-dir "$temporary_root/dirty-tag" --release-tag "$release_tag" \
    --sigil-archive "$sigil_archive" --sigil-bin "$sigil_bin" --portal-assets "$portal_assets"
rm "$fixture_repo/untracked-release-residue"

arbitrary_dir="$temporary_root/arbitrary-sigil"
mkdir "$arbitrary_dir"
arbitrary_archive="$arbitrary_dir/sigil-v1.2.3-bazzite-x86_64.tar.gz"
printf '%s\n' 'arbitrary text archive' > "$arbitrary_archive"
printf '%s\n' 'fixture checksum' > "$arbitrary_archive.sha256"
printf '%s\n' 'fixture-signature' > "$arbitrary_archive.minisig"
chmod 600 "$arbitrary_archive" "$arbitrary_archive.sha256" "$arbitrary_archive.minisig"
expect_failure 'an arbitrary text file cannot stand in for the signed Sigil archive' \
  "$harness" init --evidence-dir "$temporary_root/arbitrary-archive-bundle" \
    --release-tag "$release_tag" --sigil-archive "$arbitrary_archive" \
    --sigil-bin "$sigil_bin" --portal-assets "$portal_assets"

expect_failure 'verify reruns Sigil archive verification instead of trusting stored hashes' \
  "$harness" verify --evidence-dir "$complete_bundle" --sigil-archive "$arbitrary_archive" \
    --sigil-bin "$sigil_bin" --portal-assets "$portal_assets"

expect_failure 'a different Sigil executable is rejected against the archive payload' \
  "$harness" init --evidence-dir "$temporary_root/wrong-sigil-bundle" \
    --release-tag "$release_tag" --sigil-archive "$sigil_archive" \
    --sigil-bin "$wrong_sigil_bin" --portal-assets "$portal_assets"

arbitrary_portal="$temporary_root/arbitrary-portal"
mkdir -m 700 "$arbitrary_portal"
printf '%s\n' 'arbitrary DMG' > "$arbitrary_portal/Portal-1.2.3-arm64.dmg"
printf '%s\n' 'arbitrary checksum' > "$arbitrary_portal/Portal-1.2.3-arm64.dmg.sha256"
printf '%s\n' '{}' > "$arbitrary_portal/Portal-1.2.3-arm64.json"
chmod 600 "$arbitrary_portal"/*
expect_failure 'arbitrary Portal files are rejected by release policy verification' \
  "$harness" init --evidence-dir "$temporary_root/arbitrary-portal-bundle" \
    --release-tag "$release_tag" --sigil-archive "$sigil_archive" \
    --sigil-bin "$sigil_bin" --portal-assets "$arbitrary_portal"

expect_failure 'verify reruns Portal release and platform verification' \
  "$harness" verify --evidence-dir "$complete_bundle" --sigil-archive "$sigil_archive" \
    --sigil-bin "$sigil_bin" --portal-assets "$arbitrary_portal"

missing_bundle="$(new_bundle missing)"
expect_failure 'missing hardware gates fail closed' verify_bundle "$missing_bundle"

stale_bundle="$(new_bundle stale 3600)"
stale_source="$temporary_root/stale.evidence"
write_fixture "$stale_bundle" controller "$stale_source" "$(( $(date +%s) - 3601 ))"
expect_failure 'stale evidence is rejected at ingestion' \
  "$harness" record --evidence-dir "$stale_bundle" --kind controller --file "$stale_source"

sensitive_bundle="$(new_bundle sensitive)"
sensitive_source="$temporary_root/sensitive.evidence"
write_fixture "$sensitive_bundle" mouse "$sensitive_source"
printf 'node_id=0123456789abcdef\n' >> "$sensitive_source"
expect_failure 'node IDs are rejected before normalization' \
  "$harness" record --evidence-dir "$sensitive_bundle" --kind mouse --file "$sensitive_source"

permissions_bundle="$(new_bundle permissions)"
permissions_source="$temporary_root/permissions.evidence"
write_fixture "$permissions_bundle" controller "$permissions_source"
chmod 666 "$permissions_source"
expect_failure 'group- or other-writable evidence is rejected' \
  "$harness" record --evidence-dir "$permissions_bundle" --kind controller --file "$permissions_source"

threshold_bundle="$(new_bundle threshold)"
threshold_source="$temporary_root/threshold.evidence"
write_fixture "$threshold_bundle" soak "$threshold_source" "$(date +%s)" 51
expect_failure 'a soak percentile over threshold is rejected' \
  "$harness" record --evidence-dir "$threshold_bundle" --kind soak --file "$threshold_source"

printf 'tamper=1\n' >> "$complete_bundle/records/controller.evidence"
expect_failure 'post-ingestion evidence tampering is rejected' verify_bundle "$complete_bundle"

unexpected_bundle="$(new_bundle unexpected)"
record_all_required "$unexpected_bundle"
printf '%s\n' 'not allowed' > "$unexpected_bundle/untracked.txt"
chmod 600 "$unexpected_bundle/untracked.txt"
expect_failure 'unexpected bundle content is rejected' verify_bundle "$unexpected_bundle"

printf '1..%d\n' "$passes"
