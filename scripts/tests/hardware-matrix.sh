#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
default_repo_dir="$(cd "$script_dir/../.." && pwd)"
repo_dir="${GOQ_HARDWARE_MATRIX_REPO_DIR:-$default_repo_dir}"
matrix="$repo_dir/docs/hardware-uat/MATRIX.md"

fail() {
  printf 'hardware matrix test failed: %s\n' "$*" >&2
  exit 1
}

trim() {
  sed 's/^[[:space:]]*//; s/[[:space:]]*$//'
}

require_regular_file() {
  local path="$1"
  local description="$2"

  [[ -f "$path" && ! -L "$path" ]] || fail "$description is missing or unsafe: $path"
}

exact_report_value() {
  local report="$1"
  local field="$2"
  local prefix="- $field: \`"

  awk -v prefix="$prefix" '
    index($0, prefix) == 1 && substr($0, length($0), 1) == "`" {
      count += 1
      value = substr($0, length(prefix) + 1, length($0) - length(prefix) - 1)
    }
    END {
      if (count == 1 && length(value) > 0) {
        print value
      } else {
        exit 1
      }
    }
  ' "$report"
}

exact_env_value() {
  local document="$1"
  local key="$2"
  local count
  local value

  count="$(grep -c "^${key}=" "$document" || true)"
  [[ "$count" == 1 ]] || return 1
  value="$(sed -n "s/^${key}=//p" "$document")"
  [[ -n "$value" ]] || return 1
  printf '%s\n' "$value"
}

require_exact_env_schema() {
  local document="$1"
  shift
  local expected_keys=("$@")
  local actual_lines
  local key

  require_regular_file "$document" "structured evidence"
  actual_lines="$(wc -l <"$document" | tr -d '[:space:]')"
  [[ "$actual_lines" == "${#expected_keys[@]}" ]] \
    || fail "structured evidence has extra, missing, or unterminated fields: $document"
  for key in "${expected_keys[@]}"; do
    exact_env_value "$document" "$key" >/dev/null \
      || fail "structured evidence requires exactly one nonempty $key: $document"
  done
}

sha256_file() {
  local path="$1"

  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{print $1}'
  else
    shasum -a 256 "$path" | awk '{print $1}'
  fi
}

require_bounded_text() {
  local value="$1"
  local field="$2"

  [[ "${#value}" -ge 1 && "${#value}" -le 128 ]] \
    || fail "$field must contain 1-128 characters"
  [[ "$value" != *$'\n'* && "$value" != *$'\r'* ]] \
    || fail "$field contains a line break"
}

require_positive_integer() {
  local value="$1"
  local field="$2"

  [[ "$value" =~ ^[1-9][0-9]*$ ]] || fail "$field must be a positive integer"
}

require_regular_file "$matrix" "hardware matrix"
git -C "$repo_dir" rev-parse --git-dir >/dev/null 2>&1 \
  || fail "hardware matrix repository is not a Git worktree"

row_ids=(
  native-1280x800-handheld
  physically-headless-desktop-dgpu
)
manifest_keys=(
  schema
  exact_commit
  candidate_kind
  sigil_asset
  sigil_sha256
  portal_asset
  portal_sha256
)
evidence_keys=(
  schema
  matrix_row
  exact_commit
  workflow_run_id
  host_model
  os_family
  gpu_topology
  connector_state
  native_width
  native_height
  native_refresh_millihz
  os_version
  kernel_version
  gamescope_version
  mesa_version
  gstreamer_version
  encoder_plugin_version
  render_node
  encoder_factory
  fixed_capture_result
  fixed_capture_fps_milli
  fixed_post_encode_drops
  native_capture_result
  native_capture_fps_milli
  native_post_encode_drops
  portal_transport
  portal_video
  portal_audio
  portal_input
  portal_reconnect
  portal_second_client_rejection
  restoration
)
pass_commits=()
pass_artifact_sets=()
pass_manifests=()
pass_reports=()
pass_candidate_kinds=()
pending=false

