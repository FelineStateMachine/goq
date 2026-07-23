#!/usr/bin/env bash

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/../.." && pwd -P)"
contract_file="$repo_dir/release/sigil-target-contract.txt"
expected_contract="linux-glibc2.17-x86_64"

[[ -f "$contract_file" && ! -L "$contract_file" ]] \
  || {
    printf 'Sigil target contract file is missing or unsafe\n' >&2
    exit 1
  }
[[ "$(wc -l <"$contract_file" | tr -d ' ')" == 1 ]] \
  || {
    printf 'Sigil target contract file must contain exactly one line\n' >&2
    exit 1
  }
[[ "$(<"$contract_file")" == "$expected_contract" ]] \
  || {
    printf 'Sigil target contract does not match the approved build ABI\n' >&2
    exit 1
  }

required_consumers=(
  .github/workflows/portal-release.yml
  .github/workflows/sigil-release.yml
  AGENTS.md
  README.md
  docs/fresh-bazzite-host.md
  docs/public-alpha-uat.md
  docs/public-release-delivery.md
  scripts/install-bazzite-package.sh
  scripts/package-bazzite-release.sh
  scripts/public-alpha-uat.sh
  scripts/sign-bazzite-release.sh
  scripts/verify-sigil-bootstrap.py
  scripts/verify-sigil-release.sh
  website/install-sigil
)
for relative in "${required_consumers[@]}"; do
  grep -Fq "$expected_contract" "$repo_dir/$relative" || {
    printf 'Sigil release target contract is missing from %s\n' "$relative" >&2
    exit 1
  }
done

# The old distro suffix may remain only in immutable Bazzite hardware-UAT
# evidence and in the explicit migration note. It must never return as a
# product release asset identity.
while IFS=: read -r relative _ line; do
  relative="${relative#./}"
  case "$relative:$line" in
    .github/workflows/sigil-hardware-uat.yml:*sigil-hardware-uat-*-bazzite-x86_64.tar.gz*) ;;
    docs/hardware-uat/*:*sigil-hardware-uat-*-bazzite-x86_64.tar.gz*) ;;
    scripts/run-bazzite-hardware-uat.sh:*sigil-hardware-uat-*-bazzite-x86_64.tar.gz*) ;;
    docs/public-release-delivery.md:*candidates*using*bazzite-x86_64*) ;;
    *)
      printf 'legacy distro identity leaked outside its allowlist: %s: %s\n' \
        "$relative" "$line" >&2
      exit 1
      ;;
  esac
done < <(
  cd "$repo_dir"
  rg -n --hidden --glob '!.git/**' --glob '!target/**' \
    --glob '!scripts/tests/sigil-release-target-contract.sh' \
    --fixed-strings 'bazzite-x86_64' .
)

printf 'sigil_release_target_contract_tests=ok\n'
