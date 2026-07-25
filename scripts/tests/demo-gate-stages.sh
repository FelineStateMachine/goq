#!/usr/bin/env bash
set -euo pipefail

# Prove that splitting the complete demo gate into stages did not drop a check.
#
# The gate is now run by CI as parallel legs, one stage each, so the dangerous
# regression is a check that belongs to no stage and therefore silently stops
# running. This exercises the real gate script against stubbed tools in a
# throwaway repository and asserts two things:
#
#   1. every check in a required inventory actually executes, and
#   2. running the stages one at a time spends exactly the same commands as the
#      default complete run.
#
# Static structure is checked separately in verify_ci_cross_build_policy.py.
# This is the dynamic half, and it is what catches an early `exit 0` or a
# conditional wrapped around a stage body.

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/../.." && pwd -P)"
temp_parent="${TMPDIR:-/tmp}"
temp_parent="${temp_parent%/}"
temp_root="$(mktemp -d "$temp_parent/goq-demo-gate-stages.XXXXXX")"

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  case "$temp_root" in
    "$temp_parent"/goq-demo-gate-stages.??????) rm -rf -- "$temp_root" ;;
  esac
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

fake_repo="$temp_root/repo"
stub_bin="$temp_root/bin"
fake_home="$temp_root/home"
mkdir -p "$fake_repo/scripts" "$fake_repo/portal" "$stub_bin" "$fake_home"

# Stub logs live outside the fake repository so they cannot make
# `git diff --check` see a dirty worktree.
#
# The generated stub bodies below deliberately keep `$*` and `$GOQ_STAGE_LOG`
# unexpanded: they must expand when the stub runs, not when it is written.
write_stub() {
  local name="$1"
  shift
  {
    printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail'
    # shellcheck disable=SC2016
    printf 'printf "%%s\\n" "%s $*" >>"$GOQ_STAGE_LOG"\n' "$name"
    printf '%s\n' "$@"
  } >"$stub_bin/$name"
  chmod 0755 "$stub_bin/$name"
}

# `cargo test ... -- --list` output has to satisfy require_rust_test, and
# `cargo tree` output has to satisfy the dependency boundary greps.
# shellcheck disable=SC2016
write_stub cargo '
for arg in "$@"; do
  if [[ "$arg" == "--list" ]]; then
    printf "%s\n" \
      "stub::resolves_pinned_upstream_gamescope_pipewire_contract: test" \
      "stub::encoder_control_coalesces_latest_state_and_acknowledges_only_configured_idr: test" \
      "stub::in_process_gstreamer_x264_smoke: test"
    exit 0
  fi
done
if [[ "${1:-}" == "tree" ]]; then
  printf "%s\n" "stub-crate v0.0.0"
  exit 0
fi
if [[ "${1:-}" == "--version" ]]; then
  printf "%s\n" "cargo 1.95.0 (stub)"
fi
exit 0'
write_stub rustc 'printf "%s\n" "rustc 1.95.0 (stub)"'
write_stub node 'exit 0'
write_stub ffmpeg 'exit 0'
write_stub shellcheck 'exit 0'
write_stub gst-inspect-1.0 'exit 0'
write_stub cargo-zigbuild 'printf "%s\n" "cargo-zigbuild 0.23.0"'
write_stub zig 'printf "%s\n" "0.16.0"'

# Helper scripts the gate shells out to become logging stubs too, so their
# arguments are observable without running a real loopback session.
write_helper() {
  local relative="$1"
  local name="$2"
  {
    printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail'
    # shellcheck disable=SC2016
    printf 'printf "%%s\\n" "%s $*" >>"$GOQ_STAGE_LOG"\n' "$name"
    printf '%s\n' 'exit 0'
  } >"$fake_repo/$relative"
  chmod 0755 "$fake_repo/$relative"
}

write_helper scripts/run-linux-cross-build-gate.sh cross-build-gate
write_helper scripts/loopback-proof.sh loopback-proof
write_helper scripts/run-shell-tests.sh run-shell-tests

cp "$repo_dir/scripts/verify-demo-build.sh" "$fake_repo/scripts/verify-demo-build.sh"
chmod 0755 "$fake_repo/scripts/verify-demo-build.sh"

printf '%s\n' 'export const stub = 1;' >"$fake_repo/portal/main.js"
printf '%s\n' 'export const stub = 2;' >"$fake_repo/portal/diagnostics.mjs"
printf '%s\n' 'export const stub = 3;' >"$fake_repo/portal/stub.test.mjs"

GIT_AUTHOR_NAME=stub GIT_AUTHOR_EMAIL=stub@example.invalid \
  GIT_COMMITTER_NAME=stub GIT_COMMITTER_EMAIL=stub@example.invalid \
  git -C "$fake_repo" -c init.defaultBranch=main init --quiet
git -C "$fake_repo" add -A
GIT_AUTHOR_NAME=stub GIT_AUTHOR_EMAIL=stub@example.invalid \
  GIT_COMMITTER_NAME=stub GIT_COMMITTER_EMAIL=stub@example.invalid \
  git -C "$fake_repo" commit --quiet -m 'stub repository'

