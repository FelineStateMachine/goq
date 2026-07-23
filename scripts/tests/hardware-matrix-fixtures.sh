#!/usr/bin/env bash

# Markdown field fixtures intentionally keep backticks literal.
# shellcheck disable=SC2016
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
validator="$script_dir/hardware-matrix.sh"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/goq-hardware-matrix.XXXXXX")"

cleanup() {
  rm -rf -- "$fixture_root"
}
trap cleanup EXIT

fail() {
  printf 'hardware matrix fixture test failed: %s\n' "$*" >&2
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

write_evidence() {
  local path="$1"
  local row_id="$2"
  local commit="$3"
  local os_family="$4"
  local topology="$5"
  local connector_state="$6"
  local native_width="$7"
  local native_height="$8"

  printf '%s\n' \
    'schema=goq-hardware-matrix-evidence-v1' \
    "matrix_row=$row_id" \
    "exact_commit=$commit" \
    'workflow_run_id=29999999999' \
    "host_model=Fixture $row_id" \
    "os_family=$os_family" \
    "gpu_topology=$topology" \
    "connector_state=$connector_state" \
    "native_width=$native_width" \
    "native_height=$native_height" \
    'native_refresh_millihz=60000' \
    'os_version=fixture-os-1' \
    'kernel_version=fixture-kernel-1' \
    'gamescope_version=fixture-gamescope-1' \
    'mesa_version=fixture-mesa-1' \
    'gstreamer_version=fixture-gstreamer-1' \
    'encoder_plugin_version=fixture-encoder-1' \
    'render_node=/dev/dri/renderD129' \
    'encoder_factory=varenderD129h264enc' \
    'fixed_capture_result=pass' \
    'fixed_capture_fps_milli=60000' \
    'fixed_post_encode_drops=0' \
    'native_capture_result=pass' \
    'native_capture_fps_milli=60000' \
    'native_post_encode_drops=0' \
    'portal_transport=iroh-moq' \
    'portal_video=pass' \
    'portal_audio=pass' \
    'portal_input=pass' \
    'portal_reconnect=pass' \
    'portal_second_client_rejection=pass' \
    'restoration=pass' \
    >"$path"
}

write_report() {
  local path="$1"
  local row_id="$2"
  local commit="$3"
  local artifact_set="$4"
  local manifest="$5"
  local evidence="$6"

  {
    printf '# Fixture hardware report\n\n'
    printf -- '- Matrix row: `%s`\n' "$row_id"
    printf -- '- Exact commit: `%s`\n' "$commit"
    printf -- '- Workflow run: `29999999999`\n'
    printf -- '- Candidate artifact set SHA256: `%s`\n' "$artifact_set"
    printf -- '- Candidate artifact manifest: `%s`\n' "$manifest"
    printf -- '- Evidence summary: `%s`\n' "$evidence"
  } >"$path"
}

write_pass_matrix() {
  local path="$1"
  local prefix="$2"

  printf '%s\n' \
    '# Fixture matrix' \
    '' \
    '| Row ID | Required class | Status | Evidence report |' \
    '| --- | --- | --- | --- |' \
    "| native-1280x800-handheld | handheld | pass | docs/hardware-uat/$prefix/native-1280x800-handheld/REPORT.md |" \
    "| physically-headless-desktop-dgpu | desktop | pass | docs/hardware-uat/$prefix/physically-headless-desktop-dgpu/REPORT.md |" \
    >"$path"
}

write_pending_matrix() {
  local path="$1"

  printf '%s\n' \
    '# Fixture matrix' \
    '' \
    '| Row ID | Required class | Status | Evidence report |' \
    '| --- | --- | --- | --- |' \
    '| native-1280x800-handheld | handheld | pending | pending |' \
    '| physically-headless-desktop-dgpu | desktop | pending | pending |' \
    >"$path"
}

make_fixture() {
  local root="$1"
  local commit
  local prefix
  local manifest
  local manifest_relative
  local artifact_set
  local row_id
  local row_dir

  mkdir -p "$root/docs/hardware-uat" "$root/website"
  git -C "$root" init --quiet
  git -C "$root" config user.name 'Hardware Matrix Fixture'
  git -C "$root" config user.email 'fixture@example.invalid'
  printf 'fixture source\n' >"$root/source.txt"
  git -C "$root" add source.txt
  git -C "$root" commit --quiet -m 'fixture source'
  commit="$(git -C "$root" rev-parse HEAD)"
  prefix="${commit:0:12}"
  mkdir -p "$root/docs/hardware-uat/$prefix"

  manifest_relative="docs/hardware-uat/$prefix/candidate-artifacts.env"
  manifest="$root/$manifest_relative"
  printf '%s\n' \
    'schema=goq-hardware-matrix-artifacts-v1' \
    "exact_commit=$commit" \
    'candidate_kind=development' \
    'sigil_asset=sigil-fixture.tar.zst' \
    'sigil_sha256=1111111111111111111111111111111111111111111111111111111111111111' \
    'portal_asset=Portal-fixture.dmg' \
    'portal_sha256=2222222222222222222222222222222222222222222222222222222222222222' \
    >"$manifest"
  artifact_set="$(sha256_file "$manifest")"

  for row_id in \
    native-1280x800-handheld \
    physically-headless-desktop-dgpu; do
    row_dir="$root/docs/hardware-uat/$prefix/$row_id"
    mkdir -p "$row_dir"
    if [[ "$row_id" == native-1280x800-handheld ]]; then
      write_evidence \
        "$row_dir/EVIDENCE.env" "$row_id" "$commit" \
        steamos-upstream integrated native-panel-connected 1280 800
    else
      write_evidence \
        "$row_dir/EVIDENCE.env" "$row_id" "$commit" \
        steamos-inspired discrete physically-headless 1920 1080
    fi
    write_report \
      "$row_dir/REPORT.md" "$row_id" "$commit" "$artifact_set" \
      "$manifest_relative" \
      "docs/hardware-uat/$prefix/$row_id/EVIDENCE.env"
  done

  write_pass_matrix "$root/docs/hardware-uat/MATRIX.md" "$prefix"
  printf 'Goq hardware evidence remains explicitly qualified.\n' >"$root/README.md"
}

remove_exact_line() {
  local path="$1"
  local exact_line="$2"
  local output="$path.next"

  awk -v exact_line="$exact_line" '$0 != exact_line { print }' "$path" >"$output"
  mv "$output" "$path"
}

replace_text() {
  local path="$1"
  local old="$2"
  local new="$3"
  local output="$path.next"

  awk -v old="$old" -v new="$new" '{ gsub(old, new); print }' "$path" >"$output"
  mv "$output" "$path"
}

expect_pass() {
  local label="$1"
  local root="$2"
  local log="$root/validator.log"

  GOQ_HARDWARE_MATRIX_REPO_DIR="$root" "$validator" >"$log" 2>&1 \
    || {
      cat "$log" >&2
      fail "$label should pass"
    }
  grep -qx 'hardware_matrix_tests=ok' "$log" \
    || fail "$label did not report success"
}

expect_fail() {
  local label="$1"
  local root="$2"
  local log="$root/validator.log"

  if GOQ_HARDWARE_MATRIX_REPO_DIR="$root" "$validator" >"$log" 2>&1; then
    cat "$log" >&2
    fail "$label should fail"
  fi
  [[ -s "$log" ]] || fail "$label failed without a diagnostic"
}

case_dir="$fixture_root/valid"
make_fixture "$case_dir"
expect_pass valid-complete-evidence "$case_dir"

case_dir="$fixture_root/incomplete-report"
make_fixture "$case_dir"
commit="$(git -C "$case_dir" rev-parse HEAD)"
report="$case_dir/docs/hardware-uat/${commit:0:12}/native-1280x800-handheld/REPORT.md"
remove_exact_line "$report" '- Workflow run: `29999999999`'
expect_fail incomplete-report "$case_dir"

case_dir="$fixture_root/incomplete-evidence"
make_fixture "$case_dir"
commit="$(git -C "$case_dir" rev-parse HEAD)"
evidence="$case_dir/docs/hardware-uat/${commit:0:12}/native-1280x800-handheld/EVIDENCE.env"
remove_exact_line "$evidence" 'portal_audio=pass'
expect_fail incomplete-evidence "$case_dir"

case_dir="$fixture_root/duplicate-evidence"
make_fixture "$case_dir"
commit="$(git -C "$case_dir" rev-parse HEAD)"
evidence="$case_dir/docs/hardware-uat/${commit:0:12}/native-1280x800-handheld/EVIDENCE.env"
printf 'portal_audio=pass\n' >>"$evidence"
expect_fail duplicate-evidence "$case_dir"

case_dir="$fixture_root/fabricated-commit"
make_fixture "$case_dir"
commit="$(git -C "$case_dir" rev-parse HEAD)"
report="$case_dir/docs/hardware-uat/${commit:0:12}/native-1280x800-handheld/REPORT.md"
replace_text "$report" "$commit" '0000000000000000000000000000000000000000'
expect_fail fabricated-commit "$case_dir"

case_dir="$fixture_root/fabricated-artifact-set"
make_fixture "$case_dir"
commit="$(git -C "$case_dir" rev-parse HEAD)"
report="$case_dir/docs/hardware-uat/${commit:0:12}/native-1280x800-handheld/REPORT.md"
artifact_set="$(
  sed -n 's/^- Candidate artifact set SHA256: `\([0-9a-f]\{64\}\)`$/\1/p' \
    "$report"
)"
replace_text \
  "$report" "$artifact_set" \
  '0000000000000000000000000000000000000000000000000000000000000000'