for row_id in "${row_ids[@]}"; do
  row="$(awk -F '|' -v id="$row_id" '
    {
      key = $2
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", key)
      if (key == id) {
        count += 1
        value = $0
      }
    }
    END {
      if (count == 1) {
        print value
      }
    }
  ' "$matrix")"
  [[ -n "$row" ]] || fail "required row is missing or duplicated: $row_id"

  status="$(cut -d '|' -f 4 <<<"$row" | trim)"
  report="$(cut -d '|' -f 5 <<<"$row" | trim)"
  case "$status" in
    pending)
      [[ "$report" == pending ]] \
        || fail "pending row must use pending evidence: $row_id"
      pending=true
      ;;
    pass)
      [[ "$report" == "docs/hardware-uat/"[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]/"$row_id"/REPORT.md ]] \
        || fail "passing row has an invalid or cross-row report path: $row_id"
      report_path="$repo_dir/$report"
      require_regular_file "$report_path" "passing report"

      report_row="$(exact_report_value "$report_path" "Matrix row")" \
        || fail "passing report does not bind exactly one matrix row: $report"
      [[ "$report_row" == "$row_id" ]] \
        || fail "passing report binds the wrong matrix row: $report"
      commit="$(exact_report_value "$report_path" "Exact commit")" \
        || fail "passing report does not bind exactly one commit: $report"
      [[ "$commit" =~ ^[0-9a-f]{40}$ ]] \
        || fail "passing report commit is not exact lowercase hex: $report"
      git -C "$repo_dir" cat-file -e "$commit^{commit}" 2>/dev/null \
        || fail "passing report commit is not present in repository history: $report"
      [[ "$report" == "docs/hardware-uat/${commit:0:12}/$row_id/REPORT.md" ]] \
        || fail "passing report directory does not match its exact commit: $report"

      workflow_run="$(exact_report_value "$report_path" "Workflow run")" \
        || fail "passing report does not bind exactly one workflow run: $report"
      require_positive_integer "$workflow_run" "workflow run"
      artifact_set="$(exact_report_value \
        "$report_path" "Candidate artifact set SHA256")" \
        || fail "passing report does not bind exactly one candidate artifact set: $report"
      [[ "$artifact_set" =~ ^[0-9a-f]{64}$ ]] \
        || fail "candidate artifact set is not exact lowercase SHA256: $report"
      manifest="$(exact_report_value "$report_path" "Candidate artifact manifest")" \
        || fail "passing report does not bind exactly one artifact manifest: $report"
      expected_manifest="docs/hardware-uat/${commit:0:12}/candidate-artifacts.env"
      [[ "$manifest" == "$expected_manifest" ]] \
        || fail "passing report does not use its commit-scoped artifact manifest: $report"
      manifest_path="$repo_dir/$manifest"
      require_exact_env_schema "$manifest_path" "${manifest_keys[@]}"
      [[ "$(exact_env_value "$manifest_path" schema)" == \
        goq-hardware-matrix-artifacts-v1 ]] \
        || fail "candidate artifact manifest has the wrong schema: $manifest"
      [[ "$(exact_env_value "$manifest_path" exact_commit)" == "$commit" ]] \
        || fail "candidate artifact manifest is bound to another commit: $manifest"
      candidate_kind="$(exact_env_value "$manifest_path" candidate_kind)"
      [[ "$candidate_kind" == development || "$candidate_kind" == release ]] \
        || fail "candidate artifact kind must be development or release: $manifest"
      sigil_asset="$(exact_env_value "$manifest_path" sigil_asset)"
      portal_asset="$(exact_env_value "$manifest_path" portal_asset)"
      [[ "$sigil_asset" =~ ^[[:alnum:]][[:alnum:]_.+-]*$ ]] \
        || fail "Sigil candidate asset name is unsafe: $manifest"
      [[ "$portal_asset" =~ ^[[:alnum:]][[:alnum:]_.+-]*$ ]] \
        || fail "Portal candidate asset name is unsafe: $manifest"
      [[ "$sigil_asset" != "$portal_asset" ]] \
        || fail "Sigil and Portal candidate assets must be distinct: $manifest"
      sigil_sha256="$(exact_env_value "$manifest_path" sigil_sha256)"
      portal_sha256="$(exact_env_value "$manifest_path" portal_sha256)"
      [[ "$sigil_sha256" =~ ^[0-9a-f]{64}$ && "$sigil_sha256" =~ [1-9a-f] ]] \
        || fail "Sigil candidate digest is invalid: $manifest"
      [[ "$portal_sha256" =~ ^[0-9a-f]{64}$ && "$portal_sha256" =~ [1-9a-f] ]] \
        || fail "Portal candidate digest is invalid: $manifest"
      [[ "$(sha256_file "$manifest_path")" == "$artifact_set" ]] \
        || fail "candidate artifact set digest does not match its manifest: $report"

      evidence="$(exact_report_value "$report_path" "Evidence summary")" \
        || fail "passing report does not bind exactly one evidence summary: $report"
      expected_evidence="docs/hardware-uat/${commit:0:12}/$row_id/EVIDENCE.env"
      [[ "$evidence" == "$expected_evidence" ]] \
        || fail "passing report does not use its row-scoped evidence summary: $report"
      evidence_path="$repo_dir/$evidence"
      require_exact_env_schema "$evidence_path" "${evidence_keys[@]}"
      [[ "$(exact_env_value "$evidence_path" schema)" == \
        goq-hardware-matrix-evidence-v1 ]] \
        || fail "hardware evidence has the wrong schema: $evidence"
      [[ "$(exact_env_value "$evidence_path" matrix_row)" == "$row_id" ]] \
        || fail "hardware evidence is bound to another row: $evidence"
      [[ "$(exact_env_value "$evidence_path" exact_commit)" == "$commit" ]] \
        || fail "hardware evidence is bound to another commit: $evidence"
      [[ "$(exact_env_value "$evidence_path" workflow_run_id)" == "$workflow_run" ]] \
        || fail "hardware evidence is bound to another workflow run: $evidence"

      host_model="$(exact_env_value "$evidence_path" host_model)"
      require_bounded_text "$host_model" "host model"
      for version_key in \
        os_version kernel_version gamescope_version mesa_version \
        gstreamer_version encoder_plugin_version; do
        require_bounded_text \
          "$(exact_env_value "$evidence_path" "$version_key")" "$version_key"
      done
      os_family="$(exact_env_value "$evidence_path" os_family)"
      gpu_topology="$(exact_env_value "$evidence_path" gpu_topology)"
      connector_state="$(exact_env_value "$evidence_path" connector_state)"
      native_width="$(exact_env_value "$evidence_path" native_width)"
      native_height="$(exact_env_value "$evidence_path" native_height)"
      native_refresh="$(exact_env_value "$evidence_path" native_refresh_millihz)"
      require_positive_integer "$native_width" "native width"
      require_positive_integer "$native_height" "native height"
      require_positive_integer "$native_refresh" "native refresh"
      ((native_width <= 16384 && native_height <= 16384)) \
        || fail "native dimensions exceed the protocol bound: $evidence"
      ((native_refresh <= 1000000)) \
        || fail "native refresh exceeds the evidence bound: $evidence"

      case "$row_id" in
        native-1280x800-handheld)
          [[ "$os_family" == steamos-upstream ]] \
            || fail "handheld row requires upstream SteamOS: $evidence"
          [[ "$gpu_topology" == integrated ]] \
            || fail "handheld row requires an integrated GPU: $evidence"
          [[ "$connector_state" == native-panel-connected ]] \
            || fail "handheld row requires its native panel: $evidence"
          [[ "$native_width" == 1280 && "$native_height" == 800 ]] \
            || fail "handheld row requires a native 1280x800 mode: $evidence"
          ;;
        physically-headless-desktop-dgpu)
          [[ "$os_family" == steamos-upstream || \
            "$os_family" == steamos-inspired ]] \
            || fail "desktop row requires SteamOS or a SteamOS-inspired OS: $evidence"
          [[ "$gpu_topology" == discrete ]] \
            || fail "desktop row requires a discrete GPU: $evidence"
          [[ "$connector_state" == physically-headless ]] \
            || fail "desktop row requires no physical connector: $evidence"
          ;;
      esac

      render_node="$(exact_env_value "$evidence_path" render_node)"
      encoder_factory="$(exact_env_value "$evidence_path" encoder_factory)"
      [[ "$render_node" =~ ^/dev/dri/renderD[0-9]+$ ]] \
        || fail "render node must be capability-discovered and explicit: $evidence"
      [[ "$encoder_factory" =~ ^[[:alnum:]_.+-]+$ ]] \
        || fail "encoder factory is invalid: $evidence"
      [[ "$(exact_env_value "$evidence_path" fixed_capture_result)" == pass ]] \
        || fail "fixed capture did not pass: $evidence"
      fixed_fps="$(exact_env_value "$evidence_path" fixed_capture_fps_milli)"
      require_positive_integer "$fixed_fps" "fixed capture fps"
      ((fixed_fps >= 55000)) \
        || fail "fixed capture did not sustain 55 fps: $evidence"
      [[ "$(exact_env_value "$evidence_path" fixed_post_encode_drops)" == 0 ]] \
        || fail "fixed capture has post-encode drops: $evidence"
      [[ "$(exact_env_value "$evidence_path" native_capture_result)" == pass ]] \
        || fail "native capture did not pass: $evidence"
      native_fps="$(exact_env_value "$evidence_path" native_capture_fps_milli)"
      require_positive_integer "$native_fps" "native capture fps"
      [[ "$(exact_env_value "$evidence_path" native_post_encode_drops)" == 0 ]] \
        || fail "native capture has post-encode drops: $evidence"
      [[ "$(exact_env_value "$evidence_path" portal_transport)" == iroh-moq ]] \
        || fail "Portal matrix session must use the preferred transport: $evidence"
      for pass_key in \
        portal_video portal_audio portal_input portal_reconnect \
        portal_second_client_rejection restoration; do
        [[ "$(exact_env_value "$evidence_path" "$pass_key")" == pass ]] \
          || fail "$pass_key did not pass: $evidence"
      done

      pass_commits+=("$commit")
      pass_artifact_sets+=("$artifact_set")
      pass_manifests+=("$manifest")
      pass_reports+=("$report")
      pass_candidate_kinds+=("$candidate_kind")
      ;;
    *)
      fail "row status must be pending or pass: $row_id"
      ;;
  esac
