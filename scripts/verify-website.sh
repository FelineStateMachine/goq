#!/usr/bin/env bash
set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/.." && pwd -P)"
site_dir="$repo_dir/website"
wrangler_config="$repo_dir/wrangler.jsonc"

for command_name in bash find grep node python3; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    printf 'required command is missing: %s\n' "$command_name" >&2
    exit 1
  fi
done

for required_file in index.html styles.css dither.js _headers install-sigil portal-release.json; do
  if [[ ! -f "$site_dir/$required_file" ]]; then
    printf 'website asset is missing: %s\n' "$required_file" >&2
    exit 1
  fi
done

node - "$wrangler_config" <<'NODE'
const fs = require('node:fs');

const configPath = process.argv[2];
const config = JSON.parse(fs.readFileSync(configPath, 'utf8'));

if (config.name !== 'goq-sh') {
  throw new Error('Wrangler must deploy the existing goq-sh Worker');
}
if (config.assets?.directory !== './website') {
  throw new Error('Wrangler assets.directory must be ./website');
}
if ('main' in config || 'binding' in (config.assets ?? {})) {
  throw new Error('goq.sh must remain an assets-only Worker');
}
NODE

if find "$site_dir" -type l -print -quit | grep -q .; then
  echo 'website output must not contain symbolic links' >&2
  exit 1
fi

node --check "$site_dir/dither.js"
python3 "$repo_dir/scripts/verify-portal-release.py" website \
  --manifest "$site_dir/portal-release.json" >/dev/null
bash -n "$site_dir/install-sigil"
[[ -x "$site_dir/install-sigil" ]] || {
  echo 'Sigil bootstrap must be executable' >&2
  exit 1
}
grep -Fq 'href="./styles.css"' "$site_dir/index.html"
grep -Fq 'src="./dither.js"' "$site_dir/index.html"
grep -Fq 'Content-Security-Policy:' "$site_dir/_headers"
grep -Fq 'https://goq.sh/install-sigil' "$site_dir/index.html"
grep -Fq 'data-copy="curl -fsSL https://goq.sh/install-sigil | bash"' "$site_dir/index.html"
grep -Fq 'github.com/FelineStateMachine/goq/releases' "$site_dir/index.html"
grep -Fq 'class="portal-download disabled"' "$site_dir/index.html"
grep -Fq 'id="portal-download" aria-disabled="true"' "$site_dir/index.html"
grep -Fq 'window.fetch("./portal-release.json"' "$site_dir/dither.js"
grep -Fq 'PORTAL_BUILDS["macos-arm"] = portalBuildFromManifest(manifest)' "$site_dir/dither.js"
grep -Fq "connect-src 'self'" "$site_dir/_headers"
for issue_number in 7 8 9 10; do
  grep -Fq "github.com/FelineStateMachine/goq/issues/$issue_number" "$site_dir/index.html"
done

echo 'website_static_gate=ok'
