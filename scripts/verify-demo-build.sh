#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(cd -- "$script_dir/.." && pwd -P)"
cd "$repo_dir"

if [[ -f "$HOME/.cargo/env" ]]; then
  # shellcheck source=/dev/null
  source "$HOME/.cargo/env"
fi

for command_name in cargo rustc node git ffmpeg shellcheck; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    printf 'required command is missing: %s\n' "$command_name" >&2
    exit 1
  fi
done

case "${GOQ_VERIFY_IN_PROCESS_GSTREAMER:-0}" in
  0|1) ;;
  *)
    printf 'GOQ_VERIFY_IN_PROCESS_GSTREAMER must be 0 or 1\n' >&2
    exit 1
    ;;
esac

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

if [[ "${GOQ_VERIFY_IN_PROCESS_GSTREAMER:-0}" == 1 ]]; then
  command -v gst-inspect-1.0 >/dev/null 2>&1 || {
    echo 'in-process GStreamer gate requires gst-inspect-1.0' >&2
    exit 1
  }
  for gstreamer_element in videotestsrc queue videoconvert videoscale capsfilter x264enc h264parse appsink; do
    gst-inspect-1.0 "$gstreamer_element" >/dev/null || {
      printf 'in-process GStreamer gate requires the %s plugin\n' \
        "$gstreamer_element" >&2
      exit 1
    }
  done
fi

printf 'repo=%s\n' "$repo_dir"
printf 'revision=%s\n' "$(git rev-parse HEAD)"
rustc --version
cargo --version

require_rust_test 'resolves_pinned_upstream_gamescope_pipewire_contract' \
  -p sigil-host --bin sigil
cargo fmt --all -- --check
cargo test --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets -- -D warnings
if [[ "${GOQ_VERIFY_IN_PROCESS_GSTREAMER:-0}" == 1 ]]; then
  control_test='encoder_control_coalesces_latest_state_and_acknowledges_only_configured_idr'
  gstreamer_test='in_process_gstreamer_x264_smoke'
  require_rust_test "$control_test" -p sigil-host
  cargo test --locked -p sigil-host "$control_test"
  cargo check --locked -p sigil-host --all-targets --features in-process-gstreamer
  require_rust_test "$gstreamer_test" -p sigil-host --features in-process-gstreamer
  cargo test --locked -p sigil-host --features in-process-gstreamer \
    "$gstreamer_test" -- --ignored --nocapture
  echo 'in_process_gstreamer_gate=ok'
fi
if command -v cargo-zigbuild >/dev/null 2>&1 && command -v zig >/dev/null 2>&1; then
  if [[ "${GOQ_VERIFY_IN_PROCESS_GSTREAMER:-0}" == 1 ]]; then
    # Zig intentionally omits the host's default system-library paths for an
    # explicit target. Preserve pkg-config's system -L entries so the dynamic
    # GStreamer/GLib development libraries remain linkable without an rpath.
    PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_ALLOW_SYSTEM_LIBS=1 \
      cargo zigbuild --locked -p sigil-host --bins \
        --target x86_64-unknown-linux-gnu.2.17 --features in-process-gstreamer
  else
    cargo zigbuild --locked -p sigil-host --bins \
      --target x86_64-unknown-linux-gnu.2.17
  fi
  echo 'linux_cross_build=ok'
else
  echo 'linux_cross_build=skipped (cargo-zigbuild and zig are both required)'
fi
while IFS= read -r frontend_source; do
  node --check "$frontend_source"
done < <(find portal -maxdepth 1 -type f \( -name '*.js' -o -name '*.mjs' \) -print | sort)
node --test portal/*.test.mjs

cargo build --locked -p sigil-host --bin sigil
find scripts -type f -name '*.sh' -exec shellcheck {} +
./scripts/run-shell-tests.sh scripts/tests

host_dependencies="$(cargo tree --locked -p sigil-host --edges normal)"
if grep -Eiq '(^|[[:space:]├└│─])(tauri|wry|webkit)([[:space:]-]|$)' <<<"$host_dependencies"; then
  echo 'pure host dependency gate failed: desktop/webview dependency detected' >&2
  grep -Ei 'tauri|wry|webkit' <<<"$host_dependencies" >&2
  exit 1
fi

catalog_dependencies="$({
  cargo tree --locked -p sigil-host --edges normal
  cargo tree --locked -p portal --edges normal
})"
if grep -Eiq '(^|[[:space:]├└│─])(moq-media|moq-mux)([[:space:]-]|$)' <<<"$catalog_dependencies"; then
  echo 'MoQ catalog boundary failed: standard-media dependency detected' >&2
  grep -Ei 'moq-media|moq-mux' <<<"$catalog_dependencies" >&2
  exit 1
fi

./scripts/loopback-proof.sh

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

git diff --check
echo 'demo_build_preflight=ok'
