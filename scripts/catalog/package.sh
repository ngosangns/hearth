#!/bin/bash
# Build the darwin-arm64 shared-service tarballs catalog.json points at.
# Output: dist/catalog/<name>-<version>-darwin-arm64.tar.gz plus a sibling .sha256 file.
# Usage: scripts/catalog/package.sh [all|redis|mongodb|minio|nginx|kafka]
set -euo pipefail
cd "$(dirname "$0")"
# shellcheck disable=SC1091
source ./versions.env

REPO="$(cd ../.. && pwd)"
OUT="$REPO/dist/catalog"
CACHE="$OUT/cache"
WORK="$OUT/work"
NCPU="$(sysctl -n hw.ncpu)"
export COPYFILE_DISABLE=1
mkdir -p "$CACHE" "$WORK"

fetch() {
  local url="$1" dest="$2"
  if [[ -f "$dest" ]]; then
    return 0
  fi
  echo "fetch $url"
  # -C - resumes a previous .partial. dl.min.io is slow from some networks; keep the bytes we have.
  curl -fL --retry 5 --retry-delay 2 -C - -o "$dest.partial" "$url"
  mv "$dest.partial" "$dest"
}

extract_tgz() {
  local archive="$1" dest="$2"
  rm -rf "$dest"
  mkdir -p "$dest"
  tar -xzf "$archive" -C "$dest"
}

