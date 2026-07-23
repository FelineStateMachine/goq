#!/usr/bin/env python3
"""Fail-closed policy verifier for Portal public releases."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import sys
import tomllib
from collections.abc import Callable, Sequence
from typing import Any
from urllib.parse import quote


REPOSITORY = "FelineStateMachine/goq"
PRODUCT = "portal-client"
BUNDLE_IDENTIFIER = "sh.goq.portal"
PLATFORM = "macos"
ARCHITECTURE = "arm64"
VERIFICATION = "developer-id+hardened-runtime+notarized+stapled+gatekeeper"
NON_RELEASE_FEATURES = (
    "demo-direct-node",
    "experimental-non-macos-pointer-capture",
)
TAG_PATTERN = re.compile(
    r"^v(?P<version>(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?)$"
)
SHA256_PATTERN = re.compile(r"^[0-9a-f]{64}$")
RunCommand = Callable[[Sequence[str], pathlib.Path], str]


class VerificationError(RuntimeError):
    """A release input violates the public release contract."""


def require(condition: bool, message: str) -> None:
    if not condition:
        raise VerificationError(message)


def run_command(command: Sequence[str], cwd: pathlib.Path) -> str:
    result = subprocess.run(
        command,
        cwd=cwd,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or "command failed"
        raise VerificationError(f"{command[0]} verification failed: {detail}")
    return result.stdout.strip()


def release_version(release_tag: str) -> str:
    match = TAG_PATTERN.fullmatch(release_tag)
    require(match is not None, "release tag must be an exact vVERSION tag")
    return match.group("version")


def expected_asset_names(release_tag: str) -> tuple[str, str, str]:
    version = release_version(release_tag)
    stem = f"Portal-{version}-{ARCHITECTURE}"
    return f"{stem}.dmg", f"{stem}.dmg.sha256", f"{stem}.json"


def read_toml(path: pathlib.Path) -> dict[str, Any]:
    require(path.is_file() and not path.is_symlink(), f"required TOML file is missing: {path}")
    try:
        return tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise VerificationError(f"cannot read TOML file {path}: {error}") from error


def workspace_package_versions(repo_dir: pathlib.Path) -> dict[str, str]:
    workspace = read_toml(repo_dir / "Cargo.toml")
    members = workspace.get("workspace", {}).get("members")
    require(isinstance(members, list) and members, "Cargo workspace members are missing")
    versions: dict[str, str] = {}
    for member_pattern in members:
        require(isinstance(member_pattern, str), "Cargo workspace member must be a path")
        matches = sorted(repo_dir.glob(member_pattern))
        require(matches, f"Cargo workspace member does not resolve: {member_pattern}")
        for member in matches:
            manifest = read_toml(member / "Cargo.toml")
            package = manifest.get("package", {})
            name = package.get("name")
            version = package.get("version")
            require(isinstance(name, str) and name, f"package name is missing: {member}")
            require(isinstance(version, str) and version, f"package version is missing: {member}")
            require(name not in versions, f"duplicate workspace package name: {name}")
            versions[name] = version
    return versions


def validate_source(
    repo_dir: pathlib.Path,
    release_tag: str,
    runner: RunCommand = run_command,
) -> dict[str, Any]:
    version = release_version(release_tag)
    repo_dir = repo_dir.resolve()
    require((repo_dir / ".git").exists(), "repository does not contain .git")
    status = runner(
        ["git", "status", "--porcelain=v1", "--untracked-files=all"], repo_dir
    )
    require(status == "", "release worktree must be clean")
    head = runner(["git", "rev-parse", "--verify", "HEAD"], repo_dir)
    tagged_commit = runner(
        ["git", "rev-list", "-n", "1", f"refs/tags/{release_tag}"], repo_dir
    )
    require(tagged_commit == head, f"release tag {release_tag} must resolve exactly to HEAD")

    versions = workspace_package_versions(repo_dir)
    mismatched = sorted(name for name, value in versions.items() if value != version)
    require(not mismatched, f"workspace package versions do not match {version}: {', '.join(mismatched)}")

    tauri_path = repo_dir / "src-tauri" / "tauri.conf.json"
    try:
        tauri = json.loads(tauri_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise VerificationError(f"cannot read Tauri configuration: {error}") from error
    require(tauri.get("productName") == "Portal", "unexpected Tauri product name")
    require(tauri.get("identifier") == BUNDLE_IDENTIFIER, "unexpected Tauri bundle identifier")
    require(tauri.get("version") == version, "Tauri version does not match release tag")

    portal_manifest = read_toml(repo_dir / "src-tauri" / "Cargo.toml")
    features = portal_manifest.get("features", {})
    default_features = features.get("default", [])
    require(isinstance(default_features, list), "Portal default features must be a list")
    for feature in NON_RELEASE_FEATURES:
        require(feature in features, f"Portal {feature} feature declaration is missing")
        require(feature not in default_features, f"{feature} must not be enabled by default")
    return {
        "release_tag": release_tag,
        "version": version,
        "git_commit": head,
        "workspace_packages": versions,
        "platform": PLATFORM,
        "architecture": ARCHITECTURE,
        "demo_direct_node": False,
    }


def sha256_file(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def expected_release_manifest(
    release_tag: str, git_commit: str, digest: str
) -> dict[str, Any]:
    asset, checksum_asset, _ = expected_asset_names(release_tag)
    return {
        "architecture": ARCHITECTURE,
        "architectures": [ARCHITECTURE],
        "asset": asset,
        "bundle_identifier": BUNDLE_IDENTIFIER,
        "checksum_asset": checksum_asset,
        "demo_direct_node": False,
        "developer_id_signed": True,
        "format": 1,
        "gatekeeper_verified": True,
        "git_commit": git_commit,
        "hardened_runtime": True,
        "notarized": True,
        "platform": PLATFORM,
        "product": PRODUCT,
        "release_tag": release_tag,
        "sha256": digest,
        "stapled": True,
        "version": release_version(release_tag),
    }


def validate_assets(
    repo_dir: pathlib.Path,
    release_tag: str,
    asset_dir: pathlib.Path,
    runner: RunCommand = run_command,
) -> dict[str, Any]:
    source = validate_source(repo_dir, release_tag, runner)
    asset_dir = asset_dir.resolve()
    require(asset_dir.is_dir() and not asset_dir.is_symlink(), "asset directory is missing or unsafe")
    asset, checksum_asset, manifest_asset = expected_asset_names(release_tag)
    expected_names = {asset, checksum_asset, manifest_asset}
    entries = list(asset_dir.iterdir())
    require({entry.name for entry in entries} == expected_names, "release directory must contain the exact Portal asset set")
    require(
        all(entry.is_file() and not entry.is_symlink() for entry in entries),
        "release assets must be regular files",
    )
    digest = sha256_file(asset_dir / asset)
    require(SHA256_PATTERN.fullmatch(digest) is not None, "invalid DMG SHA-256")
    checksum = (asset_dir / checksum_asset).read_text(encoding="utf-8")
    require(checksum == f"{digest}  {asset}\n", "checksum file does not exactly match the DMG")
    try:
        manifest = json.loads((asset_dir / manifest_asset).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise VerificationError(f"cannot read Portal release manifest: {error}") from error
    expected_manifest = expected_release_manifest(release_tag, source["git_commit"], digest)
    require(manifest == expected_manifest, "Portal release manifest does not match the verified source and DMG")
    return expected_manifest


def validate_release_asset_names(release_tag: str, names: Any) -> list[str]:
    require(isinstance(names, list), "GitHub asset names must be a JSON array")
    require(all(isinstance(name, str) for name in names), "GitHub asset names must be strings")
    expected = sorted(expected_asset_names(release_tag))
    require(sorted(names) == expected, "GitHub Release must contain the exact Portal asset set")
    return expected


def validate_website_manifest(path: pathlib.Path) -> dict[str, Any]:
    try:
        manifest = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise VerificationError(f"cannot read website Portal manifest: {error}") from error
    require(
        set(manifest) == {"builds", "format", "product"},
        "website Portal manifest has unexpected top-level fields",
    )
    require(manifest.get("format") == 1, "website Portal manifest format must be 1")
    require(manifest.get("product") == PRODUCT, "website Portal manifest product is invalid")
    builds = manifest.get("builds")
    require(
        isinstance(builds, dict) and set(builds) == {"macos-arm64"},
        "website manifest may describe only macOS arm64",
    )
    build = builds["macos-arm64"]
    require(isinstance(build, dict), "website macOS arm64 entry must be an object")
    available = build.get("available")
    require(isinstance(available, bool), "website availability must be boolean")
    if not available:
        require(set(build) == {"available", "reason"}, "unavailable website entry must not contain release links")
        require(isinstance(build.get("reason"), str) and build["reason"].strip(), "unavailable website entry requires a reason")
        return manifest

    required_fields = {
        "architecture",
        "asset",
        "available",
        "checksum_asset",
        "checksum_url",
        "download_url",
        "manifest_asset",
        "manifest_url",
        "platform",
        "release_tag",
        "release_url",
        "sha256",
        "verification",
        "version",
    }
    require(set(build) == required_fields, "available website entry fields are incomplete or unexpected")
    tag = build.get("release_tag")
    require(isinstance(tag, str), "website release tag is missing")
    version = release_version(tag)
    asset, checksum_asset, manifest_asset = expected_asset_names(tag)
    require(build.get("version") == version, "website version does not match release tag")
    require(build.get("platform") == PLATFORM, "website platform must be macos")
    require(build.get("architecture") == ARCHITECTURE, "website architecture must be arm64")
    require(build.get("asset") == asset, "website DMG asset name is invalid")
    require(build.get("checksum_asset") == checksum_asset, "website checksum asset name is invalid")
    require(build.get("manifest_asset") == manifest_asset, "website release manifest asset name is invalid")
    require(
        isinstance(build.get("sha256"), str) and SHA256_PATTERN.fullmatch(build["sha256"]) is not None,
        "website SHA-256 is invalid",
    )
    encoded_tag = quote(tag, safe="")
    release_base = f"https://github.com/{REPOSITORY}/releases"
    download_base = f"{release_base}/download/{encoded_tag}"
    require(build.get("download_url") == f"{download_base}/{asset}", "website download URL is not exact")
    require(build.get("checksum_url") == f"{download_base}/{checksum_asset}", "website checksum URL is not exact")
    require(build.get("manifest_url") == f"{download_base}/{manifest_asset}", "website manifest URL is not exact")
    require(build.get("release_url") == f"{release_base}/tag/{encoded_tag}", "website release URL is not exact")
    require(build.get("verification") == VERIFICATION, "website verification claim is invalid")
    return manifest


def load_json(path: pathlib.Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise VerificationError(f"cannot read JSON file {path}: {error}") from error


def parser() -> argparse.ArgumentParser:
    argument_parser = argparse.ArgumentParser(description=__doc__)
    subparsers = argument_parser.add_subparsers(dest="command", required=True)
    source_parser = subparsers.add_parser("source")
    source_parser.add_argument("--repo-dir", type=pathlib.Path, required=True)
    source_parser.add_argument("--release-tag", required=True)
    asset_parser = subparsers.add_parser("assets")
    asset_parser.add_argument("--repo-dir", type=pathlib.Path, required=True)
    asset_parser.add_argument("--release-tag", required=True)
    asset_parser.add_argument("--asset-dir", type=pathlib.Path, required=True)
    remote_parser = subparsers.add_parser("release-assets")
    remote_parser.add_argument("--release-tag", required=True)
    remote_parser.add_argument("--names-json", type=pathlib.Path, required=True)
    website_parser = subparsers.add_parser("website")
    website_parser.add_argument("--manifest", type=pathlib.Path, required=True)
    return argument_parser


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        if args.command == "source":
            result = validate_source(args.repo_dir, args.release_tag)
        elif args.command == "assets":
            result = validate_assets(args.repo_dir, args.release_tag, args.asset_dir)
        elif args.command == "release-assets":
            result = validate_release_asset_names(args.release_tag, load_json(args.names_json))
        else:
            result = validate_website_manifest(args.manifest)
    except VerificationError as error:
        print(f"Portal release verification failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
