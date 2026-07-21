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
while IFS= read -r frontend_source; do
  node --check "$frontend_source"
done < <(find src -maxdepth 1 -type f \( -name '*.js' -o -name '*.mjs' \) -print | sort)
node --test src/*.test.mjs

find scripts -type f -name '*.sh' -exec shellcheck {} +
while IFS= read -r script_test; do
  "$script_test"
done < <(find scripts/tests -maxdepth 1 -type f -name '*.sh' -perm -u+x -print | sort)

host_dependencies="$(cargo tree --locked -p sigil-host --edges normal)"
if grep -Eiq '(^|[[:space:]├└│─])(tauri|wry|webkit)([[:space:]-]|$)' <<<"$host_dependencies"; then
  echo 'pure host dependency gate failed: desktop/webview dependency detected' >&2
  grep -Ei 'tauri|wry|webkit' <<<"$host_dependencies" >&2
  exit 1
fi

./scripts/loopback-proof.sh

cargo test --locked -p portal --release \
  commands::state::tests::rejects_direct_node_when_debug_mode_is_disabled \
  -- --exact

cargo test --locked -p portal --release --features demo-direct-node \
  commands::state::tests::app_state_accepts_direct_node_only_in_debug_builds \
  -- --exact

git diff --check
echo 'demo_build_preflight=ok'
