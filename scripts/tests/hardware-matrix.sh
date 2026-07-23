#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd "$script_dir/../.." && pwd)"
matrix="$repo_dir/docs/hardware-uat/MATRIX.md"

fail() {
  printf 'hardware matrix test failed: %s\n' "$*" >&2
  exit 1
}

[[ -f "$matrix" ]] || fail "missing docs/hardware-uat/MATRIX.md"

row_ids=(
  native-1280x800-handheld
  physically-headless-desktop-dgpu
)
pass_commits=()
pass_artifact_sets=()
pass_reports=()
pending=false

trim() {
  sed 's/^[[:space:]]*//; s/[[:space:]]*$//'
}

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
      [[ "$report" =~ ^docs/hardware-uat/[0-9a-f]{12}/REPORT\.md$ ]] \
        || fail "passing row has an invalid report path: $row_id"
      [[ -f "$repo_dir/$report" ]] \
        || fail "passing row report does not exist: $report"
      grep -Fqx -- "- Matrix row: \`$row_id\`" "$repo_dir/$report" \
        || fail "passing report does not bind its matrix row: $report"
      # The single quotes deliberately keep Markdown backticks literal.
      # shellcheck disable=SC2016
      commit="$(sed -n 's/^- Exact commit: `\([0-9a-f]\{40\}\)`$/\1/p' \
        "$repo_dir/$report")"
      [[ "$commit" =~ ^[0-9a-f]{40}$ ]] \
        || fail "passing report does not bind one exact commit: $report"
      # shellcheck disable=SC2016
      artifact_set="$(sed -n \
        's/^- Candidate artifact set SHA256: `\([0-9a-f]\{64\}\)`$/\1/p' \
        "$repo_dir/$report")"
      [[ "$artifact_set" =~ ^[0-9a-f]{64}$ ]] \
        || fail "passing report does not bind one candidate artifact set: $report"
      pass_commits+=("$commit")
      pass_artifact_sets+=("$artifact_set")
      pass_reports+=("$report")
      ;;
    *)
      fail "row status must be pending or pass: $row_id"
      ;;
  esac
done

if [[ "$pending" == true ]]; then
  if grep -RinE --include='*.html' --include='*.md' \
    '(^|[^[:alnum:]_])hardware-proven([^[:alnum:]_]|$)' \
    "$repo_dir/README.md" "$repo_dir/website"; then
    fail "public claims must not say hardware-proven while matrix rows are pending"
  fi
else
  [[ "${#pass_commits[@]}" -eq "${#row_ids[@]}" ]] \
    || fail "every required row must have passing evidence"
  [[ "${pass_commits[0]}" == "${pass_commits[1]}" ]] \
    || fail "required rows do not prove the same exact commit"
  [[ "${pass_artifact_sets[0]}" == "${pass_artifact_sets[1]}" ]] \
    || fail "required rows do not prove the same candidate artifact set"
  [[ "${pass_reports[0]}" != "${pass_reports[1]}" ]] \
    || fail "required rows must have distinct host reports"
fi

echo 'hardware_matrix_tests=ok'
