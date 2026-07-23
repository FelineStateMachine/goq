#!/usr/bin/env bash
set -euo pipefail

case "${GOQ_VERIFY_IN_PROCESS_GSTREAMER:-0}" in
  0|1) ;;
  *)
    printf 'GOQ_VERIFY_IN_PROCESS_GSTREAMER must be 0 or 1\n' >&2
    exit 1
    ;;
esac

case "${GOQ_REQUIRE_LINUX_CROSS_BUILD:-0}" in
  0|1) ;;
  *)
    printf 'GOQ_REQUIRE_LINUX_CROSS_BUILD must be 0 or 1\n' >&2
    exit 1
    ;;
esac

require_cross_commands() {
  local cross_command

  for cross_command in cargo-zigbuild zig; do
    if ! command -v "$cross_command" >/dev/null 2>&1; then
      printf 'required Linux cross-build command is missing at build time: %s\n' \
        "$cross_command" >&2
      exit 1
    fi
  done
}

run_cross_build() {
  # Re-check at the point of use. The complete repository gate may spend
  # several minutes on native tests after its initial tool preflight.
  require_cross_commands

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
}

if [[ "${GOQ_REQUIRE_LINUX_CROSS_BUILD:-0}" == 1 ]]; then
  run_cross_build
elif command -v cargo-zigbuild >/dev/null 2>&1 \
  && command -v zig >/dev/null 2>&1; then
  run_cross_build
else
  echo 'linux_cross_build=skipped (cargo-zigbuild and zig are both required)'
fi
