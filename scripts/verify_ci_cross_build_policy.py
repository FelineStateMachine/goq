#!/usr/bin/env python3
"""Validate the executable semantics of CI's required complete demo gate.

The gate used to be a single serial job, and this checker pinned that job byte
for byte: the exact step list, the exact order, the exact run bodies. That
stopped anyone weakening the gate, but it equally stopped anyone speeding it
up, so a documentation pull request paid thirty minutes for checks it could not
affect.

The gate is now several parallel legs behind one required summary job. This
checker therefore asserts *invariants* rather than byte equality:

  * every leg the gate needs exists, runs the stage it claims to run, and
    cannot suppress its own failure;
  * the legs that must always run carry no `if:` at all;
  * the legs that may be skipped carry exactly the audited docs-only condition
    and nothing else;
  * the summary job depends on every other job in the workflow, so a new job
    cannot be added outside the gate;
  * the summary job fails unless each dependency succeeded or was a skip the
    docs-only scope is allowed to make;
  * the change-scope job fails closed on anything but a provably docs-only diff;
  * no step can poison the toolchain or mask a failure.

Coverage of the individual checks inside each stage is proven separately, by
executing the gate against stubbed tools in scripts/tests/demo-gate-stages.sh.
Static structure here, dynamic coverage there.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path


CHECKOUT_ACTION = "actions/checkout@8e8c483db84b4bee98b60c0593521ed34d9990e8"
GATE_SCRIPT = "./scripts/verify-demo-build.sh"
LINUX_CROSS_BUILD_GATE = "./scripts/run-linux-cross-build-gate.sh"
DEPENDENCY_SCRIPT = "./scripts/install-linux-build-deps.sh"
SUMMARY_JOB = "demo-gate"
SUMMARY_JOB_NAME = "Complete demo gate"
SCOPE_JOB = "scope"
SCOPE_CONDITION = "needs.scope.outputs.code == 'true'"
REQUIRED_RUNNER = "ubuntu-24.04"

# Legs that run the repository gate, and the stage each one must run. The gate
# script's own stage list is checked against this in verify_gate_script.
GATE_LEG_STAGES = {
    "quick-checks": "quick",
    "repo-tests": "repo-tests",
    "native-tests": "native",
    "cross-build": "cross",
    "gstreamer-gate": "gstreamer",
    "loopback": "loopback",
    "release-containment": "containment",
}

# Legs that must run for every change, documentation included. The website,
# docs, and README claim contracts are exercised by the repository test suite,
# so these legs are what keep a docs-only pull request honest.
ALWAYS_RUN_JOBS = frozenset({SCOPE_JOB, "quick-checks", "repo-tests", "decky-gate"})

# Legs the audited docs-only scope may skip. Must match the summary job body.
SKIPPABLE_JOBS = frozenset(
    {
        "native-tests",
        "cross-build",
        "gstreamer-gate",
        "loopback",
        "release-containment",
        "portal-target-matrix",
    }
)

# Tokens that would let a step or the gate script pass while something inside
# it failed, or would let a step swap out the toolchain the gate measures.
FORBIDDEN_RUN_TOKENS = (
    "|| true",
    "|| :",
    "set +e",
    "exec true",
    "trap ",
    "RUSTC_WRAPPER",
    "continue-on-error",
    ">~/.cargo/env",
    ">> ~/.cargo/env",
    ">>~/.cargo/env",
)

# The change-scope job is security logic: every path that cannot prove the diff
# is documentation must classify the change as code. Require each of those
# fail-closed branches to be present verbatim.
REQUIRED_SCOPE_BRANCHES = (
    "if [[ \"$EVENT_NAME\" != 'pull_request' ]]; then",
    "scope=full (event is not a pull request)",
    "scope=full (could not enumerate the pull request diff)",
    "scope=full (empty diff listing)",
)

# The path allowlist decides which diffs may skip the compile-heavy legs, so it
# is pinned exactly rather than by substring. Widening it by adding another case
# arm is the change this is here to catch; indentation may still change freely.
REQUIRED_SCOPE_CASE = (
    'case "$changed_path" in',
    "docs/* | website/* | *.md | LICENSE) ;;",
    "*)",
    "code=true",
    "printf 'code_path=%s\\n' \"$changed_path\"",
    ";;",
    "esac",
)

GATE_SCRIPT_PREFIX = """\
#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(cd -- "$script_dir/.." && pwd -P)"
cd "$repo_dir"
"""

DIGEST_PINNED = re.compile(r"[A-Za-z0-9._/-]+@[0-9a-f]{40}")


class PolicyError(ValueError):
    """The workflow does not meet the mandatory complete demo gate policy."""


def _indent(line: str) -> int:
    return len(line) - len(line.lstrip(" "))


def _is_content(line: str) -> bool:
    stripped = line.strip()
    return bool(stripped) and not stripped.startswith("#")


def _scalar(value: str) -> str:
    value = value.strip()
    if value[:1] in "\"'" and len(value) >= 2:
        quote = value[0]
        end = value.find(quote, 1)
        if end != -1:
            return value[1:end]
    value = re.sub(r"\s+#.*$", "", value).strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
        return value[1:-1]
    return value


def _flow_sequence(value: str) -> list[str]:
    inner = value.strip()[1:-1].strip()
    if not inner:
        return []
    return [_scalar(item) for item in inner.split(",")]


class Parser:
    """A strict parser for the YAML subset these workflows use.

    Anything it does not recognise raises PolicyError. An unparseable workflow
    is a rejected workflow, which keeps the checker fail-closed instead of
    silently ignoring a construct it does not understand.
    """

    def __init__(self, text: str) -> None:
        self.lines = text.splitlines()

    def parse(self) -> dict:
        value, index = self._block(0, 0)
        index = self._next_content(index)
        if index != len(self.lines):
            raise PolicyError(f"cannot parse workflow line: {self.lines[index]}")
        if not isinstance(value, dict):
            raise PolicyError("workflow must be a mapping at the top level")
        return value

    def _next_content(self, index: int) -> int:
        while index < len(self.lines) and not _is_content(self.lines[index]):
            index += 1
        return index

    def _block(self, index: int, indent: int) -> tuple[object, int]:
        index = self._next_content(index)
        if index >= len(self.lines) or _indent(self.lines[index]) < indent:
            return None, index
        if self.lines[index].lstrip(" ").startswith("- "):
            return self._sequence(index, indent)
        return self._mapping_from(None, index, indent)

    def _sequence(self, index: int, indent: int) -> tuple[list, int]:
        items: list[object] = []
        while True:
            index = self._next_content(index)
            if index >= len(self.lines):
                break
            line = self.lines[index]
            if _indent(line) != indent or not line.lstrip(" ").startswith("- "):
                break
            rest = line.lstrip(" ")[2:]
            if ":" in rest:
                entry, index = self._mapping_from(rest, index + 1, indent + 2)
                items.append(entry)
            else:
                items.append(_scalar(rest))
                index += 1
        return items, index

    def _mapping_from(
        self, first: str | None, index: int, indent: int
    ) -> tuple[dict, int]:
        entries: dict[str, object] = {}

        def add(key: str, value: object) -> None:
            if key in entries:
                raise PolicyError(f"duplicate key {key!r} in the workflow")
            entries[key] = value

        if first is not None:
            key, value, index = self._entry(first, index, indent)
            add(key, value)
        while True:
            index = self._next_content(index)
            if index >= len(self.lines):
                break
            line = self.lines[index]
            if _indent(line) < indent:
                break
            if _indent(line) > indent:
                raise PolicyError(f"unexpected indentation: {line}")
            if line.lstrip(" ").startswith("- "):
                break
            key, value, index = self._entry(line.lstrip(" "), index + 1, indent)
            add(key, value)
        return entries, index

    def _entry(self, text: str, index: int, indent: int) -> tuple[str, object, int]:
        match = re.fullmatch(
            r"(\"[^\"]+\"|'[^']+'|[A-Za-z0-9_.-]+):(?:\s+(.*))?", text
        )
        if not match:
            raise PolicyError(f"cannot parse workflow field: {text}")
        key = _scalar(match.group(1))
        raw = (match.group(2) or "").strip()
        if raw in ("|", "|-", ">", ">-"):
            body, index = self._block_scalar(index, indent, raw)
            return key, body, index
        if raw.startswith("["):
            return key, _flow_sequence(raw), index
        if raw:
            return key, _scalar(raw), index
        value, index = self._block(index, indent + 2)
        return key, value, index

    def _block_scalar(self, index: int, indent: int, style: str) -> tuple[str, int]:
        body: list[str] = []
        content_indent: int | None = None
        while index < len(self.lines):
            line = self.lines[index]
            if not line.strip():
                body.append("")
                index += 1
                continue
            if _indent(line) <= indent:
                break
            if content_indent is None:
                content_indent = _indent(line)
            if _indent(line) < content_indent:
                break
            body.append(line[content_indent:])
            index += 1
        text = "\n".join(body).rstrip()
        if style.endswith("-"):
            return text, index
        return text + "\n", index


def _require_mapping(value: object, what: str) -> dict:
    if not isinstance(value, dict):
        raise PolicyError(f"{what} must be a mapping")
    return value


def _steps(job: dict, name: str) -> list[dict]:
    steps = job.get("steps")
    if not isinstance(steps, list) or not steps:
        raise PolicyError(f"jobs.{name} must define a non-empty steps list")
    for step in steps:
        if not isinstance(step, dict):
            raise PolicyError(f"jobs.{name} contains a step that is not a mapping")
    return steps


def _run_body(step: dict) -> str:
    run = step.get("run")
    return run if isinstance(run, str) else ""


def _check_no_failure_suppression(job_name: str, job: dict) -> None:
    if "continue-on-error" in job:
        raise PolicyError(f"jobs.{job_name} must not suppress failures")
    if "defaults" in job:
        raise PolicyError(
            f"jobs.{job_name} must not override run defaults or shell failure behavior"
        )
    for step in _steps(job, job_name):
        if "continue-on-error" in step:
            raise PolicyError(f"jobs.{job_name} has a step that suppresses failures")
        if "if" in step:
            raise PolicyError(
                f"jobs.{job_name} has a conditionally disabled step; gate a whole leg instead"
            )
        body = _run_body(step)
        for token in FORBIDDEN_RUN_TOKENS:
            if token in body:
                raise PolicyError(
                    f"jobs.{job_name} has a step containing the forbidden token {token!r}"
                )
        uses = step.get("uses")
        if uses is None:
            continue
        if not isinstance(uses, str) or not DIGEST_PINNED.fullmatch(uses):
            raise PolicyError(
                f"jobs.{job_name} uses an action that is not digest-pinned: {uses!r}"
            )
        if uses.startswith("actions/checkout@"):
            if uses != CHECKOUT_ACTION:
                raise PolicyError(
                    "checkout action is not digest-pinned to the policy contract"
                )
            if "with" in step:
                raise PolicyError(f"jobs.{job_name} overrides the checked-out ref")


def _gate_invocation(job_name: str, job: dict) -> dict:
    matches = [step for step in _steps(job, job_name) if GATE_SCRIPT in _run_body(step)]
    if len(matches) != 1:
        raise PolicyError(
            f"jobs.{job_name} must invoke the repository gate exactly once"
        )
    return matches[0]


def _validate_triggers(workflow: dict) -> None:
    if "defaults" in workflow:
        raise PolicyError(
            "workflow-level defaults are forbidden because they can suppress gate failures"
        )
    if set(workflow) != {"name", "on", "permissions", "concurrency", "jobs"}:
        raise PolicyError(
            "workflow top-level fields changed from the CI policy contract"
        )

    triggers = _require_mapping(workflow["on"], "workflow on field")
    if "pull_request" not in triggers:
        raise PolicyError(
            "ordinary CI must contain exactly one empty pull_request trigger"
        )
    if triggers["pull_request"] is not None:
        raise PolicyError(
            "ordinary CI pull_request trigger must be exactly empty and unfiltered"
        )
    push = _require_mapping(triggers.get("push"), "workflow push trigger")
    if push.get("branches") != ["main"]:
        raise PolicyError("workflow push trigger must remain limited to main")


def _validate_scope_job(jobs: dict) -> None:
    job = _require_mapping(jobs.get(SCOPE_JOB), f"jobs.{SCOPE_JOB}")
    if "if" in job:
        raise PolicyError(f"jobs.{SCOPE_JOB} must not be conditionally disabled")
    outputs = _require_mapping(job.get("outputs"), f"jobs.{SCOPE_JOB}.outputs")
    if set(outputs) != {"code"}:
        raise PolicyError(f"jobs.{SCOPE_JOB} must publish exactly the code output")

    body = "\n".join(_run_body(step) for step in _steps(job, SCOPE_JOB))
    for branch in REQUIRED_SCOPE_BRANCHES:
        if branch not in body:
            raise PolicyError(
                f"jobs.{SCOPE_JOB} is missing its fail-closed branch: {branch}"
            )
    if body.count("code=true") < 3:
        raise PolicyError(
            f"jobs.{SCOPE_JOB} must classify every unprovable diff as code"
        )

    lines = [line.strip() for line in body.splitlines() if line.strip()]
    try:
        start = lines.index(REQUIRED_SCOPE_CASE[0])
    except ValueError:
        raise PolicyError(
            f"jobs.{SCOPE_JOB} must classify each changed path with a case statement"
        ) from None
    try:
        end = lines.index("esac", start)
    except ValueError:
        raise PolicyError(
            f"jobs.{SCOPE_JOB} path classification is not a closed case statement"
        ) from None
    if tuple(lines[start : end + 1]) != REQUIRED_SCOPE_CASE:
        raise PolicyError(
            f"jobs.{SCOPE_JOB} documentation path allowlist changed from the "
            "CI policy contract"
        )


def _validate_gate_legs(jobs: dict) -> None:
    for leg, stage in GATE_LEG_STAGES.items():
        job = _require_mapping(jobs.get(leg), f"jobs.{leg}")
        if job.get("runs-on") != REQUIRED_RUNNER:
            raise PolicyError(f"jobs.{leg} must run on {REQUIRED_RUNNER}")

        step = _gate_invocation(leg, job)
        body = _run_body(step)
        if f"--stage {stage}" not in body:
            raise PolicyError(
                f"jobs.{leg} must run the repository gate stage {stage!r}"
            )

        env = step.get("env")
        env = env if isinstance(env, dict) else {}
        if leg == "cross-build":
            if env.get("GOQ_REQUIRE_LINUX_CROSS_BUILD") != "1":
                raise PolicyError(
                    "the cross-build leg does not require the Linux cross build"
                )
            if env.get("GOQ_VERIFY_IN_PROCESS_GSTREAMER") != "1":
                raise PolicyError(
                    "the cross-build leg must cross-build the in-process GStreamer backend"
                )
        if leg == "gstreamer-gate":
            if env.get("GOQ_VERIFY_IN_PROCESS_GSTREAMER") != "1":
                raise PolicyError(
                    "the GStreamer leg does not require the in-process GStreamer gate"
                )

        installs = [
            candidate
            for candidate in _steps(job, leg)
            if DEPENDENCY_SCRIPT in _run_body(candidate)
        ]
        if len(installs) != 1:
            raise PolicyError(
                f"jobs.{leg} must install the Linux build dependencies exactly once"
            )


def _validate_summary_job(jobs: dict) -> None:
    job = _require_mapping(jobs.get(SUMMARY_JOB), f"jobs.{SUMMARY_JOB}")
    if job.get("name") != SUMMARY_JOB_NAME:
        raise PolicyError(f"jobs.{SUMMARY_JOB} must retain its required name")
    if job.get("runs-on") != REQUIRED_RUNNER:
        raise PolicyError(f"jobs.{SUMMARY_JOB} must run on {REQUIRED_RUNNER}")
    # The summary job is the one place an `if` is mandatory rather than
    # forbidden: it has to run when a leg has already failed so that it can
    # report that failure.
    if job.get("if") != "always()":
        raise PolicyError(
            f"jobs.{SUMMARY_JOB} must run with if: always() so a failed leg is reported"
        )

    needs = job.get("needs")
    if not isinstance(needs, list):
        raise PolicyError(f"jobs.{SUMMARY_JOB} must declare a needs list")
    if len(needs) != len(set(needs)):
        raise PolicyError(f"jobs.{SUMMARY_JOB} needs list contains a duplicate")
    expected = set(jobs) - {SUMMARY_JOB}
    if set(needs) != expected:
        missing = sorted(expected - set(needs))
        unknown = sorted(set(needs) - expected)
        raise PolicyError(
            f"jobs.{SUMMARY_JOB} must depend on every other job; "
            f"missing={missing} unknown={unknown}"
        )

    body = "\n".join(_run_body(step) for step in _steps(job, SUMMARY_JOB))
    if "exit 1" not in body:
        raise PolicyError(f"jobs.{SUMMARY_JOB} must fail when a leg did not pass")
    declared = re.search(r"skippable='([^']*)'", body)
    if declared is None:
        raise PolicyError(
            f"jobs.{SUMMARY_JOB} must declare which legs a docs-only diff may skip"
        )
    if set(declared.group(1).split()) != set(SKIPPABLE_JOBS):
        raise PolicyError(
            f"jobs.{SUMMARY_JOB} skippable legs changed from the CI policy contract"
        )
    for required in ("\"$SCOPE_CODE\" == 'false'", "leg_must_not_be_skipped"):
        if required not in body:
            raise PolicyError(
                f"jobs.{SUMMARY_JOB} must only tolerate a skip the docs-only scope made"
            )


def verify(workflow_text: str) -> None:
    workflow = Parser(workflow_text).parse()
    _validate_triggers(workflow)
    jobs = _require_mapping(workflow["jobs"], "top-level jobs field")

    contract = ALWAYS_RUN_JOBS | SKIPPABLE_JOBS | {SUMMARY_JOB}
    if set(jobs) != contract:
        missing = sorted(contract - set(jobs))
        unknown = sorted(set(jobs) - contract)
        raise PolicyError(
            "workflow jobs changed from the CI policy contract: "
            f"missing={missing} unknown={unknown}"
        )

    for name, job in jobs.items():
        _check_no_failure_suppression(name, _require_mapping(job, f"jobs.{name}"))

    for name in sorted(ALWAYS_RUN_JOBS):
        if "if" in jobs[name]:
            raise PolicyError(
                f"jobs.{name} must run for every change and must carry no if condition"
            )
    for name in sorted(SKIPPABLE_JOBS):
        if jobs[name].get("if") != SCOPE_CONDITION:
            raise PolicyError(
                f"jobs.{name} may only be skipped by the audited docs-only scope, "
                f"not by {jobs[name].get('if')!r}"
            )

    _validate_scope_job(jobs)
    _validate_gate_legs(jobs)
    _validate_summary_job(jobs)


def verify_gate_script(gate_script: str) -> None:
    if not gate_script.startswith(GATE_SCRIPT_PREFIX):
        raise PolicyError(
            "complete repository gate executable prefix changed before its stage dispatch"
        )

    # Checked before the structural assertions so that masking a failure is
    # reported as masking a failure rather than as a shape problem.
    for token in FORBIDDEN_RUN_TOKENS:
        if token in gate_script:
            raise PolicyError(
                f"complete repository gate contains the forbidden token {token!r}"
            )

    if gate_script.count(LINUX_CROSS_BUILD_GATE) != 1:
        raise PolicyError(
            "complete repository gate must invoke the Linux cross-build gate exactly once"
        )

    # Unconditional at its stage function's top level, so it cannot be buried in
    # a branch that never runs. scripts/tests/demo-gate-stages.sh is what proves
    # the stage actually reaches it; this only rejects the obvious shapes.
    if not re.search(rf"^  {re.escape(LINUX_CROSS_BUILD_GATE)}$", gate_script, re.M):
        raise PolicyError(
            "the Linux cross-build gate must be invoked unconditionally by its stage"
        )

    declared = re.search(r"readonly ALL_STAGES=\(([^)]*)\)", gate_script)
    if declared is None:
        raise PolicyError("complete repository gate must declare its stage list")
    stages = declared.group(1).split()
    if set(stages) != set(GATE_LEG_STAGES.values()):
        raise PolicyError(
            "complete repository gate stages changed from the CI policy contract"
        )

    for stage in stages:
        function = f"run_stage_{stage.replace('-', '_')}"
        if f"{function}()" not in gate_script:
            raise PolicyError(f"complete repository gate is missing {function}")
        if not re.search(rf"^\s*{re.escape(stage)}\) {function} ;;", gate_script, re.M):
            raise PolicyError(
                f"complete repository gate does not dispatch the {stage!r} stage"
            )

    # With no arguments the gate must still run every stage, so the release and
    # hardware-UAT workflows keep the coverage they had as one serial job.
    if 'for current_stage in "${ALL_STAGES[@]}"' not in gate_script:
        raise PolicyError(
            "complete repository gate must run every stage when invoked with no stage"
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("workflow", type=Path)
    parser.add_argument("gate_script", type=Path)
    args = parser.parse_args()
    try:
        verify(args.workflow.read_text(encoding="utf-8"))
        verify_gate_script(args.gate_script.read_text(encoding="utf-8"))
    except (OSError, PolicyError) as error:
        print(f"CI cross-build policy failed: {error}", file=sys.stderr)
        return 1
    print("ci_cross_build_policy=ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
