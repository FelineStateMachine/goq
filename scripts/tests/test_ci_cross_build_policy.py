from __future__ import annotations

import unittest
from pathlib import Path

from scripts.verify_ci_cross_build_policy import PolicyError, verify, verify_gate_script


REPO_DIR = Path(__file__).resolve().parents[2]
WORKFLOW = (REPO_DIR / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
GATE_SCRIPT = (REPO_DIR / "scripts" / "verify-demo-build.sh").read_text(
    encoding="utf-8"
)


def replace_once(source: str, old: str, new: str) -> str:
    if source.count(old) != 1:
        raise AssertionError(f"fixture source must contain exactly one {old!r}")
    return source.replace(old, new, 1)


def job_marker(name: str) -> str:
    return f"  {name}:\n"


def add_job_field(source: str, job: str, field: str) -> str:
    """Insert a field as the first entry of the named job."""
    return replace_once(source, job_marker(job), f"{job_marker(job)}    {field}\n")


def in_job(source: str, job: str, old: str, new: str) -> str:
    """Replace the first occurrence of `old` at or after the named job."""
    start = source.index(job_marker(job))
    offset = source.index(old, start)
    return source[:offset] + new + source[offset + len(old) :]


class WorkflowStructureTests(unittest.TestCase):
    def test_repository_workflow_and_gate_script_pass(self) -> None:
        verify(WORKFLOW)
        verify_gate_script(GATE_SCRIPT)

    def test_rejects_extra_top_level_field(self) -> None:
        mutated = replace_once(WORKFLOW, "name: CI\n", "name: CI\nenv:\n  X: '1'\n")
        with self.assertRaisesRegex(PolicyError, "top-level fields changed"):
            verify(mutated)

    def test_rejects_failure_suppressing_workflow_shell_default(self) -> None:
        mutated = replace_once(
            WORKFLOW,
            "name: CI\n\non:",
            "name: CI\n\ndefaults:\n  run:\n    shell: 'bash {0} || true'\n\non:",
        )
        with self.assertRaisesRegex(PolicyError, "workflow-level defaults are forbidden"):
            verify(mutated)

    def test_rejects_missing_pull_request_trigger(self) -> None:
        mutated = replace_once(WORKFLOW, "  pull_request:\n", "")
        with self.assertRaisesRegex(PolicyError, "empty pull_request trigger"):
            verify(mutated)

    def test_rejects_filtered_pull_request_trigger(self) -> None:
        for filter_line in (
            "    branches: [main]",
            "    paths: ['crates/**']",
            "    types: [opened]",
        ):
            with self.subTest(filter_line=filter_line):
                mutated = replace_once(
                    WORKFLOW, "  pull_request:\n", f"  pull_request:\n{filter_line}\n"
                )
                with self.assertRaisesRegex(PolicyError, "exactly empty and unfiltered"):
                    verify(mutated)

    def test_rejects_inline_pull_request_mapping(self) -> None:
        mutated = replace_once(WORKFLOW, "  pull_request:", "  pull_request: {}")
        with self.assertRaisesRegex(PolicyError, "exactly empty and unfiltered"):
            verify(mutated)

    def test_rejects_widened_push_trigger(self) -> None:
        mutated = replace_once(
            WORKFLOW, "    branches: [main]", "    branches: [main, release]"
        )
        with self.assertRaisesRegex(PolicyError, "limited to main"):
            verify(mutated)

    def test_rejects_unparseable_workflow(self) -> None:
        mutated = replace_once(WORKFLOW, "permissions:\n", "permissions:\n\t- bogus\n")
        with self.assertRaises(PolicyError):
            verify(mutated)


class JobInventoryTests(unittest.TestCase):
    def test_rejects_job_added_outside_the_gate(self) -> None:
        mutated = WORKFLOW + (
            "\n  sneaky:\n"
            "    runs-on: ubuntu-24.04\n"
            "    steps:\n"
            "      - name: Do something unreviewed\n"
            "        run: echo unexpected\n"
        )
        with self.assertRaisesRegex(PolicyError, "jobs changed from the CI policy"):
            verify(mutated)

    def test_rejects_removed_gate_leg(self) -> None:
        start = WORKFLOW.index(job_marker("release-containment"))
        end = WORKFLOW.index(job_marker("decky-gate"))
        mutated = WORKFLOW[:start] + WORKFLOW[end:]
        with self.assertRaisesRegex(PolicyError, "jobs changed from the CI policy"):
            verify(mutated)

    def test_rejects_summary_job_missing_a_dependency(self) -> None:
        mutated = replace_once(WORKFLOW, "      - release-containment\n", "")
        with self.assertRaisesRegex(PolicyError, "must depend on every other job"):
            verify(mutated)

    def test_rejects_duplicate_summary_dependency(self) -> None:
        mutated = replace_once(
            WORKFLOW,
            "      - release-containment\n",
            "      - release-containment\n      - release-containment\n",
        )
        with self.assertRaisesRegex(PolicyError, "duplicate"):
            verify(mutated)


class SummaryJobTests(unittest.TestCase):
    def test_rejects_renamed_summary_job(self) -> None:
        mutated = replace_once(
            WORKFLOW, "    name: Complete demo gate", "    name: Optional demo gate"
        )
        with self.assertRaisesRegex(PolicyError, "required name"):
            verify(mutated)

    def test_rejects_summary_job_off_the_pinned_runner(self) -> None:
        mutated = in_job(WORKFLOW, "demo-gate", "runs-on: ubuntu-24.04", "runs-on: ubuntu-latest")
        with self.assertRaisesRegex(PolicyError, "must run on ubuntu-24.04"):
            verify(mutated)

    def test_rejects_summary_job_that_cannot_report_failures(self) -> None:
        for condition in ("if: false", "if: success()", "if: github.ref == 'x'"):
            with self.subTest(condition=condition):
                mutated = in_job(WORKFLOW, "demo-gate", "if: always()", condition)
                with self.assertRaisesRegex(PolicyError, "if: always\\(\\)"):
                    verify(mutated)

    def test_rejects_summary_job_that_never_fails(self) -> None:
        mutated = replace_once(
            WORKFLOW,
            "            echo 'complete demo gate failed' >&2\n            exit 1\n",
            "            echo 'complete demo gate failed' >&2\n",
        )
        with self.assertRaisesRegex(PolicyError, "must fail when a leg did not pass"):
            verify(mutated)

    def test_rejects_widened_skippable_leg_list(self) -> None:
        mutated = replace_once(
            WORKFLOW,
            "skippable='native-tests cross-build",
            "skippable='quick-checks repo-tests native-tests cross-build",
        )
        with self.assertRaisesRegex(PolicyError, "skippable legs changed"):
            verify(mutated)

    def test_rejects_skip_tolerated_without_the_scope_decision(self) -> None:
        mutated = replace_once(
            WORKFLOW,
            "          if [[ \"$SCOPE_CODE\" == 'false' ]]; then\n            docs_only=true\n          fi",
            "          docs_only=true",
        )
        with self.assertRaisesRegex(
            PolicyError, "only tolerate a skip the docs-only scope made"
        ):
            verify(mutated)


class LegConditionTests(unittest.TestCase):
    def test_rejects_conditionally_disabled_always_run_leg(self) -> None:
        for leg in ("quick-checks", "repo-tests", "decky-gate", "scope"):
            with self.subTest(leg=leg):
                mutated = add_job_field(WORKFLOW, leg, "if: false")
                with self.assertRaises(PolicyError):
                    verify(mutated)

    def test_rejects_quoted_conditionally_disabled_leg(self) -> None:
        mutated = add_job_field(WORKFLOW, "quick-checks", '"if": false')
        with self.assertRaises(PolicyError):
            verify(mutated)

    def test_rejects_skippable_leg_with_a_different_condition(self) -> None:
        for condition in (
            "if: false",
            "if: github.actor != 'nobody'",
            "if: needs.scope.outputs.code != 'true'",
        ):
            with self.subTest(condition=condition):
                mutated = in_job(
                    WORKFLOW,
                    "native-tests",
                    "if: needs.scope.outputs.code == 'true'",
                    condition,
                )
                with self.assertRaisesRegex(PolicyError, "audited docs-only scope"):
                    verify(mutated)

    def test_rejects_leg_continue_on_error(self) -> None:
        for leg in ("cross-build", "release-containment", "demo-gate"):
            with self.subTest(leg=leg):
                mutated = add_job_field(WORKFLOW, leg, "continue-on-error: true")
                with self.assertRaisesRegex(PolicyError, "must not suppress failures"):
                    verify(mutated)

    def test_rejects_leg_run_defaults_override(self) -> None:
        mutated = add_job_field(
            WORKFLOW, "cross-build", "defaults:\n      run:\n        shell: 'bash {0} || true'"
        )
        with self.assertRaisesRegex(PolicyError, "must not override run defaults"):
            verify(mutated)


class StepIntegrityTests(unittest.TestCase):
    def test_rejects_step_that_masks_failure(self) -> None:
        for token in ("|| true", "set +e", "trap 'exit 0' EXIT", "exec true"):
            with self.subTest(token=token):
                mutated = in_job(
                    WORKFLOW,
                    "native-tests",
                    "run: ./scripts/verify-demo-build.sh --stage native",
                    f"run: ./scripts/verify-demo-build.sh --stage native {token}",
                )
                with self.assertRaisesRegex(PolicyError, "forbidden token"):
                    verify(mutated)

    def test_rejects_step_poisoning_the_cargo_toolchain(self) -> None:
        mutated = in_job(
            WORKFLOW,
            "native-tests",
            "      - name: Run the native gate stage\n",
            "      - name: Poison Cargo environment\n"
            "        run: printf 'x\\n' 'export RUSTC_WRAPPER=true' >>~/.cargo/env\n\n"
            "      - name: Run the native gate stage\n",
        )
        with self.assertRaisesRegex(PolicyError, "forbidden token"):
            verify(mutated)

    def test_rejects_conditionally_disabled_step(self) -> None:
        mutated = in_job(
            WORKFLOW,
            "cross-build",
            "      - name: Install pinned cross-build tools\n",
            "      - name: Install pinned cross-build tools\n        if: false\n",
        )
        with self.assertRaisesRegex(PolicyError, "conditionally disabled step"):
            verify(mutated)

    def test_rejects_step_continue_on_error(self) -> None:
        mutated = in_job(
            WORKFLOW,
            "cross-build",
            "      - name: Install pinned cross-build tools\n",
            "      - name: Install pinned cross-build tools\n"
            "        continue-on-error: true\n",
        )
        with self.assertRaisesRegex(PolicyError, "step that suppresses failures"):
            verify(mutated)

    def test_rejects_action_that_is_not_digest_pinned(self) -> None:
        mutated = in_job(
            WORKFLOW,
            "quick-checks",
            "uses: actions/checkout@8e8c483db84b4bee98b60c0593521ed34d9990e8 # v6.0.1",
            "uses: actions/checkout@v6",
        )
        with self.assertRaisesRegex(PolicyError, "not digest-pinned"):
            verify(mutated)

    def test_rejects_checkout_ref_override(self) -> None:
        mutated = in_job(
            WORKFLOW,
            "cross-build",
            "uses: actions/checkout@8e8c483db84b4bee98b60c0593521ed34d9990e8 # v6.0.1",
            "uses: actions/checkout@8e8c483db84b4bee98b60c0593521ed34d9990e8 # v6.0.1\n"
            "        with:\n          ref: main",
        )
        with self.assertRaisesRegex(PolicyError, "overrides the checked-out ref"):
            verify(mutated)

    def test_rejects_swapped_checkout_digest(self) -> None:
        mutated = in_job(
            WORKFLOW,
            "quick-checks",
            "actions/checkout@8e8c483db84b4bee98b60c0593521ed34d9990e8",
            "actions/checkout@" + "b" * 40,
        )
        with self.assertRaisesRegex(PolicyError, "not digest-pinned to the policy"):
            verify(mutated)


class GateLegTests(unittest.TestCase):
    def test_rejects_leg_running_the_wrong_stage(self) -> None:
        mutated = in_job(
            WORKFLOW,
            "release-containment",
            "run: ./scripts/verify-demo-build.sh --stage containment",
            "run: ./scripts/verify-demo-build.sh --stage quick",
        )
        with self.assertRaisesRegex(PolicyError, "must run the repository gate stage"):
            verify(mutated)

    def test_rejects_leg_that_does_not_run_the_gate(self) -> None:
        mutated = in_job(
            WORKFLOW,
            "native-tests",
            "run: ./scripts/verify-demo-build.sh --stage native",
            "run: echo bypassed",
        )
        mutated += "\n# run: ./scripts/verify-demo-build.sh --stage native\n"
        with self.assertRaisesRegex(PolicyError, "must invoke the repository gate"):
            verify(mutated)

    def test_rejects_cross_build_leg_without_the_required_cross_build(self) -> None:
        mutated = in_job(
            WORKFLOW, "cross-build", 'GOQ_REQUIRE_LINUX_CROSS_BUILD: "1"', 'GOQ_REQUIRE_LINUX_CROSS_BUILD: "0"'
        )
        with self.assertRaisesRegex(PolicyError, "does not require the Linux cross build"):
            verify(mutated)

    def test_rejects_cross_build_leg_without_the_gstreamer_backend(self) -> None:
        mutated = in_job(
            WORKFLOW,
            "cross-build",
            '          GOQ_VERIFY_IN_PROCESS_GSTREAMER: "1"\n',
            "",
        )
        with self.assertRaisesRegex(PolicyError, "in-process GStreamer backend"):
            verify(mutated)

    def test_rejects_gstreamer_leg_without_its_gate(self) -> None:
        mutated = in_job(
            WORKFLOW,
            "gstreamer-gate",
            '          GOQ_VERIFY_IN_PROCESS_GSTREAMER: "1"\n',
            "",
        )
        with self.assertRaisesRegex(
            PolicyError, "does not require the in-process GStreamer gate"
        ):
            verify(mutated)

    def test_rejects_leg_that_skips_dependency_installation(self) -> None:
        mutated = in_job(
            WORKFLOW,
            "loopback",
            "        run: ./scripts/install-linux-build-deps.sh --profile full\n",
            "        run: echo skipped\n",
        )
        with self.assertRaisesRegex(PolicyError, "install the Linux build dependencies"):
            verify(mutated)

    def test_rejects_leg_off_the_pinned_runner(self) -> None:
        mutated = in_job(
            WORKFLOW, "loopback", "runs-on: ubuntu-24.04", "runs-on: ubuntu-latest"
        )
        with self.assertRaisesRegex(PolicyError, "must run on ubuntu-24.04"):
            verify(mutated)


class ScopeJobTests(unittest.TestCase):
    def test_rejects_missing_fail_closed_branch(self) -> None:
        for branch in (
            "            echo 'scope=full (event is not a pull request)'\n",
            "            echo 'scope=full (could not enumerate the pull request diff)'\n",
            "            echo 'scope=full (empty diff listing)'\n",
        ):
            with self.subTest(branch=branch):
                mutated = replace_once(WORKFLOW, branch, "")
                with self.assertRaisesRegex(PolicyError, "fail-closed branch"):
                    verify(mutated)

    def test_rejects_widened_documentation_allowlist(self) -> None:
        for arm in (
            "docs/* | website/* | *.md | LICENSE | crates/*) ;;",
            "docs/* | website/* | *.md | LICENSE | src-tauri/*) ;;",
            "*) ;;",
        ):
            with self.subTest(arm=arm):
                mutated = replace_once(
                    WORKFLOW, "docs/* | website/* | *.md | LICENSE) ;;", arm
                )
                with self.assertRaisesRegex(PolicyError, "allowlist changed"):
                    verify(mutated)

    def test_rejects_extra_allowlist_arm(self) -> None:
        mutated = replace_once(
            WORKFLOW,
            "              docs/* | website/* | *.md | LICENSE) ;;\n",
            "              docs/* | website/* | *.md | LICENSE) ;;\n"
            "              crates/*) ;;\n",
        )
        with self.assertRaisesRegex(PolicyError, "allowlist changed"):
            verify(mutated)

    def test_rejects_scope_job_that_cannot_be_trusted(self) -> None:
        mutated = add_job_field(WORKFLOW, "scope", "if: false")
        with self.assertRaisesRegex(PolicyError, "must carry no if condition"):
            verify(mutated)

    def test_rejects_extra_scope_output(self) -> None:
        mutated = replace_once(
            WORKFLOW,
            "      code: ${{ steps.classify.outputs.code }}\n",
            "      code: ${{ steps.classify.outputs.code }}\n"
            "      other: ${{ steps.classify.outputs.other }}\n",
        )
        with self.assertRaisesRegex(PolicyError, "exactly the code output"):
            verify(mutated)


class GateScriptTests(unittest.TestCase):
    def test_rejects_changed_executable_prefix(self) -> None:
        for mutation in ("set -eu\n", "set -euo pipefail\nexit 0\n"):
            with self.subTest(mutation=mutation):
                mutated = replace_once(GATE_SCRIPT, "set -euo pipefail\n", mutation)
                with self.assertRaisesRegex(PolicyError, "executable prefix changed"):
                    verify_gate_script(mutated)

    def test_rejects_failure_masked_cross_build_helper(self) -> None:
        mutated = replace_once(
            GATE_SCRIPT,
            "  ./scripts/run-linux-cross-build-gate.sh\n",
            "  ./scripts/run-linux-cross-build-gate.sh || true\n",
        )
        with self.assertRaisesRegex(PolicyError, "forbidden token"):
            verify_gate_script(mutated)

    def test_rejects_conditionally_wrapped_cross_build_helper(self) -> None:
        mutated = replace_once(
            GATE_SCRIPT,
            "  ./scripts/run-linux-cross-build-gate.sh\n",
            "  if false; then\n    ./scripts/run-linux-cross-build-gate.sh\n  fi\n",
        )
        with self.assertRaisesRegex(PolicyError, "invoked unconditionally"):
            verify_gate_script(mutated)

    def test_rejects_duplicated_or_removed_cross_build_helper(self) -> None:
        removed = replace_once(
            GATE_SCRIPT, "  ./scripts/run-linux-cross-build-gate.sh\n", ""
        )
        with self.assertRaisesRegex(PolicyError, "exactly once"):
            verify_gate_script(removed)

        duplicated = replace_once(
            GATE_SCRIPT,
            "  ./scripts/run-linux-cross-build-gate.sh\n",
            "  ./scripts/run-linux-cross-build-gate.sh\n"
            "  ./scripts/run-linux-cross-build-gate.sh\n",
        )
        with self.assertRaisesRegex(PolicyError, "exactly once"):
            verify_gate_script(duplicated)

    def test_rejects_failure_control_tokens_anywhere(self) -> None:
        for token in ("set +e", "trap 'exit 0' EXIT", "exec true"):
            with self.subTest(token=token):
                mutated = replace_once(
                    GATE_SCRIPT,
                    "run_stage_quick() {\n",
                    f"run_stage_quick() {{\n  {token}\n",
                )
                with self.assertRaisesRegex(PolicyError, "forbidden token"):
                    verify_gate_script(mutated)

    def test_rejects_dropped_stage(self) -> None:
        mutated = replace_once(
            GATE_SCRIPT,
            "readonly ALL_STAGES=(quick cross native gstreamer repo-tests loopback containment)",
            "readonly ALL_STAGES=(quick native gstreamer repo-tests loopback containment)",
        )
        with self.assertRaisesRegex(PolicyError, "stages changed"):
            verify_gate_script(mutated)

    def test_rejects_undispatched_stage(self) -> None:
        mutated = replace_once(
            GATE_SCRIPT, "    cross) run_stage_cross ;;\n", ""
        )
        with self.assertRaisesRegex(PolicyError, "does not dispatch"):
            verify_gate_script(mutated)

    def test_rejects_missing_stage_function(self) -> None:
        mutated = replace_once(
            GATE_SCRIPT, "run_stage_containment() {", "run_stage_containment_disabled() {"
        )
        with self.assertRaises(PolicyError):
            verify_gate_script(mutated)

    def test_rejects_default_run_that_skips_stages(self) -> None:
        mutated = replace_once(
            GATE_SCRIPT,
            '  for current_stage in "${ALL_STAGES[@]}"; do',
            "  for current_stage in quick; do",
        )
        with self.assertRaisesRegex(PolicyError, "must run every stage"):
            verify_gate_script(mutated)


class CacheBudgetTests(unittest.TestCase):
    """A cache entry belongs to the branch that wrote it.

    Saving from pull requests gave every open pull request its own
    multi-gigabyte set, which exhausted the repository's cache ceiling and
    evicted the entries the legs depend on. These assert that saves stay gated
    to pushes and that a leg cannot smuggle in an unpaired or mismatched entry.
    """

    def test_rejects_unconditional_cache_save(self) -> None:
        mutated = replace_once(
            WORKFLOW,
            "      - name: Save the Cargo build cache\n"
            "        if: github.event_name == 'push' && steps.cargo-cache.outputs.cache-hit != 'true'\n"
            "        uses: actions/cache/save@0400d5f644dc74513175e3cd8d07132dd4860809 # v4.2.4\n"
            "        with:\n"
            "          path: |\n"
            "            ~/.cargo/registry/index\n"
            "            ~/.cargo/registry/cache\n"
            "            ~/.cargo/git/db\n"
            "            target\n"
            "          key: cargo-repo-tests-${{ runner.os }}-${{ hashFiles('Cargo.lock', 'rust-toolchain.toml') }}\n",
            "      - name: Save the Cargo build cache\n"
            "        uses: actions/cache/save@0400d5f644dc74513175e3cd8d07132dd4860809 # v4.2.4\n"
            "        with:\n"
            "          path: |\n"
            "            ~/.cargo/registry/index\n"
            "            ~/.cargo/registry/cache\n"
            "            ~/.cargo/git/db\n"
            "            target\n"
            "          key: cargo-repo-tests-${{ runner.os }}-${{ hashFiles('Cargo.lock', 'rust-toolchain.toml') }}\n",
        )
        with self.assertRaisesRegex(PolicyError, "unconditionally"):
            verify(mutated)

    def test_rejects_cache_save_on_pull_requests(self) -> None:
        for condition in (
            "if: always()",
            "if: github.event_name == 'pull_request'",
            "if: steps.cargo-cache.outputs.cache-hit != 'true'",
        ):
            with self.subTest(condition=condition):
                mutated = in_job(
                    WORKFLOW,
                    "native-tests",
                    "if: github.event_name == 'push' && steps.cargo-cache.outputs.cache-hit != 'true'",
                    condition,
                )
                with self.assertRaisesRegex(PolicyError, "unapproved condition"):
                    verify(mutated)

    def test_rejects_conditional_step_that_is_not_a_cache_save(self) -> None:
        mutated = in_job(
            WORKFLOW,
            "native-tests",
            "      - name: Run the native gate stage\n",
            "      - name: Run the native gate stage\n"
            "        if: github.event_name == 'push' && steps.cargo-cache.outputs.cache-hit != 'true'\n",
        )
        with self.assertRaisesRegex(PolicyError, "conditionally disabled step"):
            verify(mutated)

    def test_rejects_restore_without_a_paired_save(self) -> None:
        start = WORKFLOW.index("      - name: Save the Cargo build cache\n")
        end = WORKFLOW.index("  native-tests:\n")
        mutated = WORKFLOW[:start] + WORKFLOW[end:]
        with self.assertRaisesRegex(PolicyError, "pair its Cargo cache restore"):
            verify(mutated)

    def test_rejects_save_under_a_different_key(self) -> None:
        mutated = in_job(
            WORKFLOW,
            "release-containment",
            "      - name: Save the Cargo build cache\n"
            "        if: github.event_name == 'push' && steps.cargo-cache.outputs.cache-hit != 'true'\n"
            "        uses: actions/cache/save@0400d5f644dc74513175e3cd8d07132dd4860809 # v4.2.4\n"
            "        with:\n"
            "          path: |\n"
            "            ~/.cargo/registry/index\n"
            "            ~/.cargo/registry/cache\n"
            "            ~/.cargo/git/db\n"
            "            target\n"
            "          key: cargo-containment-",
            "      - name: Save the Cargo build cache\n"
            "        if: github.event_name == 'push' && steps.cargo-cache.outputs.cache-hit != 'true'\n"
            "        uses: actions/cache/save@0400d5f644dc74513175e3cd8d07132dd4860809 # v4.2.4\n"
            "        with:\n"
            "          path: |\n"
            "            ~/.cargo/registry/index\n"
            "            ~/.cargo/registry/cache\n"
            "            ~/.cargo/git/db\n"
            "            target\n"
            "          key: cargo-elsewhere-",
        )
        with self.assertRaisesRegex(PolicyError, "different key"):
            verify(mutated)

    def test_rejects_missing_cargo_cache_id(self) -> None:
        mutated = in_job(
            WORKFLOW,
            "gstreamer-gate",
            "      - name: Restore the Cargo build cache\n        id: cargo-cache\n",
            "      - name: Restore the Cargo build cache\n",
        )
        with self.assertRaisesRegex(PolicyError, "cargo-cache id"):
            verify(mutated)

    def test_rejects_read_write_cache_beyond_the_zigbuild_binary(self) -> None:
        mutated = in_job(
            WORKFLOW,
            "cross-build",
            "          path: ~/.cargo/bin/cargo-zigbuild\n",
            "          path: target\n",
        )
        with self.assertRaisesRegex(PolicyError, "cargo-zigbuild binary"):
            verify(mutated)

    def test_shared_matrix_cache_nominates_exactly_one_writer(self) -> None:
        """Two concurrent legs must not race to reserve the same key.

        Cases 1-3 share one debug entry and case 4 owns the release entry, so
        exactly one leg per distinct cache key may carry save: "true".
        """
        legs = WORKFLOW[WORKFLOW.index(job_marker("loopback")) :]
        legs = legs[: legs.index(job_marker("release-containment"))]
        writers: dict[str, int] = {}
        cache = None
        for line in legs.splitlines():
            stripped = line.strip()
            if stripped.startswith("- case:") or stripped.startswith("cache:"):
                if stripped.startswith("cache:"):
                    cache = stripped.split(":", 1)[1].strip()
                    writers.setdefault(cache, 0)
            elif stripped.startswith("save:"):
                if stripped.split(":", 1)[1].strip().strip('"') == "true":
                    assert cache is not None
                    writers[cache] += 1
        self.assertEqual(writers, {"debug": 1, "release": 1})


if __name__ == "__main__":
    unittest.main()
