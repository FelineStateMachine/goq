from __future__ import annotations

import unittest
from pathlib import Path

from scripts.verify_ci_cross_build_policy import PolicyError, verify


REPO_DIR = Path(__file__).resolve().parents[2]
WORKFLOW = (REPO_DIR / ".github" / "workflows" / "ci.yml").read_text(
    encoding="utf-8"
)


def replace_once(source: str, old: str, new: str) -> str:
    if source.count(old) != 1:
        raise AssertionError(f"fixture source must contain exactly one {old!r}")
    return source.replace(old, new, 1)


def remove_named_step(source: str, name: str, next_name: str) -> tuple[str, str]:
    marker = f"      - name: {name}\n"
    next_marker = f"      - name: {next_name}\n"
    start = source.index(marker)
    end = source.index(next_marker, start)
    return source[:start] + source[end:], source[start:end]


class CiCrossBuildPolicyTests(unittest.TestCase):
    def test_repository_workflow_passes(self) -> None:
        verify(WORKFLOW)

    def test_rejects_demo_gate_run_bypass_even_with_decoy_comment(self) -> None:
        mutated = replace_once(
            WORKFLOW,
            "        run: ./scripts/verify-demo-build.sh",
            "        run: echo bypassed",
        )
        mutated += "\n# run: ./scripts/verify-demo-build.sh\n"
        with self.assertRaisesRegex(
            PolicyError, "does not execute the repository gate"
        ):
            verify(mutated)

    def test_rejects_conditionally_disabled_cross_install(self) -> None:
        mutated = replace_once(
            WORKFLOW,
            "      - name: Install pinned cross-build tools\n        run: |",
            "      - name: Install pinned cross-build tools\n"
            "        if: false\n"
            "        run: |",
        )
        with self.assertRaisesRegex(PolicyError, "unexpected or missing fields"):
            verify(mutated)

    def test_rejects_conditionally_disabled_demo_job(self) -> None:
        mutated = replace_once(
            WORKFLOW,
            "  demo-gate:\n    name: Complete demo gate",
            "  demo-gate:\n    if: false\n    name: Complete demo gate",
        )
        with self.assertRaisesRegex(PolicyError, "must not be conditionally disabled"):
            verify(mutated)

    def test_ignores_tokens_in_comments_and_later_jobs(self) -> None:
        mutated, removed = remove_named_step(
            WORKFLOW,
            "Install pinned cross-build tools",
            "Run complete demo gate",
        )
        decoy = (
            "\n# Install pinned cross-build tools\n"
            "# GOQ_REQUIRE_LINUX_CROSS_BUILD: \"1\"\n"
            "  later-decoy:\n"
            "    runs-on: ubuntu-24.04\n"
            "    steps:\n"
            f"{removed}"
        )
        with self.assertRaisesRegex(PolicyError, "must exist exactly once"):
            verify(mutated + decoy)

    def test_rejects_wrong_install_body_even_with_tokens_in_later_job(self) -> None:
        mutated = replace_once(
            WORKFLOW,
            "          cargo install cargo-zigbuild --locked --version 0.23.0",
            "          echo skipped cargo install",
        )
        mutated += (
            "\n  later-decoy:\n"
            "    runs-on: ubuntu-24.04\n"
            "    steps:\n"
            "      - name: Decoy\n"
            "        run: cargo install cargo-zigbuild --locked --version 0.23.0\n"
        )
        with self.assertRaisesRegex(PolicyError, "install step body changed"):
            verify(mutated)


if __name__ == "__main__":
    unittest.main()
