#!/usr/bin/env bash
set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/.." && pwd -P)"
site_dir="$repo_dir/website"

for command_name in bash find grep node; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    printf 'required command is missing: %s\n' "$command_name" >&2
    exit 1
  fi
done

for required_file in index.html styles.css dither.js _headers install-sigil; do
  if [[ ! -f "$site_dir/$required_file" ]]; then
    printf 'website asset is missing: %s\n' "$required_file" >&2
    exit 1
  fi
done

if find "$site_dir" -type l -print -quit | grep -q .; then
  echo 'website output must not contain symbolic links' >&2
  exit 1
fi

node --check "$site_dir/dither.js"
bash -n "$site_dir/install-sigil"
[[ -x "$site_dir/install-sigil" ]] || {
  echo 'Sigil bootstrap must be executable' >&2
  exit 1
}
grep -Fq 'href="./styles.css"' "$site_dir/index.html"
grep -Fq 'src="./dither.js"' "$site_dir/index.html"
grep -Fq 'Content-Security-Policy:' "$site_dir/_headers"
grep -Fq 'https://goq.sh/install-sigil' "$site_dir/index.html"
grep -Fq 'github.com/FelineStateMachine/goq/releases' "$site_dir/index.html"
for issue_number in 7 8 9 10; do
  grep -Fq "github.com/FelineStateMachine/goq/issues/$issue_number" "$site_dir/index.html"
done

echo 'website_static_gate=ok'
