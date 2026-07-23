#!/usr/bin/env bash

set -euo pipefail
umask 077

usage() {
  cat <<'EOF'
Usage: scripts/sign-bazzite-release.sh \
  --tag vVERSION \
  --archive /absolute/path/sigil-vVERSION-linux-glibc2.17-x86_64.tar.gz \
  --minisign-key /absolute/offline/path/to/release.key \
  --public-key-file /absolute/path/to/sigil-minisign.pub \
  [--source-commit HEX]

Verify and sign one clean Sigil release candidate on the offline publisher
machine. The secret key and passphrase are consumed only by Minisign and are
never copied into the package or printed by this script.
EOF
}

die() {
  printf 'offline signing failed: %s\n' "$*" >&2
  exit 1
}

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
release_tag=""
archive_path=""
secret_key=""
public_key_file=""
source_commit=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tag)
      [[ $# -ge 2 ]] || die "--tag requires a value"
      release_tag="$2"
      shift 2
      ;;
    --archive)
      [[ $# -ge 2 ]] || die "--archive requires a path"
      archive_path="$2"
      shift 2
      ;;
    --minisign-key)
      [[ $# -ge 2 ]] || die "--minisign-key requires a path"
      secret_key="$2"
      shift 2
      ;;
    --public-key-file)
      [[ $# -ge 2 ]] || die "--public-key-file requires a path"
      public_key_file="$2"
      shift 2
      ;;
    --source-commit)
      [[ $# -ge 2 ]] || die "--source-commit requires a value"
      source_commit="$2"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ -n "$release_tag" && -n "$archive_path" && -n "$secret_key" \
  && -n "$public_key_file" ]] || die "tag, archive, secret key, and public key are required"
[[ "$secret_key" == /* && -f "$secret_key" && ! -L "$secret_key" ]] \
  || die "Minisign secret key must be an absolute regular file"
[[ "$(stat -c '%a' "$secret_key" 2>/dev/null || stat -f '%Lp' "$secret_key")" =~ ^(400|600)$ ]] \
  || die "Minisign secret key permissions must be 0400 or 0600"
command -v minisign >/dev/null 2>&1 || die "minisign is required on the offline signer"

verify_args=(--tag "$release_tag" --archive "$archive_path" --candidate)
if [[ -n "$source_commit" ]]; then
  verify_args+=(--source-commit "$source_commit")
fi
"$script_dir/verify-sigil-release.sh" "${verify_args[@]}"

signature_path="$archive_path.minisig"
[[ ! -e "$signature_path" ]] || die "detached signature already exists: $signature_path"
signature_created=false
cleanup_signature() {
  local status=$?
  trap - EXIT INT TERM HUP
  if [[ "$status" -ne 0 && "$signature_created" == true \
    && -f "$signature_path" && ! -L "$signature_path" ]]; then
    rm -f -- "$signature_path"
  fi
  exit "$status"
}
trap cleanup_signature EXIT INT TERM HUP
minisign -S -s "$secret_key" -m "$archive_path" -x "$signature_path" \
  -t "Sigil host $release_tag" -c "offline release signature"
signature_created=true

verify_args=(--tag "$release_tag" --archive "$archive_path" \
  --public-key-file "$public_key_file")
if [[ -n "$source_commit" ]]; then
  verify_args+=(--source-commit "$source_commit")
fi
"$script_dir/verify-sigil-release.sh" "${verify_args[@]}"
trap - EXIT INT TERM HUP
printf 'detached_signature=%s\n' "$signature_path"
printf 'offline_signing=ok\n'