expect_fail fabricated-artifact-set "$case_dir"

case_dir="$fixture_root/wrong-row-topology"
make_fixture "$case_dir"
commit="$(git -C "$case_dir" rev-parse HEAD)"
evidence="$case_dir/docs/hardware-uat/${commit:0:12}/native-1280x800-handheld/EVIDENCE.env"
replace_text "$evidence" 'gpu_topology=integrated' 'gpu_topology=discrete'
expect_fail wrong-row-topology "$case_dir"

case_dir="$fixture_root/pending-matrix-claim"
make_fixture "$case_dir"
write_pending_matrix "$case_dir/docs/hardware-uat/MATRIX.md"
printf 'Goq is matrix-proven on all supported hardware.\n' \
  >"$case_dir/docs/public-claim.md"
expect_fail pending-matrix-claim "$case_dir"

case_dir="$fixture_root/pending-hardware-claim"
make_fixture "$case_dir"
write_pending_matrix "$case_dir/docs/hardware-uat/MATRIX.md"
printf '<p>Goq is hardware-proven.</p>\n' >"$case_dir/website/index.html"
expect_fail pending-hardware-claim "$case_dir"

case_dir="$fixture_root/development-release-claim"
make_fixture "$case_dir"
printf 'Goq is release-matrix-proven.\n' >"$case_dir/docs/public-claim.md"
expect_fail development-release-claim "$case_dir"

echo 'hardware_matrix_fixture_tests=ok'
