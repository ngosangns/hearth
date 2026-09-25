#!/usr/bin/env python3
"""Write repo-root catalog.json from dist/catalog/*.sha256 produced by package.sh."""

import json
import pathlib

ROOT = pathlib.Path(__file__).resolve().parents[2]
DIST = ROOT / "dist" / "catalog"
VERSIONS = {}
for line in (ROOT / "scripts" / "catalog" / "versions.env").read_text().splitlines():
    line = line.strip()
    if not line or line.startswith("#") or "=" not in line:
        continue
    key, value = line.split("=", 1)
    VERSIONS[key] = value

def sha(name: str, version: str) -> str:
    path = DIST / f"{name}-{version}.sha256"
    text = path.read_text().strip()
    if len(text) != 64 or any(c not in "0123456789abcdef" for c in text):
        raise SystemExit(f"bad sha256 in {path}: {text!r}")
    return text


def artifact(name: str, version: str) -> dict:
    # Packaging scripts produce these archives. MongoDB is the exception: catalog.json points `url`
    # at the official tarball and is not rewritten by this helper.
    return {
        "darwin-arm64": {
            "script": "scripts/catalog/pack.sh",
            "scriptArgs": [name],
            "sha256": sha(name, version),
        }
    }


def argv(*args: str) -> dict:
    return {"argv": list(args)}


redis = VERSIONS["REDIS_VERSION"]
mongodb = VERSIONS["MONGODB_VERSION"]
minio = VERSIONS["MINIO_VERSION"]
nginx = VERSIONS["NGINX_VERSION"]
kafka = VERSIONS["KAFKA_VERSION"]

document = {
    "version": 1,
    "_comment": (
        "Shared-services registry served to smp daemons over HTTPS (see docs/shared-services.md). "
        "darwin-arm64 only. Tarballs are built by scripts/catalog/package.sh and published on the catalog-v1 release."
    ),
    "services": {
        "redis": {
            "versions": {
                redis: {
                    "artifacts": artifact("redis", redis),
                    "prepare": argv("{installDir}/bin/hearth-prepare", "{dataDir}", "{port}"),
                    "run": argv("{installDir}/bin/redis-server", "{dataDir}/redis.conf"),
                    "readiness": {"kind": "command", "command": argv("{installDir}/bin/hearth-ready", "{port}")},
                    "connection": {"url": "redis://127.0.0.1:{port}/0", "env": {"REDIS_URL": "{url}"}},
                }
            }
        },
        "mongodb": {
            "versions": {
                mongodb: {
                    "artifacts": {
                        "darwin-arm64": {
                            "url": "https://fastdl.mongodb.org/osx/mongodb-macos-arm64-8.0.32.tgz",
                            "sha256": "f81cb258434d548dca7244d599c82eb339043d8dedd0b1b807870c9d263117f2",
                        }
                    },
                    "run": argv(
                        "{installDir}/bin/mongod",
                        "--bind_ip",
                        "127.0.0.1",
                        "--port",
                        "{port}",
                        "--dbpath",
                        "{dataDir}",
                        "--nounixsocket",
                    ),
                    "readiness": {
                        "kind": "command",
                        "command": {
                            "shell": "perl -e 'use IO::Socket::INET; my $p = shift; exit(IO::Socket::INET->new(PeerAddr => \"127.0.0.1:$p\", Timeout => 1) ? 0 : 1)' {port}"
                        },
                    },
                    "connection": {"url": "mongodb://127.0.0.1:{port}/{projectDb}", "env": {"MONGODB_URI": "{url}"}},
                }
            }
        },
        "minio": {
            "versions": {
                minio: {
                    "artifacts": artifact("minio", minio),
                    "additionalPorts": 1,
                    "extraPortLabels": ["console"],
                    "env": {"MINIO_ROOT_USER": "hearth", "MINIO_ROOT_PASSWORD": "hearth-local-dev"},
                    "run": argv(
                        "{installDir}/bin/minio",
                        "server",
                        "{dataDir}",
                        "--address",
                        "127.0.0.1:{port}",
                        "--console-address",
                        "127.0.0.1:{port2}",
                    ),
                    "readiness": {"kind": "http", "url": "http://127.0.0.1:{port}/minio/health/live"},
                    "provision": [argv("{installDir}/bin/hearth-provision", "{dataDir}", "{port}", "{projectBucket}", "{projectUser}", "{projectId}")],
                    "connection": {
                        "url": "http://127.0.0.1:{port}",
                        "env": {
                            "S3_ENDPOINT": "{url}",
                            "S3_BUCKET": "{projectBucket}",
                            "AWS_ACCESS_KEY_ID": "hearth",
                            "AWS_SECRET_ACCESS_KEY": "hearth-local-dev",
                            "AWS_REGION": "us-east-1",
                            "S3_FORCE_PATH_STYLE": "true",
                            "MINIO_CONSOLE_URL": "http://127.0.0.1:{port2}",
                        },
                    },
                }
            }
        },
        "nginx": {
            "versions": {
                nginx: {
                    "artifacts": artifact("nginx", nginx),
                    "prepare": argv("{installDir}/bin/hearth-prepare", "{dataDir}", "{port}"),
                    # nginx rewrites its process title (`nginx: master process …`). `exec: true`
                    # stores that settled ps line, which a plain argv identity would reject.
                    "run": {
                        "shell": "exec {installDir}/bin/nginx -p {dataDir} -c {dataDir}/nginx.conf",
                        "exec": True,
                    },
                    "readiness": {"kind": "http", "url": "http://127.0.0.1:{port}/healthz"},
                    "provision": [argv("{installDir}/bin/hearth-provision", "{dataDir}", "{port}", "{projectDb}", "{projectUser}", "{projectId}")],
                    "connection": {"url": "http://127.0.0.1:{port}/{projectId}/"},
                }
            }
        },
        "kafka": {
            "versions": {
                kafka: {
                    "artifacts": artifact("kafka", kafka),
                    "additionalPorts": 1,
                    "extraPortLabels": ["controller"],
                    "prepare": argv("{installDir}/bin/hearth-prepare", "{dataDir}", "{port}", "{port2}"),
                    "run": argv("{installDir}/jre/bin/java", "@{dataDir}/jvm.args"),
                    "readiness": {"kind": "command", "command": argv("{installDir}/bin/hearth-ready", "{port}")},
                    "provision": [argv("{installDir}/bin/hearth-provision", "{dataDir}", "{port}", "{projectDb}", "{projectUser}", "{projectId}")],
                    "connection": {"url": "127.0.0.1:{port}", "env": {"KAFKA_BROKERS": "{url}", "KAFKA_TOPIC": "{projectDb}"}},
                }
            }
        },
    },
}

out = ROOT / "catalog.json"
out.write_text(json.dumps(document, indent=2) + "\n")
print(f"wrote {out}")
