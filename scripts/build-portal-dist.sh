#!/usr/bin/env bash
# Assemble the Portal webview payload from an explicit runtime allowlist.
#
# Tauri's frontendDist previously pointed at the raw portal/ source tree, so
# every file dropped into that directory — test suites included — shipped
# verbatim inside the signed, notarized app. This script makes the release
# payload enforceable by construction: only the file classes named below can
# reach the bundle, and *.test.mjs files are rejected even if a glob would
# have matched them.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source_dir="${repo_root}/portal"
dist_dir="${repo_root}/portal-dist"

rm -rf "${dist_dir}"
mkdir -p "${dist_dir}"

# Runtime payload allowlist. Anything not matched here does not ship.
allowlist=(
  "index.html"
  "style.css"
  "main.js"
  "codecs.js"
  "audio-worklet.js"
)
while IFS= read -r module; do
  allowlist+=("$(basename "${module}")")
done < <(find "${source_dir}" -maxdepth 1 -name '*.mjs' ! -name '*.test.mjs' | sort)

for file in "${allowlist[@]}"; do
  case "${file}" in
    *.test.mjs)
      echo "refusing to bundle test module: ${file}" >&2
      exit 1
      ;;
  esac
  if [[ ! -f "${source_dir}/${file}" ]]; then
    echo "allowlisted portal payload file is missing: ${file}" >&2
    exit 1
  fi
  cp "${source_dir}/${file}" "${dist_dir}/${file}"
done

echo "portal payload: $(find "${dist_dir}" -type f | wc -l | tr -d ' ') files in ${dist_dir}"
