#!/usr/bin/env bash
# shellcheck disable=SC2016
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(cd -- "$script_dir/../.." && pwd -P)"
workflow="$repo_dir/.github/workflows/ci.yml"
gate="$repo_dir/scripts/verify-demo-build.sh"
demo_job="$(sed -n '/^  demo-gate:/,$p' "$workflow")"

[[ -n "$demo_job" ]] || {
  echo 'CI cross-build policy failed: ordinary CI lacks the demo-gate job' >&2
  exit 1
}

fail() {
  printf 'CI cross-build policy failed: %s\n' "$*" >&2
  exit 1
}

require_workflow_text() {
  local expected="$1"
  grep -Fq -- "$expected" <<<"$demo_job" \
    || fail "ordinary CI is missing required text: $expected"
}

require_gate_text() {
  local expected="$1"
  grep -Fq -- "$expected" "$gate" \
    || fail "repository gate is missing required text: $expected"
}

require_workflow_text 'python3-venv'
require_workflow_text \
  'uses: actions/cache@0400d5f644dc74513175e3cd8d07132dd4860809 # v4.2.4'
require_workflow_text 'path: ~/.cargo/bin/cargo-zigbuild'
require_workflow_text \
  'key: cargo-zigbuild-${{ runner.os }}-${{ runner.arch }}-0.23.0'
require_workflow_text \
  'ziglang==0.16.0 --hash=sha256:9fcda73f62b851dd72a54b710ad40a209896db14cfb13649e62191243556342b'
require_workflow_text '--only-binary=:all:'
require_workflow_text '--require-hashes'
require_workflow_text \
  'exec "$RUNNER_TEMP/zig-venv/bin/python" -m ziglang "$@"'
require_workflow_text 'cargo install cargo-zigbuild --locked --version 0.23.0'
require_workflow_text 'test "$(zig version)" = 0.16.0'
require_workflow_text \
  "test \"\$(cargo-zigbuild --version)\" = 'cargo-zigbuild 0.23.0'"
require_workflow_text 'GOQ_REQUIRE_LINUX_CROSS_BUILD: "1"'

require_gate_text 'case "${GOQ_REQUIRE_LINUX_CROSS_BUILD:-0}" in'
require_gate_text \
  'if [[ "${GOQ_REQUIRE_LINUX_CROSS_BUILD:-0}" == 1 ]]; then'
require_gate_text \
  'for cross_command in cargo-zigbuild zig; do'
require_gate_text \
  'required Linux cross-build command is missing: %s'
require_gate_text \
  'cargo zigbuild --locked -p sigil-host --bins'
require_gate_text \
  '--target x86_64-unknown-linux-gnu.2.17'
require_gate_text "echo 'linux_cross_build=ok'"

install_step_line="$(
  grep -nF '      - name: Install pinned cross-build tools' "$workflow" \
    | cut -d: -f1
)"
gate_step_line="$(
  grep -nF '      - name: Run complete demo gate' "$workflow" \
    | cut -d: -f1
)"
[[ "$install_step_line" =~ ^[0-9]+$ ]] \
  || fail 'cross-build install step must occur exactly once'
[[ "$gate_step_line" =~ ^[0-9]+$ ]] \
  || fail 'complete demo gate step must occur exactly once'
((install_step_line < gate_step_line)) \
  || fail 'cross-build tools must be installed before the complete demo gate'

echo 'ci_cross_build_policy=ok'
