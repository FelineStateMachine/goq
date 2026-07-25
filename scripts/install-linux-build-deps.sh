#!/usr/bin/env bash
set -euo pipefail

# Install the Linux build and runtime dependencies the complete demo gate needs.
#
# CI runs the gate as several parallel jobs, and every job that compiles Sigil
# or Portal needs the same package set. Keeping the list here rather than
# duplicated per job means a dependency can only be added or removed in one
# reviewable place, and ShellCheck covers it like every other repository script.
#
#   --profile full     Everything the Rust workspace, GStreamer gate, and
#                      loopback proofs need (default).
#   --profile lint     Only what format, syntax, and ShellCheck passes need.

profile=full

while [[ $# -gt 0 ]]; do
  case "$1" in
    --profile)
      [[ $# -ge 2 ]] || {
        printf -- '--profile requires a value\n' >&2
        exit 2
      }
      profile="$2"
      shift 2
      ;;
    *)
      printf 'unknown argument: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done

lint_packages=(
  shellcheck
)

full_packages=(
  build-essential
  file
  ffmpeg
  gstreamer1.0-plugins-base
  gstreamer1.0-plugins-bad
  gstreamer1.0-plugins-good
  gstreamer1.0-plugins-ugly
  gstreamer1.0-tools
  libayatana-appindicator3-dev
  libgstreamer-plugins-base1.0-dev
  libgstreamer1.0-dev
  librsvg2-dev
  libssl-dev
  libudev-dev
  libwebkit2gtk-4.1-dev
  libxdo-dev
  pkg-config
  python3-venv
  shellcheck
)

case "$profile" in
  full) packages=("${full_packages[@]}") ;;
  lint) packages=("${lint_packages[@]}") ;;
  *)
    printf -- '--profile must be full or lint\n' >&2
    exit 2
    ;;
esac

sudo apt-get update
sudo apt-get install --yes --no-install-recommends "${packages[@]}"
printf 'linux_build_deps=%s\n' "$profile"