done

claim_files=()
for claim_root in "$repo_dir/README.md" "$repo_dir/website" "$repo_dir/docs"; do
  [[ -e "$claim_root" ]] || continue
  if [[ -f "$claim_root" ]]; then
    claim_files+=("$claim_root")
    continue
  fi
  while IFS= read -r claim_file; do
    [[ "$claim_file" == "$matrix" ]] || claim_files+=("$claim_file")
  done < <(find "$claim_root" -type f \( -name '*.md' -o -name '*.html' \) -print)
done

if [[ "$pending" == true ]]; then
  if ((${#claim_files[@]} > 0)) &&
    grep -HinE \
      '(^|[^[:alnum:]_])(matrix-proven|hardware-proven)([^[:alnum:]_]|$)' \
      "${claim_files[@]}"; then
    fail "public claims must not say matrix-proven or hardware-proven while rows are pending"
  fi
else
  [[ "${#pass_commits[@]}" -eq "${#row_ids[@]}" ]] \
    || fail "every required row must have passing evidence"
  [[ "${pass_commits[0]}" == "${pass_commits[1]}" ]] \
    || fail "required rows do not prove the same exact commit"
  [[ "${pass_artifact_sets[0]}" == "${pass_artifact_sets[1]}" ]] \
    || fail "required rows do not prove the same candidate artifact set"
  [[ "${pass_manifests[0]}" == "${pass_manifests[1]}" ]] \
    || fail "required rows do not use one shared candidate manifest"
  [[ "${pass_reports[0]}" != "${pass_reports[1]}" ]] \
    || fail "required rows must have distinct host reports"
  if [[ "${pass_candidate_kinds[0]}" != release || \
    "${pass_candidate_kinds[1]}" != release ]]; then
    if ((${#claim_files[@]} > 0)) &&
      grep -HinE \
        '(^|[^[:alnum:]_])release-matrix-proven([^[:alnum:]_]|$)' \
        "${claim_files[@]}"; then
      fail "release-matrix-proven requires release candidate evidence"
    fi
  fi
fi

echo 'hardware_matrix_tests=ok'