topdir() {
  local dest="$1" entry count=0 only=""
  for entry in "$dest"/*; do
    [[ -e "$entry" ]] || continue
    count=$((count + 1))
    only="$entry"
  done
  if [[ "$count" -eq 1 && -d "$only" ]]; then
    printf '%s\n' "$only"
  else
    printf '%s\n' "$dest"
  fi
}

copy_payload() {
  local service="$1" dest="$2"
  local file base
  mkdir -p "$dest"
  for file in ./payload/"$service"/hearth-*; do
    [[ -f "$file" ]] || continue
    base="$(basename "$file")"
    # Config shipped next to kafka, not as a bin script.
    if [[ "$base" == *.xml ]]; then
      continue
    fi
    cp "$file" "$dest/$base"
    chmod +x "$dest/$base"
  done
}

pack() {
  local name="$1" version="$2" stage="$3"
  local payload="$WORK/pack-$name"
  local out="$OUT/${name}-${version}-darwin-arm64.tar.gz"
  rm -rf "$payload"
  mkdir -p "$payload/$name"
  cp -R "$stage/." "$payload/$name/"
  tar -czf "$out" -C "$payload" "$name"
  shasum -a 256 "$out" | awk '{print $1}' > "$OUT/${name}-${version}.sha256"
  echo "packed $out"
  cat "$OUT/${name}-${version}.sha256"
  rm -rf "$payload" "$stage"
}

package_redis() {
  local archive="$CACHE/redis-${REDIS_VERSION}.tar.gz"
  local dest="$WORK/redis-src"
  local stage="$WORK/stage-redis"
  fetch "https://github.com/redis/redis/archive/refs/tags/${REDIS_VERSION}.tar.gz" "$archive"
  extract_tgz "$archive" "$dest"
  local src
  src="$(topdir "$dest")"
  make -C "$src" -j "$NCPU" MALLOC=libc redis-server redis-cli
  rm -rf "$stage"
  mkdir -p "$stage/bin"
  cp "$src/src/redis-server" "$src/src/redis-cli" "$stage/bin/"
  copy_payload redis "$stage/bin"
  rm -rf "$dest"
  pack redis "$REDIS_VERSION" "$stage"
}

package_mongodb() {
  local archive="$CACHE/mongodb-macos-arm64-${MONGODB_VERSION}.tgz"
  local zsh_archive="$CACHE/mongosh-${MONGOSH_VERSION}-darwin-arm64.zip"
  local dest="$WORK/mongodb-src"
  local mongosh_dest="$WORK/mongosh-src"
  local stage="$WORK/stage-mongodb"
  fetch "https://fastdl.mongodb.org/osx/mongodb-macos-arm64-${MONGODB_VERSION}.tgz" "$archive"
  fetch "https://github.com/mongodb-js/mongosh/releases/download/v${MONGOSH_VERSION}/mongosh-${MONGOSH_VERSION}-darwin-arm64.zip" "$zsh_archive"
  extract_tgz "$archive" "$dest"
  rm -rf "$mongosh_dest"
  mkdir -p "$mongosh_dest"
  unzip -q "$zsh_archive" -d "$mongosh_dest"
  local src mongod mongosh_bin
  src="$(topdir "$dest")"
  mongod="$(find "$src/bin" -type f -name mongod | head -n 1)"
  mongosh_bin="$(find "$mongosh_dest" -type d -name bin | head -n 1)"
  [[ -n "$mongod" && -n "$mongosh_bin" ]] || { echo "mongodb layout not recognized" >&2; exit 1; }
  rm -rf "$stage"
  mkdir -p "$stage/bin"
  cp "$mongod" "$stage/bin/mongod"
  cp -R "$mongosh_bin/." "$stage/bin/"
  chmod +x "$stage/bin/mongod" "$stage/bin/mongosh"
  copy_payload mongodb "$stage/bin"
  rm -rf "$dest" "$mongosh_dest"
  pack mongodb "$MONGODB_VERSION" "$stage"
}

# Official darwin binaries live on dl.min.io, which is too slow to be a practical fetch from
# some networks. Build the pinned release tags instead; the catalog sha256 is of our tarball.
# The clone is kept across runs (it is large), but only while it is still checked out at $tag —
# otherwise a bumped version in versions.env would rebuild the old source under the new name.
build_go_tool() {
  local repo="$1" tag="$2" dest="$3" out="$4"
  if [[ ! -d "$dest/.git" ]] || [[ "$(git -C "$dest" describe --tags --exact-match 2>/dev/null)" != "$tag" ]]; then
    rm -rf "$dest"
    git clone --depth 1 --branch "$tag" "$repo" "$dest"
  fi
  (cd "$dest" && CGO_ENABLED=0 go build -trimpath -o "$out" .)
}

package_minio() {
  local stage="$WORK/stage-minio"
  rm -rf "$stage"
  mkdir -p "$stage/bin"
  build_go_tool "https://github.com/minio/minio.git" "$MINIO_VERSION" "$WORK/minio-src" "$stage/bin/minio"
  build_go_tool "https://github.com/minio/mc.git" "$MC_VERSION" "$WORK/mc-src" "$stage/bin/mc"
  chmod +x "$stage/bin/minio" "$stage/bin/mc"
  copy_payload minio "$stage/bin"
  pack minio "$MINIO_VERSION" "$stage"
}

package_nginx() {
  local nginx_archive="$CACHE/nginx-${NGINX_VERSION}.tar.gz"
  local pcre_archive="$CACHE/pcre2-${PCRE2_VERSION}.tar.gz"
  local zlib_archive="$CACHE/zlib-${ZLIB_VERSION}.tar.gz"
  local openssl_archive="$CACHE/openssl-${OPENSSL_VERSION}.tar.gz"
  local nginx_dest="$WORK/nginx-src"
  local pcre_dest="$WORK/pcre-src"
  local zlib_dest="$WORK/zlib-src"
  local openssl_dest="$WORK/openssl-src"
  local stage="$WORK/stage-nginx"
  fetch "https://nginx.org/download/nginx-${NGINX_VERSION}.tar.gz" "$nginx_archive"
  fetch "https://github.com/PCRE2Project/pcre2/releases/download/pcre2-${PCRE2_VERSION}/pcre2-${PCRE2_VERSION}.tar.gz" "$pcre_archive"
  fetch "https://github.com/madler/zlib/releases/download/v${ZLIB_VERSION}/zlib-${ZLIB_VERSION}.tar.gz" "$zlib_archive"
  fetch "https://github.com/openssl/openssl/releases/download/openssl-${OPENSSL_VERSION}/openssl-${OPENSSL_VERSION}.tar.gz" "$openssl_archive"
  extract_tgz "$nginx_archive" "$nginx_dest"
  extract_tgz "$pcre_archive" "$pcre_dest"
  extract_tgz "$zlib_archive" "$zlib_dest"
  extract_tgz "$openssl_archive" "$openssl_dest"
  local nginx_src pcre_src zlib_src openssl_src
  nginx_src="$(topdir "$nginx_dest")"
  pcre_src="$(topdir "$pcre_dest")"
  zlib_src="$(topdir "$zlib_dest")"
  openssl_src="$(topdir "$openssl_dest")"
  (
    cd "$nginx_src"
    ./configure \
      --with-cc-opt="-O2 -Wno-deprecated-declarations" \
      --with-pcre="$pcre_src" \
      --with-zlib="$zlib_src" \
      --with-http_ssl_module \
      --with-http_auth_request_module \
      --with-openssl="$openssl_src"
    make -j "$NCPU"
  )
  rm -rf "$stage"
  mkdir -p "$stage/bin" "$stage/conf"
  cp "$nginx_src/objs/nginx" "$stage/bin/nginx"
  cp "$nginx_src/conf/mime.types" "$stage/conf/mime.types"
  chmod +x "$stage/bin/nginx"
  copy_payload nginx "$stage/bin"
  rm -rf "$nginx_dest" "$pcre_dest" "$zlib_dest" "$openssl_dest"
  pack nginx "$NGINX_VERSION" "$stage"
}

package_kafka() {
  local kafka_archive="$CACHE/kafka_${KAFKA_SCALA}-${KAFKA_VERSION}.tgz"
  local jre_archive="$CACHE/OpenJDK21U-jre_aarch64_mac_hotspot_${TEMURIN_VERSION}.tar.gz"
  local kafka_dest="$WORK/kafka-src"
  local jre_dest="$WORK/jre-src"
  local stage="$WORK/stage-kafka"
  fetch "https://archive.apache.org/dist/kafka/${KAFKA_VERSION}/kafka_${KAFKA_SCALA}-${KAFKA_VERSION}.tgz" "$kafka_archive"
  fetch "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-${TEMURIN_VERSION/%_*/}%2B${TEMURIN_VERSION##*_}/OpenJDK21U-jre_aarch64_mac_hotspot_${TEMURIN_VERSION}.tar.gz" "$jre_archive"
  extract_tgz "$kafka_archive" "$kafka_dest"
  extract_tgz "$jre_archive" "$jre_dest"
  local kafka_src java_bin jre_home
  kafka_src="$(topdir "$kafka_dest")"
  java_bin="$(find "$jre_dest" -type f -path '*/bin/java' | head -n 1)"
  [[ -n "$java_bin" && -d "$kafka_src/libs" ]] || { echo "kafka/jre layout not recognized" >&2; exit 1; }
  jre_home="$(cd "$(dirname "$java_bin")/.." && pwd)"
  rm -rf "$stage"
  mkdir -p "$stage/bin" "$stage/kafka"
  cp -R "$kafka_src/bin" "$kafka_src/libs" "$kafka_src/config" "$stage/kafka/"
  if [[ -d "$kafka_src/licenses" ]]; then
    cp -R "$kafka_src/licenses" "$stage/kafka/"
  fi
  cp ./payload/kafka/hearth-log4j2.xml "$stage/kafka/config/hearth-log4j2.xml"
  cp -R "$jre_home/." "$stage/jre/"
  copy_payload kafka "$stage/bin"
  chmod +x "$stage/jre/bin/java"
  rm -rf "$kafka_dest" "$jre_dest"
  pack kafka "$KAFKA_VERSION" "$stage"
}

target="${1:-all}"
case "$target" in
  all)
    package_redis
    package_mongodb
    package_minio
    package_nginx
    package_kafka
    ;;
  redis|mongodb|minio|nginx|kafka) "package_$target" ;;
  *) echo "unknown service: $target" >&2; exit 2 ;;
esac
