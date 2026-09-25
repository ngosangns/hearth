#!/bin/bash
# Attach every shipped shared service against the local tarballs (no GitHub release needed).
# Uses a throwaway HOME so it does not touch ~/.hearth/shared.
set -euo pipefail
cd "$(dirname "$0")"
# shellcheck disable=SC1091
source ./versions.env

REPO="$(cd ../.. && pwd)"
HEARTHD="${HEARTHD:-$REPO/rust/target/debug/hearthd}"
if [[ ! -x "$HEARTHD" ]]; then
  (cd "$REPO/rust" && cargo build -p hearthd)
fi

HOME_DIR="$(mktemp -d "${TMPDIR:-/tmp}/hearth-catalog-smoke.XXXXXX")"
export HOME="$HOME_DIR"
PROJECT="$HOME_DIR/project"
mkdir -p "$PROJECT"
export REPO
python3 - <<'PY'
import json, os, pathlib
root = pathlib.Path(os.environ["REPO"])
doc = json.loads((root / "catalog.json").read_text())
dist = root / "dist" / "catalog"
for name, family in doc["services"].items():
    for version, recipe in family["versions"].items():
        archive = dist / f"{name}-{version}-darwin-arm64.tar.gz"
        if not archive.is_file():
            raise SystemExit(f"missing tarball {archive}")
        recipe["artifacts"]["darwin-arm64"]["url"] = "file://" + str(archive)
out = pathlib.Path(os.environ["HOME"]) / "catalog.json"
out.write_text(json.dumps(doc))
print(out)
PY
export HEARTH_SHARED_CATALOG_URL="file://${HOME_DIR}/catalog.json"

ids=(
  "redis@${REDIS_VERSION}"
  "mongodb@${MONGODB_VERSION}"
  "minio@${MINIO_VERSION}"
  "nginx@${NGINX_VERSION}"
  "kafka@${KAFKA_VERSION}"
)

cleanup() {
  local id
  for id in "${ids[@]}"; do
    "$HEARTHD" --root "$PROJECT" shared stop "$id" >/dev/null 2>&1 || true
    "$HEARTHD" --root "$PROJECT" shared remove "$id" >/dev/null 2>&1 || true
  done
  local meta="$HOME_DIR/.hearth/shared/runtime-v1/manager.lock/metadata.json"
  if [[ -f "$meta" ]]; then
    local pid
    pid="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["pid"])' "$meta")"
    kill "$pid" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

cd "$PROJECT"
attach() {
  local id="$1"
  echo "== attach $id"
  "$HEARTHD" --root "$PROJECT" shared attach "$id" --json | tee "$HOME_DIR/${id//@/_}.json"
  "$HEARTHD" --root "$PROJECT" shared probe "$id"
}

attach "redis@${REDIS_VERSION}"
REDIS_PORT="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["port"])' "$HOME_DIR/redis_${REDIS_VERSION}.json")"
"$HOME_DIR/.hearth/shared/installs/redis/${REDIS_VERSION}/bin/redis-cli" -h 127.0.0.1 -p "$REDIS_PORT" ping | grep -q PONG

attach "nginx@${NGINX_VERSION}"
NGINX_URL="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["attachment"]["connection"]["url"])' "$HOME_DIR/nginx_${NGINX_VERSION}.json")"
curl -fsS "http://127.0.0.1:$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["port"])' "$HOME_DIR/nginx_${NGINX_VERSION}.json")/healthz" | grep -q ok
curl -fsS "$NGINX_URL" | grep -q "hearth nginx"

attach "minio@${MINIO_VERSION}"
MINIO_PORT="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["port"])' "$HOME_DIR/minio_${MINIO_VERSION}.json")"
MINIO_BUCKET="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["attachment"]["connection"]["env"]["S3_BUCKET"])' "$HOME_DIR/minio_${MINIO_VERSION}.json")"
curl -fsS "http://127.0.0.1:${MINIO_PORT}/minio/health/live" >/dev/null
MC_CONFIG_DIR="$HOME_DIR/.hearth/shared/instances/minio@${MINIO_VERSION}/mc" \
  "$HOME_DIR/.hearth/shared/installs/minio/${MINIO_VERSION}/bin/mc" ls "hearth/${MINIO_BUCKET}" >/dev/null

attach "mongodb@${MONGODB_VERSION}"
MONGO_PORT="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["port"])' "$HOME_DIR/mongodb_${MONGODB_VERSION}.json")"
MONGO_DB="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["attachment"]["connection"]["url"].rsplit("/",1)[-1])' "$HOME_DIR/mongodb_${MONGODB_VERSION}.json")"
MONGOSH_NO_TELEMETRY=1 "$HOME_DIR/.hearth/shared/installs/mongodb/${MONGODB_VERSION}/bin/mongosh" --quiet --norc --host 127.0.0.1 --port "$MONGO_PORT" "$MONGO_DB" --eval 'const d=db.hearth.findOne({_id:"hearth"}); if(!d) quit(1)'

attach "kafka@${KAFKA_VERSION}"
KAFKA_PORT="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["port"])' "$HOME_DIR/kafka_${KAFKA_VERSION}.json")"
KAFKA_TOPIC="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["attachment"]["connection"]["env"]["KAFKA_TOPIC"])' "$HOME_DIR/kafka_${KAFKA_VERSION}.json")"
export JAVA_HOME="$HOME_DIR/.hearth/shared/installs/kafka/${KAFKA_VERSION}/jre"
export PATH="$JAVA_HOME/bin:$PATH"
export KAFKA_HEAP_OPTS="-Xms64M -Xmx256M"
"$HOME_DIR/.hearth/shared/installs/kafka/${KAFKA_VERSION}/kafka/bin/kafka-topics.sh" --bootstrap-server "127.0.0.1:${KAFKA_PORT}" --list | grep -qx "$KAFKA_TOPIC"

echo "== detach leaves redis listening"
"$HEARTHD" --root "$PROJECT" shared detach "redis@${REDIS_VERSION}"
if "$HEARTHD" --root "$PROJECT" shared probe "redis@${REDIS_VERSION}"; then
  echo "probe should fail after detach" >&2
  exit 1
fi
"$HOME_DIR/.hearth/shared/installs/redis/${REDIS_VERSION}/bin/redis-cli" -h 127.0.0.1 -p "$REDIS_PORT" ping | grep -q PONG
echo "smoke ok"
