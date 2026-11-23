#!/usr/bin/env bash
# Locally runs what `.github/workflows/release.yml`'s `finalize` job does with
# one machine's archives: `<platform>=<archive>` pairs go through
# `nsh update-manifest`, then `check-updater-manifest.mjs` proves the manifest
# names exactly those files with their sizes and SHA-256s.
#
# The archive names come from `nsh package --target macos`, `linux` or
# `windows` with `"updater"` in `apps/desktop/nsh.json`; a release on GitHub
# builds them on each OS, so this script is for checking the manifest half.
#
#   .github/scripts/local-release-probe.sh \
#     keys/update.secret \
#     "$TMPDIR/out"
#
# with `<archive>` files in `$TMPDIR/out` already packaged.
set -euo pipefail

key="${1:?usage: local-release-probe.sh <update.secret> <dist dir> [platforms]}"
dist="${2:?usage: local-release-probe.sh <update.secret> <dist dir> [platforms]}"
platforms="${3:-macos-aarch64,macos-x86_64,linux-x86_64,linux-aarch64,windows-x86_64}"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
manifest="$dist/latest.json"

version="$(node -p "require('$root/apps/desktop/nsh.json').version")"
name="$(node -p "require('$root/apps/desktop/nsh.json').name")"

archives=()
for platform in ${platforms//,/ }; do
  case "${platform}" in
    macos-* | linux-*) ext=tar.gz ;;
    windows-*) ext=zip ;;
    *) echo "unknown platform ${platform}" >&2; exit 1 ;;
  esac
  file="$dist/${name}-${version}-${platform}.${ext}"
  if [[ ! -f "$file" ]]; then
    echo "missing ${file}; package it with 'nsh package --target <os>' first" >&2
    exit 1
  fi
  archives+=("$file")
done

notes="$root/.github/release-notes/${version}.md"
nsh update-manifest "${archives[@]}" \
  --entry "$root/apps/desktop" \
  --notes "$notes" \
  --key "$key" \
  -o "$dist"

node "$root/.github/scripts/check-updater-manifest.mjs" \
  --app "$root/apps/desktop/nsh.json" \
  --manifest "$manifest" \
  --platforms "$platforms"

echo "Manifest and signature are in ${dist}"