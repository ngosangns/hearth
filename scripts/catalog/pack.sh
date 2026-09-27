#!/bin/bash
# Catalog packaging entry. The daemon runs `bash pack.sh <service> <out.tar.gz>`.
# Next to package.sh this just builds that service. Downloaded alone (remote catalog), it
# pulls the rest of scripts/catalog from $HEARTH_CATALOG_ORIGIN and re-execs.
set -euo pipefail

service="${1:?service name required}"
out="${2:?output tar.gz required}"
here="$(cd "$(dirname "$0")" && pwd)"

if [[ ! -f "$here/package.sh" ]]; then
  origin="${HEARTH_CATALOG_ORIGIN:?HEARTH_CATALOG_ORIGIN is not set}"
  work="$(mktemp -d)"
  # MANIFEST (written by write-catalog.py) lists every file the build needs, relative to
  # scripts/catalog, so a new payload script can't be missing from a remote install's tarball.
  mkdir -p "$work/scripts/catalog"
  curl -fsSL "$origin/scripts/catalog/MANIFEST" -o "$work/scripts/catalog/MANIFEST"
  files=()
  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ -z "$line" || "$line" == \#* ]] && continue
    files+=("scripts/catalog/$line")
  done < "$work/scripts/catalog/MANIFEST"
  [[ "${#files[@]}" -gt 0 ]] || { echo "empty MANIFEST at $origin" >&2; exit 1; }
  for rel in "${files[@]}"; do
    mkdir -p "$work/$(dirname "$rel")"
    curl -fsSL "$origin/$rel" -o "$work/$rel"
  done
  chmod +x "$work/scripts/catalog/package.sh" "$work/scripts/catalog/pack.sh"
  find "$work/scripts/catalog/payload" -type f ! -name '*.xml' -exec chmod +x {} +
  exec bash "$work/scripts/catalog/pack.sh" "$service" "$out"
fi

# shellcheck disable=SC1091
source "$here/versions.env"
"$here/package.sh" "$service"
repo="$(cd "$here/../.." && pwd)"
case "$service" in
  redis) version="$REDIS_VERSION" ;;
  mongodb) version="$MONGODB_VERSION" ;;
  minio) version="$MINIO_VERSION" ;;
  nginx) version="$NGINX_VERSION" ;;
  kafka) version="$KAFKA_VERSION" ;;
  *) echo "unknown service: $service" >&2; exit 2 ;;
esac
cp "$repo/dist/catalog/${service}-${version}-darwin-arm64.tar.gz" "$out"