run_gate() {
  local log="$1"
  shift
  : >"$log"
  # HOME points at an empty directory so the gate cannot source a real
  # ~/.cargo/env and put the genuine toolchain ahead of the stubs on PATH.
  env -i \
    HOME="$fake_home" \
    PATH="$stub_bin:/usr/bin:/bin:/usr/sbin:/sbin" \
    GOQ_STAGE_LOG="$log" \
    GOQ_VERIFY_IN_PROCESS_GSTREAMER=1 \
    GOQ_REQUIRE_LINUX_CROSS_BUILD=1 \
    LC_ALL=C \
    bash "$fake_repo/scripts/verify-demo-build.sh" "$@" \
    >"$log.stdout" 2>"$log.stderr" || {
    printf 'gate invocation failed: %s\n' "$*" >&2
    sed -n '1,60p' "$log.stderr" >&2
    exit 1
  }
}

complete_log="$temp_root/complete.log"
run_gate "$complete_log"

if ! grep -Fxq 'demo_build_preflight=ok' "$complete_log.stdout"; then
  printf 'the complete gate did not report success\n' >&2
  exit 1
fi

# Every check the single serial job used to run. A stage that stops reaching one
# of these fails here rather than silently passing in CI.
required_commands=(
  'cargo fmt --all -- --check'
  'cargo test --locked --workspace --all-targets'
  'cargo clippy --locked --workspace --all-targets -- -D warnings'
  'cargo check --locked -p portal --all-targets --features experimental-non-macos-pointer-capture'
  'cargo build --locked -p sigil-host --bin sigil'
  'cargo tree --locked -p sigil-host --edges normal'
  'cargo tree --locked -p portal --edges normal'
  'cargo check --locked -p sigil-host --all-targets --features in-process-gstreamer'
  'cargo test --locked -p sigil-host --features in-process-gstreamer in_process_gstreamer_x264_smoke -- --ignored --nocapture'
  'cargo test --locked -p sigil-host encoder_control_coalesces_latest_state_and_acknowledges_only_configured_idr'
  'cargo test --locked -p portal --release commands::state::tests::rejects_direct_node_when_debug_mode_is_disabled -- --exact'
  'cargo test --locked -p portal --release commands::state::tests::ordinary_release_excludes_direct_node_bypass -- --exact'
  'cargo test --locked -p portal --release --features demo-direct-node commands::state::tests::app_state_accepts_direct_node_only_in_debug_builds -- --exact'
  'cargo test --locked -p sigil-host --release tests::ordinary_release_excludes_configured_host_auth_bypass -- --exact'
  'cargo test --locked -p sigil-host --release --features demo-auth-bypass tests::configured_host_auth_bypass_is_explicitly_build_contained -- --exact'
  'cross-build-gate '
  'run-shell-tests scripts/tests'
  'shellcheck '
  'node --check portal/main.js'
  'node --check portal/diagnostics.mjs'
  'node --test portal/stub.test.mjs'
  'gst-inspect-1.0 videotestsrc'
  'gst-inspect-1.0 x264enc'
  'gst-inspect-1.0 appsink'
  'loopback-proof '
  'loopback-proof --profile debug --control-v2 --viewers 3 --primary-frames 600 --reconnect-cycles 3'
  'loopback-proof --profile debug --control-v2 --viewers 3 --focus-handoffs 3 --assert-neutral-before-successor --primary-frames 600 --reconnect-cycles 3'
  'loopback-proof --profile release --legacy-exclusive --viewers 2 --expect-second-rejected'
)

missing=0
for required in "${required_commands[@]}"; do
  if ! grep -Fq -- "$required" "$complete_log"; then
    printf 'the complete gate never ran: %s\n' "$required" >&2
    missing=1
  fi
done
if (( missing != 0 )); then
  printf 'complete gate command log follows\n' >&2
  sed -n '1,200p' "$complete_log" >&2
  exit 1
fi

# Running the stages one at a time must spend exactly the same commands as the
# default complete run. This is what makes the parallel CI legs equivalent to
# the serial job they replaced.
union_log="$temp_root/union.log"
: >"$union_log"
for stage in quick cross native gstreamer repo-tests containment; do
  stage_log="$temp_root/stage-$stage.log"
  run_gate "$stage_log" --stage "$stage"
  cat "$stage_log" >>"$union_log"
done
for case_number in 1 2 3 4; do
  stage_log="$temp_root/stage-loopback-$case_number.log"
  run_gate "$stage_log" --stage loopback --loopback-case "$case_number"
  cat "$stage_log" >>"$union_log"
done

# Each invocation prints its own toolchain banner, so ten stage runs report it
# ten times. That is diagnostics rather than a check, so compare the checks.
strip_banner() {
  grep -Fvx -e 'cargo --version' -e 'rustc --version' "$1" | sort
}

strip_banner "$complete_log" >"$temp_root/complete.sorted"
strip_banner "$union_log" >"$temp_root/union.sorted"
if ! diff -u "$temp_root/complete.sorted" "$temp_root/union.sorted" \
  >"$temp_root/union.diff"; then
  printf 'the per-stage runs do not spend the same commands as the complete gate\n' >&2
  printf '(-) only in the complete run, (+) only in the per-stage runs\n' >&2
  sed -n '1,80p' "$temp_root/union.diff" >&2
  exit 1
fi

printf 'demo_gate_stage_coverage=ok\n'
