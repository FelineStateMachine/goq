#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(cd -- "$script_dir/.." && pwd -P)"
cd "$repo_dir"

if [[ -f "$HOME/.cargo/env" ]]; then
  # shellcheck source=/dev/null
  source "$HOME/.cargo/env"
fi

# The complete gate is the ordered union of these stages. Running with no
# arguments runs every one of them, so the release and hardware-UAT workflows
# and a local `./scripts/verify-demo-build.sh` keep their exact prior coverage.
# CI runs the same stages as separate parallel jobs purely to cut wall clock;
# scripts/tests/demo-gate-stages.sh proves the union still spends every check.
#
# `quick` deliberately runs first: format, syntax, ShellCheck, and whitespace
# failures used to surface only after the multi-minute cross build had already
# run, which cost a full gate to learn about a missing newline.
readonly ALL_STAGES=(quick cross native gstreamer repo-tests loopback containment)
readonly LOOPBACK_CASES=(1 2 3 4)

usage() {
  cat >&2 <<'USAGE'
usage: verify-demo-build.sh [--stage STAGE] [--loopback-case NUMBER]

  --stage STAGE           Run a single stage instead of the complete gate.
                          One of: quick cross native gstreamer repo-tests
                          loopback containment all (default: all).
  --loopback-case NUMBER  Run a single loopback proof (1-4) instead of all
                          four. Requires --stage loopback.

With no arguments this runs the complete repository gate.
USAGE
}

stage=all
loopback_case=all

while [[ $# -gt 0 ]]; do
  case "$1" in
    --stage)
      [[ $# -ge 2 ]] || {
        printf -- '--stage requires a value\n' >&2
        exit 2
      }
      stage="$2"
      shift 2
      ;;
    --loopback-case)
      [[ $# -ge 2 ]] || {
        printf -- '--loopback-case requires a value\n' >&2
        exit 2
      }
      loopback_case="$2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      printf 'unknown argument: %s\n' "$1" >&2
      usage
      exit 2
      ;;
  esac
done

stage_is_known=false
for known_stage in "${ALL_STAGES[@]}" all; do
  if [[ "$stage" == "$known_stage" ]]; then
    stage_is_known=true
    break
  fi
done
if [[ "$stage_is_known" != true ]]; then
  printf 'unknown stage: %s\n' "$stage" >&2
  usage
  exit 2
fi

if [[ "$loopback_case" != all ]]; then
  if [[ "$stage" != loopback ]]; then
    printf -- '--loopback-case requires --stage loopback\n' >&2
    exit 2
  fi
  case_is_known=false
  for known_case in "${LOOPBACK_CASES[@]}"; do
    if [[ "$loopback_case" == "$known_case" ]]; then
      case_is_known=true
      break
    fi
  done
  if [[ "$case_is_known" != true ]]; then
    printf -- '--loopback-case must be one of %s\n' "${LOOPBACK_CASES[*]}" >&2
    exit 2
  fi
fi

case "${GOQ_VERIFY_IN_PROCESS_GSTREAMER:-0}" in
  0 | 1) ;;
  *)
    printf 'GOQ_VERIFY_IN_PROCESS_GSTREAMER must be 0 or 1\n' >&2
    exit 1
    ;;
esac

case "${GOQ_REQUIRE_LINUX_CROSS_BUILD:-0}" in
  0 | 1) ;;
  *)
    printf 'GOQ_REQUIRE_LINUX_CROSS_BUILD must be 0 or 1\n' >&2
    exit 1
    ;;
esac

require_commands() {
  local command_name
  for command_name in "$@"; do
    if ! command -v "$command_name" >/dev/null 2>&1; then
      printf 'required command is missing: %s\n' "$command_name" >&2
      exit 1
    fi
  done
}

require_rust_test() {
  local test_name="$1"
  shift
  local test_list

  test_list="$(cargo test --locked "$@" -- --list)"
  if ! grep -Eq "(^|::)${test_name}: test$" <<<"$test_list"; then
    printf 'required Rust test is not discoverable: %s\n' "$test_name" >&2
    exit 1
  fi
}

