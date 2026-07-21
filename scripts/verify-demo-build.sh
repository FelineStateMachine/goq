#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(cd -- "$script_dir/.." && pwd -P)"
cd "$repo_dir"

if [[ -f "$HOME/.cargo/env" ]]; then
  # shellcheck source=/dev/null
  source "$HOME/.cargo/env"
fi

for command_name in cargo rustc node git; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    printf 'required command is missing: %s\n' "$command_name" >&2
    exit 1
  fi
done

printf 'repo=%s\n' "$repo_dir"
printf 'revision=%s\n' "$(git rev-parse HEAD)"
rustc --version
cargo --version

cargo fmt --all -- --check
cargo test --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets -- -D warnings
if command -v cargo-zigbuild >/dev/null 2>&1 && command -v zig >/dev/null 2>&1; then
  cargo zigbuild --locked -p sigil-host --target x86_64-unknown-linux-gnu.2.17
  echo 'linux_cross_build=ok'
else
  echo 'linux_cross_build=skipped (cargo-zigbuild and zig are both required)'
fi
node --check src/main.js
node --check src/audio-envelope.mjs
node --check src/audio-ring.mjs
node --check src/audio-worklet.js
node --check src/controller-state.mjs
node --check src/frame-envelope.mjs
node --check src/input-state.mjs
node --check src/stream-metrics.mjs
node --test \
  src/audio-envelope.test.mjs \
  src/audio-ring.test.mjs \
  src/controller-state.test.mjs \
  src/frame-envelope.test.mjs \
  src/input-state.test.mjs \
  src/stream-metrics.test.mjs

if command -v shellcheck >/dev/null 2>&1; then
  find scripts -type f -name '*.sh' -exec shellcheck {} +
else
  echo 'shellcheck is required for the demo preflight' >&2
  exit 1
fi

host_dependencies="$(cargo tree --locked -p sigil-host --edges normal)"
if grep -Eiq '(^|[[:space:]├└│─])(tauri|wry|webkit)([[:space:]-]|$)' <<<"$host_dependencies"; then
  echo 'pure host dependency gate failed: desktop/webview dependency detected' >&2
  grep -Ei 'tauri|wry|webkit' <<<"$host_dependencies" >&2
  exit 1
fi

./scripts/loopback-proof.sh

cargo test --locked -p sigil-spark --release \
  commands::state::tests::rejects_direct_node_when_debug_mode_is_disabled \
  -- --exact

cargo test --locked -p sigil-spark --release --features demo-direct-node \
  commands::state::tests::app_state_accepts_direct_node_only_in_debug_builds \
  -- --exact

git diff --check
echo 'demo_build_preflight=ok'
