#!/usr/bin/env python3

from __future__ import annotations

import hashlib
import importlib.util
import json
import pathlib
import tempfile
import unittest


REPO_DIR = pathlib.Path(__file__).resolve().parents[2]
MODULE_PATH = REPO_DIR / "scripts" / "verify-portal-release.py"
SPEC = importlib.util.spec_from_file_location("portal_release_verifier", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
VERIFIER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VERIFIER)


class FakeGit:
    def __init__(self, *, dirty: bool = False, tag_commit: str = "a" * 40) -> None:
        self.dirty = dirty
        self.tag_commit = tag_commit
        self.commands: list[tuple[str, ...]] = []

    def __call__(self, command: list[str], _cwd: pathlib.Path) -> str:
        self.commands.append(tuple(command))
        if command[1] == "status":
            return " M tracked" if self.dirty else ""
        if command[1] == "rev-parse":
            return "a" * 40
        if command[1] == "rev-list":
            return self.tag_commit
        raise AssertionError(command)


class PortalReleaseVerifierTests(unittest.TestCase):
    def make_repo(self, root: pathlib.Path, version: str = "0.1.0") -> pathlib.Path:
        repo = root / "repo"
        (repo / ".git").mkdir(parents=True)
        (repo / "src-tauri").mkdir()
        (repo / "crates" / "sigil-host").mkdir(parents=True)
        (repo / "crates" / "sigil-protocol").mkdir(parents=True)
        (repo / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["crates/sigil-host", "crates/sigil-protocol", "src-tauri"]\n',
            encoding="utf-8",
        )
        for path, name in [
            (repo / "crates" / "sigil-host" / "Cargo.toml", "sigil-host"),
            (repo / "crates" / "sigil-protocol" / "Cargo.toml", "sigil-protocol"),
        ]:
            path.write_text(f'[package]\nname = "{name}"\nversion = "{version}"\n', encoding="utf-8")
        (repo / "src-tauri" / "Cargo.toml").write_text(
            f'[package]\nname = "portal"\nversion = "{version}"\n'
            '[features]\ndemo-direct-node = []\n',
            encoding="utf-8",
        )
        (repo / "src-tauri" / "tauri.conf.json").write_text(
            json.dumps(
                {
                    "productName": "Portal",
                    "identifier": "sh.goq.portal",
                    "version": version,
                }
            ),
            encoding="utf-8",
        )
        return repo

    def test_source_binds_clean_tag_head_and_all_versions(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            repo = self.make_repo(pathlib.Path(temporary))
            fake_git = FakeGit()
            result = VERIFIER.validate_source(repo, "v0.1.0", fake_git)
        self.assertEqual(result["architecture"], "arm64")
        self.assertFalse(result["demo_direct_node"])
        self.assertEqual(len(result["workspace_packages"]), 3)
        self.assertIn(("git", "status", "--porcelain=v1", "--untracked-files=all"), fake_git.commands)

    def test_source_rejects_dirty_mismatched_or_demo_default(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            repo = self.make_repo(root)
            with self.assertRaisesRegex(VERIFIER.VerificationError, "clean"):
                VERIFIER.validate_source(repo, "v0.1.0", FakeGit(dirty=True))
            with self.assertRaisesRegex(VERIFIER.VerificationError, "exactly to HEAD"):
                VERIFIER.validate_source(repo, "v0.1.0", FakeGit(tag_commit="b" * 40))
            (repo / "src-tauri" / "Cargo.toml").write_text(
                '[package]\nname = "portal"\nversion = "0.1.0"\n'
                '[features]\ndefault = ["demo-direct-node"]\ndemo-direct-node = []\n',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(VERIFIER.VerificationError, "enabled by default"):
                VERIFIER.validate_source(repo, "v0.1.0", FakeGit())

    def write_assets(self, assets: pathlib.Path) -> None:
        assets.mkdir()
        dmg_name, checksum_name, manifest_name = VERIFIER.expected_asset_names("v0.1.0")
        dmg = assets / dmg_name
        dmg.write_bytes(b"signed-notarized-dmg")
        digest = hashlib.sha256(dmg.read_bytes()).hexdigest()
        (assets / checksum_name).write_text(f"{digest}  {dmg_name}\n", encoding="utf-8")
        manifest = VERIFIER.expected_release_manifest("v0.1.0", "a" * 40, digest)
        (assets / manifest_name).write_text(json.dumps(manifest), encoding="utf-8")

    def test_assets_require_exact_names_digest_and_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            repo = self.make_repo(root)
            assets = root / "assets"
            self.write_assets(assets)
            result = VERIFIER.validate_assets(repo, "v0.1.0", assets, FakeGit())
            self.assertTrue(result["gatekeeper_verified"])
            _, checksum_name, manifest_name = VERIFIER.expected_asset_names("v0.1.0")
            original_checksum = (assets / checksum_name).read_text(encoding="utf-8")
            (assets / checksum_name).write_text("0" * 64 + "  wrong.dmg\n", encoding="utf-8")
            with self.assertRaisesRegex(VERIFIER.VerificationError, "checksum file"):
                VERIFIER.validate_assets(repo, "v0.1.0", assets, FakeGit())
            (assets / checksum_name).write_text(original_checksum, encoding="utf-8")
            manifest = json.loads((assets / manifest_name).read_text(encoding="utf-8"))
            manifest["notarized"] = False
            (assets / manifest_name).write_text(json.dumps(manifest), encoding="utf-8")
            with self.assertRaisesRegex(VERIFIER.VerificationError, "release manifest"):
                VERIFIER.validate_assets(repo, "v0.1.0", assets, FakeGit())
            self.write_assets_replacement(assets)
            (assets / "extra.txt").write_text("unexpected", encoding="utf-8")
            with self.assertRaisesRegex(VERIFIER.VerificationError, "exact Portal asset set"):
                VERIFIER.validate_assets(repo, "v0.1.0", assets, FakeGit())

    def write_assets_replacement(self, assets: pathlib.Path) -> None:
        dmg_name, checksum_name, manifest_name = VERIFIER.expected_asset_names("v0.1.0")
        digest = hashlib.sha256((assets / dmg_name).read_bytes()).hexdigest()
        (assets / checksum_name).write_text(f"{digest}  {dmg_name}\n", encoding="utf-8")
        manifest = VERIFIER.expected_release_manifest("v0.1.0", "a" * 40, digest)
        (assets / manifest_name).write_text(json.dumps(manifest), encoding="utf-8")

    def test_remote_release_names_are_exact(self) -> None:
        expected = list(VERIFIER.expected_asset_names("v0.1.0"))
        self.assertEqual(
            VERIFIER.validate_release_asset_names("v0.1.0", list(reversed(expected))),
            sorted(expected),
        )
        with self.assertRaisesRegex(VERIFIER.VerificationError, "exact Portal asset set"):
            VERIFIER.validate_release_asset_names("v0.1.0", expected + ["extra"])

    def test_website_manifest_fails_closed_and_accepts_exact_promoted_release(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = pathlib.Path(temporary) / "portal-release.json"
            unavailable = {
                "format": 1,
                "product": "portal-client",
                "builds": {
                    "macos-arm64": {
                        "available": False,
                        "reason": "No signed release has been promoted.",
                    }
                },
            }
            path.write_text(json.dumps(unavailable), encoding="utf-8")
            VERIFIER.validate_website_manifest(path)

            tag = "v0.1.0"
            asset, checksum, manifest = VERIFIER.expected_asset_names(tag)
            base = f"https://github.com/FelineStateMachine/goq/releases/download/{tag}"
            promoted = {
                "format": 1,
                "product": "portal-client",
                "builds": {
                    "macos-arm64": {
                        "architecture": "arm64",
                        "asset": asset,
                        "available": True,
                        "checksum_asset": checksum,
                        "checksum_url": f"{base}/{checksum}",
                        "download_url": f"{base}/{asset}",
                        "manifest_asset": manifest,
                        "manifest_url": f"{base}/{manifest}",
                        "platform": "macos",
                        "release_tag": tag,
                        "release_url": f"https://github.com/FelineStateMachine/goq/releases/tag/{tag}",
                        "sha256": "1" * 64,
                        "verification": VERIFIER.VERIFICATION,
                        "version": "0.1.0",
                    }
                },
            }
            path.write_text(json.dumps(promoted), encoding="utf-8")
            VERIFIER.validate_website_manifest(path)
            promoted["builds"]["macos-arm64"]["download_url"] = "https://example.invalid/dev.dmg"
            path.write_text(json.dumps(promoted), encoding="utf-8")
            with self.assertRaisesRegex(VERIFIER.VerificationError, "download URL"):
                VERIFIER.validate_website_manifest(path)

    def test_workflow_keeps_secrets_protected_and_attaches_to_shared_draft(self) -> None:
        workflow = (REPO_DIR / ".github" / "workflows" / "portal-release.yml").read_text(encoding="utf-8")
        self.assertIn("environment:\n      name: main", workflow)
        self.assertIn("workflow_dispatch:", workflow)
        self.assertIn("public-release-${{ inputs.tag }}", workflow)
        self.assertIn("release-assets", workflow)
        self.assertIn("Require the exact Sigil candidate draft", workflow)
        self.assertIn("five-asset pre-signature contract", workflow)
        self.assertNotIn("release create", workflow)
        self.assertNotIn("--draft=false --prerelease", workflow)
        self.assertNotIn("demo-direct-node", workflow)

        sigil_workflow = (REPO_DIR / ".github" / "workflows" / "sigil-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("complete six-asset public-alpha contract", sigil_workflow)
        self.assertIn("verify-portal-release.py assets", sigil_workflow)
        self.assertIn("runs-on: macos-26", sigil_workflow)
        self.assertIn("verify-macos-portal-signature.sh", sigil_workflow)
        self.assertLess(
            sigil_workflow.index("verify-portal-release.py assets"),
            sigil_workflow.index("--draft=false"),
        )
        self.assertLess(
            sigil_workflow.index("verify-macos-portal-signature.sh"),
            sigil_workflow.index("--draft=false"),
        )
        package = (REPO_DIR / "scripts" / "package-macos-client.sh").read_text(encoding="utf-8")
        self.assertIn("--target aarch64-apple-darwin", package)
        self.assertIn('[[ "$architectures" == arm64 ]]', package)
        self.assertNotIn("--expected-arch", package)
        self.assertNotIn("--features", package)


if __name__ == "__main__":
    unittest.main()