run_stage_quick() {
  require_commands cargo node git shellcheck
  cargo fmt --all -- --check
  while IFS= read -r frontend_source; do
    node --check "$frontend_source"
  done < <(find portal -maxdepth 1 -type f \( -name '*.js' -o -name '*.mjs' \) -print | sort)
  node --test portal/*.test.mjs
  find scripts -type f -name '*.sh' -exec shellcheck {} +
  git diff --check
  echo 'demo_gate_stage_quick=ok'
}

run_stage_cross() {
  local cross_command
  if [[ "${GOQ_REQUIRE_LINUX_CROSS_BUILD:-0}" == 1 ]]; then
    for cross_command in cargo-zigbuild zig; do
      if ! command -v "$cross_command" >/dev/null 2>&1; then
        printf 'required Linux cross-build command is missing: %s\n' \
          "$cross_command" >&2
        exit 1
      fi
    done
  fi
  ./scripts/run-linux-cross-build-gate.sh
  echo 'demo_gate_stage_cross=ok'
}

run_stage_native() {
  require_commands cargo rustc git
  require_rust_test 'resolves_pinned_upstream_gamescope_pipewire_contract' \
    -p sigil-host --bin sigil
  cargo test --locked --workspace --all-targets
  cargo clippy --locked --workspace --all-targets -- -D warnings
  cargo check --locked -p portal --all-targets \
    --features experimental-non-macos-pointer-capture
  echo 'experimental_non_macos_pointer_capture_compile=ok'
  cargo build --locked -p sigil-host --bin sigil

  local host_dependencies
  host_dependencies="$(cargo tree --locked -p sigil-host --edges normal)"
  if grep -Eiq '(^|[[:space:]├└│─])(tauri|wry|webkit)([[:space:]-]|$)' <<<"$host_dependencies"; then
    echo 'pure host dependency gate failed: desktop/webview dependency detected' >&2
    grep -Ei 'tauri|wry|webkit' <<<"$host_dependencies" >&2
    exit 1
  fi

  local catalog_dependencies
  catalog_dependencies="$({
    cargo tree --locked -p sigil-host --edges normal
    cargo tree --locked -p portal --edges normal
  })"
  if grep -Eiq '(^|[[:space:]├└│─])(moq-media|moq-mux)([[:space:]-]|$)' <<<"$catalog_dependencies"; then
    echo 'MoQ catalog boundary failed: standard-media dependency detected' >&2
    grep -Ei 'moq-media|moq-mux' <<<"$catalog_dependencies" >&2
    exit 1
  fi
  echo 'demo_gate_stage_native=ok'
}

run_stage_gstreamer() {
  local gstreamer_element
  if [[ "${GOQ_VERIFY_IN_PROCESS_GSTREAMER:-0}" != 1 ]]; then
    echo 'in_process_gstreamer_gate=skipped (GOQ_VERIFY_IN_PROCESS_GSTREAMER is not 1)'
    return 0
  fi
  require_commands cargo gst-inspect-1.0
  for gstreamer_element in videotestsrc queue videoconvert videoscale capsfilter x264enc h264parse appsink; do
    gst-inspect-1.0 "$gstreamer_element" >/dev/null || {
      printf 'in-process GStreamer gate requires the %s plugin\n' \
        "$gstreamer_element" >&2
      exit 1
    }
  done
  local control_test='encoder_control_coalesces_latest_state_and_acknowledges_only_configured_idr'
  local gstreamer_test='in_process_gstreamer_x264_smoke'
  require_rust_test "$control_test" -p sigil-host
  cargo test --locked -p sigil-host "$control_test"
  cargo check --locked -p sigil-host --all-targets --features in-process-gstreamer
  require_rust_test "$gstreamer_test" -p sigil-host --features in-process-gstreamer
  cargo test --locked -p sigil-host --features in-process-gstreamer \
    "$gstreamer_test" -- --ignored --nocapture
  echo 'in_process_gstreamer_gate=ok'
}

run_stage_repo_tests() {
  require_commands cargo rustc node git ffmpeg shellcheck
  ./scripts/run-shell-tests.sh scripts/tests
  echo 'demo_gate_stage_repo_tests=ok'
}

run_loopback_case() {
  case "$1" in
    1)
      ./scripts/loopback-proof.sh
      ;;
    2)
      ./scripts/loopback-proof.sh --profile debug --control-v2 --viewers 3 \
        --primary-frames 600 --reconnect-cycles 3
      ;;
    3)
      ./scripts/loopback-proof.sh --profile debug --control-v2 --viewers 3 \
        --focus-handoffs 3 --assert-neutral-before-successor \
        --primary-frames 600 --reconnect-cycles 3
      ;;
    4)
      ./scripts/loopback-proof.sh --profile release --legacy-exclusive --viewers 2 \
        --expect-second-rejected
      ;;
    *)
      printf 'unknown loopback case: %s\n' "$1" >&2
      exit 2
      ;;
  esac
}

run_stage_loopback() {
  local case_number
  require_commands cargo rustc git ffmpeg
  if [[ "$loopback_case" == all ]]; then
    for case_number in "${LOOPBACK_CASES[@]}"; do
      run_loopback_case "$case_number"
    done
  else
    run_loopback_case "$loopback_case"
  fi
  echo 'demo_gate_stage_loopback=ok'
}

run_stage_containment() {
  require_commands cargo
  cargo test --locked -p portal --release \
    commands::state::tests::rejects_direct_node_when_debug_mode_is_disabled \
    -- --exact

  cargo test --locked -p portal --release \
    commands::state::tests::ordinary_release_excludes_direct_node_bypass \
    -- --exact

  cargo test --locked -p portal --release --features demo-direct-node \
    commands::state::tests::app_state_accepts_direct_node_only_in_debug_builds \
    -- --exact

  cargo test --locked -p sigil-host --release \
    tests::ordinary_release_excludes_configured_host_auth_bypass \
    -- --exact

  cargo test --locked -p sigil-host --release --features demo-auth-bypass \
    tests::configured_host_auth_bypass_is_explicitly_build_contained \
    -- --exact
  echo 'demo_gate_stage_containment=ok'
}

run_stage() {
  case "$1" in
    quick) run_stage_quick ;;
    cross) run_stage_cross ;;
    native) run_stage_native ;;
    gstreamer) run_stage_gstreamer ;;
    repo-tests) run_stage_repo_tests ;;
    loopback) run_stage_loopback ;;
    containment) run_stage_containment ;;
    *)
      printf 'unknown stage: %s\n' "$1" >&2
      exit 2
      ;;
  esac
}

if [[ "$stage" == all ]]; then
  require_commands cargo rustc node git ffmpeg shellcheck
fi

printf 'repo=%s\n' "$repo_dir"
printf 'revision=%s\n' "$(git rev-parse HEAD)"
printf 'stage=%s\n' "$stage"
rustc --version
cargo --version

if [[ "$stage" == all ]]; then
  for current_stage in "${ALL_STAGES[@]}"; do
    run_stage "$current_stage"
  done
  # `quick` already checked this, but re-check after every build so a stage
  # that dirties the worktree still fails the complete gate.
  git diff --check
  echo 'demo_build_preflight=ok'
else
  run_stage "$stage"
fi
