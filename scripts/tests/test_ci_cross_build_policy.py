from __future__ import annotations

import unittest
from pathlib import Path

from scripts.verify_ci_cross_build_policy import PolicyError, verify, verify_gate_script


REPO_DIR = Path(__file__).resolve().parents[2]
WORKFLOW = (REPO_DIR / ".github" / "workflows" / "ci.yml").read_text(
    encoding="utf-8"
)
GATE_SCRIPT = (REPO_DIR / "scripts" / "verify-demo-build.sh").read_text(
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
        verify_gate_script(GATE_SCRIPT)

    def test_rejects_cross_build_helper_inside_false_branch(self) -> None:
        mutated = replace_once(
            GATE_SCRIPT,
            "./scripts/run-linux-cross-build-gate.sh",
            "if false; then\n./scripts/run-linux-cross-build-gate.sh\nfi",
        )
        with self.assertRaisesRegex(
            PolicyError, "executable prefix changed"
        ):
            verify_gate_script(mutated)

    def test_rejects_successful_early_exit_before_cross_build_helper(self) -> None:
        mutated = replace_once(
            GATE_SCRIPT,
            "./scripts/run-linux-cross-build-gate.sh",
            "if true; then\n  exit 0\nfi\n"
            "./scripts/run-linux-cross-build-gate.sh",
        )
        with self.assertRaisesRegex(PolicyError, "executable prefix changed"):
            verify_gate_script(mutated)

    def test_rejects_failure_control_changes_before_cross_build_helper(self) -> None:
        prefixes = [
            "exit 0",
            "return 0",
            "trap 'exit 0' EXIT",
            "exec true",
            "set +e",
        ]
        for prefix in prefixes:
            with self.subTest(prefix=prefix):
                mutated = replace_once(
                    GATE_SCRIPT,
                    "./scripts/run-linux-cross-build-gate.sh",
                    f"{prefix}\n./scripts/run-linux-cross-build-gate.sh",
                )
                with self.assertRaisesRegex(
                    PolicyError, "executable prefix changed"
                ):
                    verify_gate_script(mutated)

    def test_rejects_failure_masked_cross_build_helper(self) -> None:
        mutated = replace_once(
            GATE_SCRIPT,
            "./scripts/run-linux-cross-build-gate.sh",
            "./scripts/run-linux-cross-build-gate.sh || true",
        )
        with self.assertRaisesRegex(PolicyError, "executable prefix changed"):
            verify_gate_script(mutated)

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

    def test_rejects_quoted_conditionally_disabled_demo_job(self) -> None:
        mutated = replace_once(
            WORKFLOW,
            "  demo-gate:\n    name: Complete demo gate",
            '  demo-gate:\n    "if": false\n    name: Complete demo gate',
        )
        with self.assertRaisesRegex(PolicyError, "must not be conditionally disabled"):
            verify(mutated)

    def test_rejects_demo_job_continue_on_error(self) -> None:
        mutated = replace_once(
            WORKFLOW,
            "  demo-gate:\n    name: Complete demo gate",
            "  demo-gate:\n    continue-on-error: true\n"
            "    name: Complete demo gate",
        )
        with self.assertRaisesRegex(PolicyError, "must not suppress failures"):
            verify(mutated)

    def test_rejects_missing_pull_request_trigger(self) -> None:
        mutated = replace_once(WORKFLOW, "  pull_request:\n", "")
        with self.assertRaisesRegex(
            PolicyError, "empty pull_request trigger"
        ):
            verify(mutated)

    def test_rejects_filtered_pull_request_trigger(self) -> None:
        filters = [
            "    branches: [main]",
            "    paths: ['crates/**']",
            "    types: [opened]",
        ]
        for filter_line in filters:
            with self.subTest(filter_line=filter_line):
                mutated = replace_once(
                    WORKFLOW,
                    "  pull_request:\n",
                    f"  pull_request:\n{filter_line}\n",
                )
                with self.assertRaisesRegex(
                    PolicyError, "exactly empty and unfiltered"
                ):
                    verify(mutated)

    def test_rejects_inline_pull_request_mapping(self) -> None:
        mutated = replace_once(WORKFLOW, "  pull_request:", "  pull_request: {}")
        with self.assertRaisesRegex(PolicyError, "must be exactly empty"):
            verify(mutated)

    def test_rejects_changed_demo_job_identity_and_execution_limits(self) -> None:
        changes = [
            ("    name: Complete demo gate", "    name: Optional demo gate", "required name"),
            (
                "  demo-gate:\n    name: Complete demo gate\n"
                "    runs-on: ubuntu-24.04",
                "  demo-gate:\n    name: Complete demo gate\n"
                "    runs-on: ubuntu-latest",
                "ubuntu-24.04",
            ),
            ("    timeout-minutes: 45", "    timeout-minutes: 5", "45 minutes"),
        ]
        for old, new, error in changes:
            with self.subTest(field=old):
                mutated = replace_once(WORKFLOW, old, new)
                with self.assertRaisesRegex(PolicyError, error):
                    verify(mutated)

    def test_rejects_changed_demo_job_environment(self) -> None:
        changes = [
            ('      CARGO_TERM_COLOR: always', '      CARGO_TERM_COLOR: never'),
            ('      RUST_BACKTRACE: "1"\n', ""),
            (
                '      RUST_BACKTRACE: "1"',
                '      RUST_BACKTRACE: "1"\n      CI_FAILURES_OPTIONAL: "1"',
            ),
        ]
        for old, new in changes:
            with self.subTest(replacement=new):
                mutated = replace_once(WORKFLOW, old, new)
                with self.assertRaisesRegex(
                    PolicyError, "env changed from the CI policy contract"
                ):
                    verify(mutated)

    def test_rejects_failure_suppressing_workflow_shell_default(self) -> None:
        mutated = replace_once(
            WORKFLOW,
            "name: CI\n\non:",
            "name: CI\n\ndefaults:\n"
            "  run:\n"
            "    shell: 'bash {0} || true'\n\n"
            "on:",
        )
        with self.assertRaisesRegex(PolicyError, "workflow-level defaults are forbidden"):
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
