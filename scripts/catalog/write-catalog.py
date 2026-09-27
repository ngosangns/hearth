#!/usr/bin/env python3
"""Write repo-root catalog.json from dist/catalog/*.sha256 produced by package.sh, and
scripts/catalog/MANIFEST, the file list pack.sh downloads when it runs from a remote catalog.

The daemon never checks sha256 for `script` artifacts (a rebuild is not byte-reproducible); the
recorded hash is what smoke.sh verifies when it rewrites each artifact to a file:// `url`.
"""

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
        "darwin-arm64 only. Every artifact is a script: the daemon runs scripts/catalog/pack.sh to build the "
        "tarball on demand (nothing is published as a release asset)."
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
                    # Repacked tarball (mongod + mongosh + payload): the recipe needs mongosh for
                    # the replica-set initiate in provision, which the upstream tarball lacks.
                    "artifacts": artifact("mongodb", mongodb),
                    "run": argv(
                        "{installDir}/bin/mongod",
                        "--bind_ip",
                        "127.0.0.1",
                        "--port",
                        "{port}",
                        "--dbpath",
                        "{dataDir}",
                        "--nounixsocket",
                        "--replSet",
                        "rs0",
                        "--wiredTigerCacheSizeGB",
                        "0.25",
                    ),
                    "readiness": {
                        "kind": "command",
                        "command": {
                            "shell": "perl -e 'use IO::Socket::INET; my $p = shift; exit(IO::Socket::INET->new(PeerAddr => \"127.0.0.1:$p\", Timeout => 1) ? 0 : 1)' {port}"
                        },
                    },
                    # provision runs after the first ready: hearth-provision initiates rs0
                    # (idempotent) before writing the project-db marker — rs state then persists
                    # in dataDir across restarts.
                    "provision": [argv("{installDir}/bin/hearth-provision", "{dataDir}", "{port}", "{projectDb}", "{projectUser}", "{projectId}")],
                    "connection": {"url": "mongodb://127.0.0.1:{port}/{projectDb}?replicaSet=rs0&directConnection=true", "env": {"MONGODB_URI": "{url}"}},
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
                    # Pinned, not hashed — the whole point of this recipe is being THE edge nginx.
                    # Modern macOS keeps <1024 privileged for everyone; the public :80/:443
                    # listeners come from a pf rdr anchor onto these loopback ports:
                    #   rdr pass on lo0 inet proto tcp from any to 127.0.0.1 port 80  -> 127.0.0.1 port 18080
                    #   rdr pass on lo0 inet proto tcp from any to 127.0.0.1 port 443 -> 127.0.0.1 port 18443
                    "ports": [18080, 18443],
                    "extraPortLabels": ["https"],
                    "prepare": argv("{installDir}/bin/hearth-prepare", "{dataDir}", "{port}", "{port2}"),
                    # nginx rewrites its process title (`nginx: master process …`). `exec: true`
                    # stores that settled ps line, which a plain argv identity would reject.
                    "run": {
                        "shell": "exec {installDir}/bin/nginx -p {dataDir} -c {dataDir}/nginx.conf",
                        "exec": True,
                    },
                    "readiness": {"kind": "http", "url": "http://127.0.0.1:{port}/healthz"},
                    "provision": [argv("{installDir}/bin/hearth-provision", "{dataDir}", "{port}", "{projectDb}", "{projectUser}", "{projectId}")],
                    "deprovision": [argv("{installDir}/bin/hearth-deprovision", "{dataDir}", "{port}", "{projectDb}", "{projectUser}", "{projectId}")],
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

CATALOG_DIR = ROOT / "scripts" / "catalog"
payload = sorted(
    p.relative_to(CATALOG_DIR).as_posix()
    for p in (CATALOG_DIR / "payload").rglob("*")
    if p.is_file() and p.name != ".DS_Store"
)
manifest = CATALOG_DIR / "MANIFEST"
manifest.write_text(
    "# Generated by write-catalog.py: files pack.sh fetches from $HEARTH_CATALOG_ORIGIN, relative to scripts/catalog.\n"
    + "\n".join(["pack.sh", "package.sh", "versions.env", *payload])
    + "\n"
)
print(f"wrote {manifest}")
