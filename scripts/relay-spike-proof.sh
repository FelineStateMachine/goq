#!/usr/bin/env bash
set -euo pipefail

video="1280x800@60"
audio="opus-48k-stereo"
duration_seconds=300

usage() {
  cat <<'USAGE'
Usage: relay-spike-proof.sh [options]

  --video TARGET              Fixed encoded target (required: 1280x800@60)
  --audio FORMAT              Fixed audio format (required: opus-48k-stereo)
  --duration-seconds SECONDS  Virtual benchmark horizon, 1..300 (default: 300)
USAGE
}

while (($#)); do
  case "$1" in
    --video)
      video=${2:?missing --video value}
      shift 2
      ;;
    --audio)
      audio=${2:?missing --audio value}
      shift 2
      ;;
    --duration-seconds)
      duration_seconds=${2:?missing --duration-seconds value}
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      printf 'error: unknown argument: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

[[ "$video" == "1280x800@60" ]] || { printf 'error: unsupported video target\n' >&2; exit 2; }
[[ "$audio" == "opus-48k-stereo" ]] || { printf 'error: unsupported audio format\n' >&2; exit 2; }
[[ "$duration_seconds" =~ ^[0-9]+$ ]] || { printf 'error: duration must be an integer\n' >&2; exit 2; }
((duration_seconds >= 1 && duration_seconds <= 300)) || {
  printf 'error: duration must be within 1..300 seconds\n' >&2
  exit 2
}

repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tmp_root=$(mktemp -d "${TMPDIR:-/tmp}/goq-relay-spike.XXXXXX")
trap 'rm -rf "$tmp_root"' EXIT
log="$tmp_root/relay-spike.log"

# The automated spike advances a bounded virtual media horizon without sleeping.
# Exact direct/relay-fallback Iroh path measurements remain the manual gate.
(
  cd "$repo_root"
  # shellcheck source=/dev/null
  source "${HOME}/.cargo/env"
  cargo run --quiet --release -p sigil-host --bin sigil-relay-spike -- \
    --video "$video" \
    --audio "$audio" \
    --duration-seconds "$duration_seconds"
) | tee "$log"

grep -Fxq 'relay_spike=ok' "$log"
grep -Fxq 'authentication_mode=ed25519-v1' "$log"
grep -Fxq 'decision=defer-production-mesh' "$log"
grep -Fxq 'host_upload_savings_percent=50' "$log"
grep -Fxq 'tamper_rejected=ok' "$log"
grep -Fxq 'wrong_subscriber_rejected=ok' "$log"
grep -Fxq 'expired_subscription_rejected=ok' "$log"
grep -Fxq 'relay_loss_direct_fallback=ok' "$log"
grep -Fxq 'withheld_media_trusted=0' "$log"
grep -Eq '^maximum_relay_queue=[1-4]$' "$log"

printf 'proof=ok\n'
