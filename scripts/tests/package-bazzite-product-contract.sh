#!/usr/bin/env bash

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/../.." && pwd -P)"
temp_parent="${TMPDIR:-/tmp}"
temp_parent="${temp_parent%/}"
temp_root="$(mktemp -d "$temp_parent/sigil-product-contract.XXXXXX")"

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  case "$temp_root" in
    "$temp_parent"/sigil-product-contract.??????) rm -rf -- "$temp_root" ;;
  esac
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

fixture_repo="$temp_root/repo"
install -d -m 0700 "$fixture_repo/scripts" "$fixture_repo/crates/sigil-host" "$temp_root/bin" "$temp_root/output"
for path in \
  scripts/package-bazzite-release.sh \
  scripts/install-bazzite-package.sh \
  scripts/rollback-bazzite-release.sh \
  scripts/sigil-host.service \
  scripts/50-sigil-spark-audio.conf \
  scripts/70-sigil-remote-input.rules \
  scripts/72-sigil-uinput.rules \
  scripts/99-sigil-uinput.rules \
  docs/sigil-host-activation.md \
  crates/sigil-host/Cargo.toml \
  Cargo.lock rust-toolchain.toml LICENSE
do
  install -d -m 0700 "$(dirname -- "$fixture_repo/$path")"
  install -m "$(stat -c '%a' "$repo_dir/$path" 2>/dev/null || stat -f '%Lp' "$repo_dir/$path")" \
    "$repo_dir/$path" "$fixture_repo/$path"
done

# Intentional literals form isolated cargo command fixtures.
# shellcheck disable=SC2016
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'test "${1:-}" = zigbuild' \
  'test "${PKG_CONFIG_ALLOW_CROSS:-}" = 1' \
  'test "${PKG_CONFIG_ALLOW_SYSTEM_LIBS:-}" = 1' \
  'case " $* " in *" --features in-process-gstreamer "*) ;; *) exit 64 ;; esac' \
  'output="$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release"' \
  'install -d -m 0700 "$output"' \
  'printf "#!/usr/bin/env bash\\nexit 0\\n" >"$output/sigil"' \
  'cp "$output/sigil" "$output/sigil-probe"' \
  'chmod 0755 "$output/sigil" "$output/sigil-probe"' \
  >"$temp_root/bin/cargo"
printf '%s\n' '#!/usr/bin/env bash' 'printf "cargo-zigbuild 0.23.0\n"' \
  >"$temp_root/bin/cargo-zigbuild"
chmod 0755 "$temp_root/bin/cargo" "$temp_root/bin/cargo-zigbuild"

git -C "$fixture_repo" init -q
git -C "$fixture_repo" config user.name 'Sigil Release Test'
git -C "$fixture_repo" config user.email 'release-test@invalid.example'
git -C "$fixture_repo" add .
git -C "$fixture_repo" commit -qm 'fixture release'
git -C "$fixture_repo" tag v0.1.0
source_commit="$(git -C "$fixture_repo" rev-parse HEAD)"

archive="$temp_root/output/sigil-v0.1.0-bazzite-x86_64.tar.gz"
PATH="$temp_root/bin:$PATH" "$fixture_repo/scripts/package-bazzite-release.sh" \
  --release-tag v0.1.0 --output "$archive" >"$temp_root/product.log"
grep -Fq 'publisher_signature=pending-offline' "$temp_root/product.log"
"$repo_dir/scripts/verify-sigil-release.sh" \
  --tag v0.1.0 --archive "$archive" --source-commit "$source_commit" --candidate \
  | grep -Fq 'sigil_release_verification=ok'

# Exercise the offline boundary with a recording Minisign fixture. The archive
# and provenance validation are production code; only cryptography is stubbed.
# shellcheck disable=SC2016
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'signature=""' \
  'signing=false' \
  'while test "$#" -gt 0; do' \
  '  case "$1" in -S) signing=true ;; -x) signature="$2"; shift ;; esac' \
  '  shift' \
  'done' \
  'if $signing; then printf "fixture signature\\n" >"$signature"; fi' \
  'exit 0' >"$temp_root/bin/minisign"
chmod 0755 "$temp_root/bin/minisign"
secret_key="$temp_root/offline.key"
public_key="$temp_root/sigil-minisign.pub"
printf 'fixture secret\n' >"$secret_key"
chmod 0600 "$secret_key"
printf '%s\n%s\n' 'untrusted comment: fixture public key' \
  'RWAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA' >"$public_key"
PATH="$temp_root/bin:$PATH" "$repo_dir/scripts/sign-bazzite-release.sh" \
  --tag v0.1.0 \
  --archive "$archive" \
  --source-commit "$source_commit" \
  --minisign-key "$secret_key" \
  --public-key-file "$public_key" \
  | grep -Fq 'offline_signing=ok'
[[ -f "$archive.minisig" ]]

assert_rejected() {
  local name="$1"
  local expected="$2"
  local tag="$3"
  local output_dir="$temp_root/$name"
  install -d -m 0700 "$output_dir"
  local output="$output_dir/sigil-$tag-bazzite-x86_64.tar.gz"
  local log="$output_dir/rejected.log"
  if PATH="$temp_root/bin:$PATH" "$fixture_repo/scripts/package-bazzite-release.sh" \
    --release-tag "$tag" --output "$output" >"$log" 2>&1
  then
    printf 'FAIL: %s unexpectedly succeeded\n' "$name" >&2
    exit 1
  fi
  grep -Fq "$expected" "$log" || {
    printf 'FAIL: %s did not report %s\n' "$name" "$expected" >&2
    sed -n '1,120p' "$log" >&2
    exit 1
  }
}

printf 'dirty\n' >"$fixture_repo/dirty-file"
assert_rejected dirty-source 'worktree is dirty' v0.1.0
git -C "$fixture_repo" add dirty-file
git -C "$fixture_repo" commit -qm 'new untagged source'
assert_rejected tag-head-mismatch 'must resolve exactly to clean HEAD' v0.1.0
git -C "$fixture_repo" tag v0.1.1
assert_rejected tag-version-mismatch 'does not match Sigil version v0.1.0' v0.1.1

printf 'package_bazzite_product_contract_tests=ok\n'
