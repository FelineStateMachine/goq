#!/usr/bin/env bash
# shellcheck disable=SC2016
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(cd -- "$script_dir/../.." && pwd -P)"
gate="$repo_dir/scripts/run-linux-cross-build-gate.sh"
temp_root="$(mktemp -d "${TMPDIR:-/tmp}/goq-cross-build-gate.XXXXXX")"

cleanup() {
  rm -r -- "$temp_root"
}
trap cleanup EXIT

make_command() {
  local bin_dir="$1"
  local command_name="$2"
  shift 2

  install -d "$bin_dir"
  if [[ ! -e "$bin_dir/bash" ]]; then
    ln -s "$(command -v bash)" "$bin_dir/bash"
  fi
  printf '%s\n' '#!/usr/bin/env bash' "$@" >"$bin_dir/$command_name"
  chmod 0755 "$bin_dir/$command_name"
}

success_bin="$temp_root/success-bin"
make_command "$success_bin" cargo \
  'printf "%s\n" "$*" >"$GOQ_CARGO_ARGS_LOG"' \
  'printf "%s %s\n" "${PKG_CONFIG_ALLOW_CROSS:-}" "${PKG_CONFIG_ALLOW_SYSTEM_LIBS:-}" >"$GOQ_CARGO_ENV_LOG"'
make_command "$success_bin" cargo-zigbuild 'exit 0'
make_command "$success_bin" zig 'exit 0'

success_log="$temp_root/success.log"
GOQ_CARGO_ARGS_LOG="$temp_root/cargo-args.log" \
  GOQ_CARGO_ENV_LOG="$temp_root/cargo-env.log" \
  GOQ_REQUIRE_LINUX_CROSS_BUILD=1 \
  PATH="$success_bin" \
  "$gate" >"$success_log"
grep -Fxq 'linux_cross_build=ok' "$success_log"
grep -Fxq \
  'zigbuild --locked -p sigil-host --bins --target x86_64-unknown-linux-gnu.2.17' \
  "$temp_root/cargo-args.log"

GOQ_CARGO_ARGS_LOG="$temp_root/cargo-args.log" \
  GOQ_CARGO_ENV_LOG="$temp_root/cargo-env.log" \
  GOQ_REQUIRE_LINUX_CROSS_BUILD=1 \
  GOQ_VERIFY_IN_PROCESS_GSTREAMER=1 \
  PATH="$success_bin" \
  "$gate" >"$success_log"
grep -Fxq \
  'zigbuild --locked -p sigil-host --bins --target x86_64-unknown-linux-gnu.2.17 --features in-process-gstreamer' \
  "$temp_root/cargo-args.log"
grep -Fxq '1 1' "$temp_root/cargo-env.log"

missing_bin="$temp_root/missing-bin"
make_command "$missing_bin" cargo \
  'echo "required mode invoked cargo without both tools" >&2' \
  'exit 91'
make_command "$missing_bin" cargo-zigbuild 'exit 0'

missing_log="$temp_root/missing.log"
if GOQ_REQUIRE_LINUX_CROSS_BUILD=1 \
  PATH="$missing_bin" \
  "$gate" >"$temp_root/missing.stdout" 2>"$missing_log"; then
  echo 'required cross-build gate accepted a missing point-of-use tool' >&2
  exit 1
fi
grep -Fxq \
  'required Linux cross-build command is missing at build time: zig' \
  "$missing_log"
if grep -Fq 'required mode invoked cargo' "$missing_log"; then
  echo 'required cross-build gate invoked cargo after tool validation failed' >&2
  exit 1
fi
if grep -Fq 'linux_cross_build=skipped' "$temp_root/missing.stdout"; then
  echo 'required cross-build gate entered the optional skip path' >&2
  exit 1
fi

optional_bin="$temp_root/optional-bin"
make_command "$optional_bin" cargo 'exit 92'
optional_log="$temp_root/optional.log"
GOQ_REQUIRE_LINUX_CROSS_BUILD=0 \
  PATH="$optional_bin" \
  "$gate" >"$optional_log"
grep -Fxq \
  'linux_cross_build=skipped (cargo-zigbuild and zig are both required)' \
  "$optional_log"

echo 'linux_cross_build_gate=ok'
