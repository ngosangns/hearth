#!/usr/bin/env python3
"""Build Hearth's Sparkle appcast.

Same shape as xmannv/xkey's `.github/scripts/generate_appcast.py`: the latest
stable GitHub release, an EdDSA signature that must already exist, and the
result deployed to GitHub Pages. Hearth's archive is the ditto zip
`Hearth-<tag>-macos.zip`, and both Sparkle version fields are the tag without
the leading `v` (CFBundleVersion and CFBundleShortVersionString are that string).

Local (the release workflow, after the zip and signature.txt are uploaded):

    python3 generate_appcast.py \
        --tag v0.14.0 --asset-name Hearth-v0.14.0-macos.zip \
        --length 1234 --signature-file signature.txt \
        --notes-file notes.txt --published-at 2026-09-30T08:51:32Z \
        --output appcast.xml

Regenerate from GitHub (workflow_dispatch), which refuses a release with no
signature.txt:

    python3 generate_appcast.py --from-github --output appcast.xml
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.request
from datetime import datetime
from typing import Optional

FEED_URL = "https://ngosangns.github.io/hearth/appcast.xml"
RELEASES_PAGE = "https://github.com/ngosangns/hearth/releases"
SIGNATURE_RE = re.compile(r"[A-Za-z0-9+/=]{80,}")


def validate_signature(signature: str, label: str) -> str:
    signature = signature.strip()
    if "sparkle:edSignature=" in signature:
        match = re.search(r'sparkle:edSignature="([^"]+)"', signature)
        if match:
            signature = match.group(1)
    if not SIGNATURE_RE.fullmatch(signature):
        print(f"Error: invalid EdDSA signature for {label}", file=sys.stderr)
        print("       Refusing to generate an appcast that Sparkle cannot validate.", file=sys.stderr)
        sys.exit(1)
    return signature


def cdata(text: str) -> str:
    return text.replace("]]>", "]]]]><![CDATA[>")


def rfc822(iso_date: str) -> str:
    dt = datetime.fromisoformat(iso_date.replace("Z", "+00:00"))
    return dt.strftime("%a, %d %b %Y %H:%M:%S %z")


def version_from_tag(tag: str) -> str:
    version = tag[1:] if tag.startswith("v") else tag
    if not re.fullmatch(r"\d+(?:\.\d+)*", version):
        print(f"Error: tag {tag} is not a numeric version Sparkle can compare", file=sys.stderr)
        sys.exit(1)
    return version


def render_appcast(
    *,
    repo: str,
    tag: str,
    asset_name: str,
    length: int,
    signature: str,
    notes: str,
    published_at: str,
) -> str:
    version = version_from_tag(tag)
    signature = validate_signature(signature, tag)
    owner, name = repo.split("/", 1)
    download = f"https://github.com/{owner}/{name}/releases/download/{tag}/{asset_name}"
    lines = [
        '<?xml version="1.0" encoding="utf-8"?>',
        '<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle" xmlns:dc="http://purl.org/dc/elements/1.1/">',
        "  <channel>",
        "    <title>Hearth Updates</title>",
        f"    <link>{FEED_URL}</link>",
        "    <description>Hearth for macOS</description>",
        "    <language>en</language>",
        "    <item>",
        f"      <title>Version {version}</title>",
        f"      <link>{RELEASES_PAGE}</link>",
        f'      <sparkle:version>{version}</sparkle:version>',
        f"      <sparkle:shortVersionString>{version}</sparkle:shortVersionString>",
        f'      <sparkle:fullReleaseNotesLink>{RELEASES_PAGE}</sparkle:fullReleaseNotesLink>',
        f"      <sparkle:minimumSystemVersion>13.0.0</sparkle:minimumSystemVersion>",
        '      <description sparkle:format="plain-text"><![CDATA[',
        cdata(notes.strip()) if notes.strip() else f"Version {version}",
        "      ]]></description>",
        f"      <pubDate>{rfc822(published_at)}</pubDate>",
        "      <enclosure",
        f'        url="{download}"',
        f'        sparkle:edSignature="{signature}"',
        f'        length="{length}"',
        '        type="application/octet-stream" />',
        "    </item>",
        "  </channel>",
        "</rss>",
        "",
    ]
    return "\n".join(lines)


def fetch_json(url: str, token: Optional[str]) -> object:
    headers = {"User-Agent": "Hearth-Appcast", "Accept": "application/vnd.github+json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(request) as response:
        return json.load(response)


def fetch_text(url: str, token: Optional[str]) -> str:
    headers = {"User-Agent": "Hearth-Appcast"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(request) as response:
        return response.read().decode()


def from_github(repo: str, token: Optional[str]) -> str:
    releases = fetch_json(f"https://api.github.com/repos/{repo}/releases?per_page=20", token)
    if not isinstance(releases, list):
        print("Error: GitHub releases response was not a list", file=sys.stderr)
        sys.exit(1)
    for release in releases:
        if release.get("draft") or release.get("prerelease"):
            continue
        tag = release["tag_name"]
        asset_name = f"Hearth-{tag}-macos.zip"
        assets = {asset["name"]: asset for asset in release.get("assets", [])}
        archive = assets.get(asset_name)
        signature_asset = assets.get("signature.txt")
        if archive is None:
            print(f"Warning: {tag} has no {asset_name}", file=sys.stderr)
            continue
        if signature_asset is None:
            print(f"Error: no EdDSA signature.txt for {tag}", file=sys.stderr)
            print("       Refusing to generate an appcast that Sparkle cannot validate.", file=sys.stderr)
            sys.exit(1)
        signature = fetch_text(signature_asset["browser_download_url"], token)
        published = release.get("published_at") or release.get("created_at")
        return render_appcast(
            repo=repo,
            tag=tag,
            asset_name=asset_name,
            length=int(archive["size"]),
            signature=signature,
            notes=release.get("body") or "",
            published_at=published,
        )
    print("Error: no stable release with a Hearth zip", file=sys.stderr)
    sys.exit(1)


def self_test() -> None:
    signature = "A" * 88
    xml = render_appcast(
        repo="ngosangns/hearth",
        tag="v0.14.0",
        asset_name="Hearth-v0.14.0-macos.zip",
        length=42,
        signature=signature,
        notes="hello ]]> world",
        published_at="2026-09-30T08:51:32Z",
    )
    assert "<sparkle:version>0.14.0</sparkle:version>" in xml
    assert "<sparkle:shortVersionString>0.14.0</sparkle:shortVersionString>" in xml
    assert "Hearth-v0.14.0-macos.zip" in xml
    assert 'length="42"' in xml
    assert f'sparkle:edSignature="{signature}"' in xml
    assert "]]]]><![CDATA[>" in xml
    assert "13.0.0" in xml
    try:
        version_from_tag("not-a-version")
    except SystemExit as exc:
        assert exc.code == 1
    else:
        raise AssertionError("a non-numeric tag must be rejected")
    print("appcast self-test ok")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--from-github", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", "ngosangns/hearth"))
    parser.add_argument("--tag")
    parser.add_argument("--asset-name")
    parser.add_argument("--length", type=int)
    parser.add_argument("--signature-file")
    parser.add_argument("--notes-file")
    parser.add_argument("--published-at")
    parser.add_argument("--output", default=os.environ.get("OUTPUT_PATH", "appcast.xml"))
    args = parser.parse_args()

    if args.self_test:
        self_test()
        return

    if args.from_github:
        xml = from_github(args.repo, os.environ.get("GITHUB_TOKEN"))
    else:
        missing = [
            name
            for name, value in (
                ("--tag", args.tag),
                ("--asset-name", args.asset_name),
                ("--length", args.length),
                ("--signature-file", args.signature_file),
                ("--published-at", args.published_at),
            )
            if value is None
        ]
        if missing:
            parser.error("local mode needs " + ", ".join(missing))
        notes = ""
        if args.notes_file:
            with open(args.notes_file, encoding="utf-8") as handle:
                notes = handle.read()
        with open(args.signature_file, encoding="utf-8") as handle:
            signature = handle.read()
        xml = render_appcast(
            repo=args.repo,
            tag=args.tag,
            asset_name=args.asset_name,
            length=args.length,
            signature=signature,
            notes=notes,
            published_at=args.published_at,
        )

    with open(args.output, "w", encoding="utf-8") as handle:
        handle.write(xml)
    print(f"wrote {args.output}")


if __name__ == "__main__":
    try:
        main()
    except urllib.error.HTTPError as error:
        print(f"Error: GitHub API {error.code} {error.reason}", file=sys.stderr)
        sys.exit(1)
