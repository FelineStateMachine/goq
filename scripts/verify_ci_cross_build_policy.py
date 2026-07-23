#!/usr/bin/env python3
"""Validate the executable semantics of CI's required glibc cross-build path."""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path


CACHE_ACTION = "actions/cache@0400d5f644dc74513175e3cd8d07132dd4860809"
CACHE_PATH = "~/.cargo/bin/cargo-zigbuild"
CACHE_KEY = "cargo-zigbuild-${{ runner.os }}-${{ runner.arch }}-0.23.0"
LINUX_CROSS_BUILD_GATE = "./scripts/run-linux-cross-build-gate.sh"
MAPPING_KEY = r"""(?:[A-Za-z0-9_-]+|"[A-Za-z0-9_-]+"|'[A-Za-z0-9_-]+')"""

INSTALL_RUN = """\
python3 -m venv "$RUNNER_TEMP/zig-venv"
printf '%s\\n' \\
  'ziglang==0.16.0 --hash=sha256:9fcda73f62b851dd72a54b710ad40a209896db14cfb13649e62191243556342b' \\
  | "$RUNNER_TEMP/zig-venv/bin/pip" install \\
      --disable-pip-version-check \\
      --only-binary=:all: \\
      --require-hashes \\
      --requirement /dev/stdin
install -d "$RUNNER_TEMP/ci-bin"
# Expand runner paths when the generated wrapper executes.
# shellcheck disable=SC2016
printf '%s\\n' \\
  '#!/usr/bin/env bash' \\
  'exec "$RUNNER_TEMP/zig-venv/bin/python" -m ziglang "$@"' \\
  >"$RUNNER_TEMP/ci-bin/zig"
chmod 0755 "$RUNNER_TEMP/ci-bin/zig"
echo "$RUNNER_TEMP/ci-bin" >>"$GITHUB_PATH"
export PATH="$RUNNER_TEMP/ci-bin:$PATH"
if ! cargo-zigbuild --version 2>/dev/null \\
  | grep -Fxq 'cargo-zigbuild 0.23.0'; then
  cargo install cargo-zigbuild --locked --version 0.23.0
fi
test "$(zig version)" = 0.16.0
test "$(cargo-zigbuild --version)" = 'cargo-zigbuild 0.23.0'"""


class PolicyError(ValueError):
    """The workflow does not meet the mandatory cross-build policy."""


@dataclass
class Step:
    name: str
    scalars: dict[str, str] = field(default_factory=dict)
    mappings: dict[str, dict[str, str]] = field(default_factory=dict)
    blocks: dict[str, str] = field(default_factory=dict)

    @property
    def field_names(self) -> set[str]:
        return {"name", *self.scalars, *self.mappings, *self.blocks}


def _indent(line: str) -> int:
    return len(line) - len(line.lstrip(" "))


def _is_content(line: str) -> bool:
    stripped = line.strip()
    return bool(stripped) and not stripped.startswith("#")


def _scalar(value: str) -> str:
    value = value.split(" #", 1)[0].strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
        return value[1:-1]
    return value


def _mapping_entry(line: str, indent: int) -> tuple[str, str] | None:
    if _indent(line) != indent or not _is_content(line):
        return None
    match = re.fullmatch(
        rf" {{{indent}}}({MAPPING_KEY}):(?:\s*(.*))?",
        line,
    )
    if not match:
        return None
    return _scalar(match.group(1)), (match.group(2) or "").strip()


def _mapping_key(line: str, indent: int) -> str | None:
    entry = _mapping_entry(line, indent)
    return entry[0] if entry else None


def _block_end(lines: list[str], start: int, indent: int) -> int:
    for index in range(start + 1, len(lines)):
        if _mapping_entry(lines[index], indent) is not None:
            return index
        if _is_content(lines[index]) and _indent(lines[index]) < indent:
            return index
    return len(lines)


def _validate_workflow_layout(workflow: str) -> None:
    lines = workflow.splitlines()
    for line in lines:
        if _is_content(line) and _indent(line) == 0:
            if _mapping_entry(line, 0) is None:
                raise PolicyError(f"cannot parse top-level workflow field: {line}")
    top_level = [
        (index, entry)
        for index, line in enumerate(lines)
        if (entry := _mapping_entry(line, 0)) is not None
    ]
    top_level_names = [key for _, (key, _) in top_level]
    if len(top_level_names) != len(set(top_level_names)):
        raise PolicyError("workflow contains a duplicate top-level field")

    if any(key == "defaults" for _, (key, _) in top_level):
        raise PolicyError(
            "workflow-level defaults are forbidden because they can suppress gate failures"
        )
    allowed_top_level = {"name", "on", "permissions", "concurrency", "jobs"}
    if set(top_level_names) != allowed_top_level:
        raise PolicyError("workflow top-level fields changed from the CI policy contract")

    on_entries = [(index, value) for index, (key, value) in top_level if key == "on"]
    if len(on_entries) != 1:
        raise PolicyError("workflow must contain exactly one top-level on mapping")
    on_start, on_value = on_entries[0]
    if _scalar(on_value):
        raise PolicyError("workflow on field must be a trigger mapping")
    on_end = _block_end(lines, on_start, 0)
    pull_request_entries = [
        _mapping_entry(line, 2)
        for line in lines[on_start + 1 : on_end]
        if _mapping_key(line, 2) == "pull_request"
    ]
    if len(pull_request_entries) != 1 or _scalar(pull_request_entries[0][1]):
        raise PolicyError(
            "ordinary CI must contain exactly one unconditional pull_request trigger"
        )


def _job_lines(workflow: str, job_name: str) -> list[str]:
    lines = workflow.splitlines()
    jobs_indices = [
        index
        for index, line in enumerate(lines)
        if _mapping_key(line, 0) == "jobs"
    ]
    if len(jobs_indices) != 1:
        raise PolicyError("workflow must contain exactly one top-level jobs mapping")
    jobs_entry = _mapping_entry(lines[jobs_indices[0]], 0)
    if jobs_entry is None or _scalar(jobs_entry[1]):
        raise PolicyError("top-level jobs field must be a mapping")

    jobs_start = jobs_indices[0] + 1
    jobs_end = len(lines)
    for index in range(jobs_start, len(lines)):
        if _is_content(lines[index]) and _indent(lines[index]) == 0:
            jobs_end = index
            break

    matches = [
        index
        for index in range(jobs_start, jobs_end)
        if _mapping_key(lines[index], 2) == job_name
    ]
    if len(matches) != 1:
        raise PolicyError(f"jobs.{job_name} must exist exactly once")

    job_start = matches[0]
    job_entry = _mapping_entry(lines[job_start], 2)
    if job_entry is None or _scalar(job_entry[1]):
        raise PolicyError(f"jobs.{job_name} must be a mapping")
    job_end = jobs_end
    for index in range(job_start + 1, jobs_end):
        if _mapping_key(lines[index], 2) is not None:
            job_end = index
            break
    return lines[job_start:job_end]


def _parse_step(block: list[str]) -> Step:
    match = re.fullmatch(r"      - name:\s*(.+?)\s*", block[0])
    if not match:
        raise PolicyError("every demo-gate step must begin with an explicit name")
    step = Step(name=_scalar(match.group(1)))

    index = 1
    while index < len(block):
        line = block[index]
        if not _is_content(line):
            index += 1
            continue
        if _indent(line) != 8:
            raise PolicyError(f"unexpected YAML structure in step {step.name!r}: {line}")
        match = re.fullmatch(rf"        ({MAPPING_KEY}):(?:\s*(.*))?", line)
        if not match:
            raise PolicyError(f"cannot parse field in step {step.name!r}: {line}")
        key = _scalar(match.group(1))
        value = (match.group(2) or "").strip()
        if key in step.field_names:
            raise PolicyError(f"duplicate field {key!r} in step {step.name!r}")

        if value == "|":
            index += 1
            body: list[str] = []
            while index < len(block):
                child = block[index]
                if _is_content(child) and _indent(child) <= 8:
                    break
                if child.strip():
                    if _indent(child) < 10:
                        raise PolicyError(
                            f"invalid block indentation in step {step.name!r}"
                        )
                    body.append(child[10:])
                else:
                    body.append("")
                index += 1
            step.blocks[key] = "\n".join(body).rstrip()
            continue

        if not value:
            index += 1
            entries: dict[str, str] = {}
            while index < len(block):
                child = block[index]
                if not _is_content(child):
                    index += 1
                    continue
                if _indent(child) <= 8:
                    break
                child_match = re.fullmatch(
                    rf"          ({MAPPING_KEY}):\s*(.+?)\s*", child
                )
                if not child_match:
                    raise PolicyError(
                        f"cannot parse {key!r} mapping in step {step.name!r}: {child}"
                    )
                child_key = _scalar(child_match.group(1))
                if child_key in entries:
                    raise PolicyError(
                        f"duplicate {key}.{child_key} in step {step.name!r}"
                    )
                entries[child_key] = _scalar(child_match.group(2))
                index += 1
            step.mappings[key] = entries
            continue

        step.scalars[key] = _scalar(value)
        index += 1

    return step


def _parse_steps(job: list[str]) -> list[Step]:
    job_entries: list[tuple[str, str]] = []
    for line in job:
        if not _is_content(line) or _indent(line) != 4:
            continue
        entry = _mapping_entry(line, 4)
        if entry is None:
            raise PolicyError(f"cannot parse jobs.demo-gate field: {line}")
        job_entries.append(entry)
    job_field_list = [key for key, _ in job_entries]
    if len(job_field_list) != len(set(job_field_list)):
        raise PolicyError("jobs.demo-gate contains a duplicate field")
    job_fields = set(job_field_list)
    if "if" in job_fields:
        raise PolicyError("jobs.demo-gate must not be conditionally disabled")
    if "continue-on-error" in job_fields:
        raise PolicyError("jobs.demo-gate must not suppress failures")
    if "defaults" in job_fields:
        raise PolicyError(
            "jobs.demo-gate must not override run defaults or shell failure behavior"
        )
    if job_fields != {"name", "runs-on", "timeout-minutes", "env", "steps"}:
        raise PolicyError("jobs.demo-gate fields changed from the CI policy contract")

    steps_indices = [
        index
        for index, line in enumerate(job)
        if line == "    steps:"
    ]
    if len(steps_indices) != 1:
        raise PolicyError("jobs.demo-gate must contain exactly one steps list")

    start = steps_indices[0] + 1
    starts = [
        index
        for index in range(start, len(job))
        if re.fullmatch(r"      - name:\s*.+", job[index])
    ]
    if not starts:
        raise PolicyError("jobs.demo-gate contains no named steps")

    steps: list[Step] = []
    for position, step_start in enumerate(starts):
        step_end = starts[position + 1] if position + 1 < len(starts) else len(job)
        steps.append(_parse_step(job[step_start:step_end]))
    return steps


def _named_step(steps: list[Step], name: str) -> tuple[int, Step]:
    matches = [
        (index, step)
        for index, step in enumerate(steps)
        if step.name == name
    ]
    if len(matches) != 1:
        raise PolicyError(f"demo-gate step {name!r} must exist exactly once")
    return matches[0]


def _shell_compound_stack(lines: list[str], stop: int) -> list[str]:
    stack: list[str] = []
    closers = {
        "fi": "if",
        "done": "loop",
        "esac": "case",
        "}": "brace",
        ")": "subshell",
    }

    for number, line in enumerate(lines[:stop], start=1):
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue

        if stripped in closers:
            expected = closers[stripped]
            if not stack or stack[-1] != expected:
                raise PolicyError(
                    f"cannot prove shell control flow before cross-build gate at line {number}"
                )
            stack.pop()
            continue

        if re.match(
            r"^(?:function\s+)?[A-Za-z_][A-Za-z0-9_]*(?:\(\))?\s*\{\s*$",
            stripped,
        ):
            stack.append("brace")
        elif re.search(r"(?:^|\|\||&&)\s*\{\s*$", stripped):
            stack.append("brace")
        elif re.search(r"(?:^|\|\||&&)\s*\(\s*$", stripped):
            stack.append("subshell")
        elif re.match(r"^if\b", stripped):
            stack.append("if")
        elif re.match(r"^(?:for|while|until|select)\b", stripped):
            stack.append("loop")
        elif re.match(r"^case\b", stripped):
            stack.append("case")

    return stack


def verify_gate_script(gate_script: str) -> None:
    lines = gate_script.splitlines()
    calls = [
        index
        for index, line in enumerate(lines)
        if line.strip() == LINUX_CROSS_BUILD_GATE
    ]
    if len(calls) != 1:
        raise PolicyError(
            "complete repository gate must invoke the Linux cross-build gate exactly once"
        )

    call = calls[0]
    if lines[call] != LINUX_CROSS_BUILD_GATE:
        raise PolicyError(
            "Linux cross-build gate must be an unindented top-level simple command"
        )
    if _shell_compound_stack(lines, call):
        raise PolicyError(
            "Linux cross-build gate must not be nested in conditional or compound shell control flow"
        )

    content_before = [
        line.strip()
        for line in lines[:call]
        if _is_content(line)
    ]
    content_after = [
        line.strip()
        for line in lines[call + 1 :]
        if _is_content(line)
    ]
    if not content_before or content_before[-1] != "fi":
        raise PolicyError(
            "Linux cross-build gate placement changed before the required frontend gates"
        )
    if (
        not content_after
        or content_after[0] != "while IFS= read -r frontend_source; do"
    ):
        raise PolicyError(
            "Linux cross-build gate placement changed before the required frontend gates"
        )


def verify(workflow: str) -> None:
    _validate_workflow_layout(workflow)
    steps = _parse_steps(_job_lines(workflow, "demo-gate"))

    dependency_index, dependency = _named_step(
        steps, "Install Linux build dependencies"
    )
    cache_index, cache = _named_step(steps, "Restore pinned cargo-zigbuild")
    install_index, install = _named_step(steps, "Install pinned cross-build tools")
    gate_index, gate = _named_step(steps, "Run complete demo gate")

    dependency_run = dependency.blocks.get("run", "")
    if "  python3-venv \\" not in dependency_run.splitlines():
        raise PolicyError(
            "Linux dependency step must install python3-venv for the pinned Zig venv"
        )
    if "if" in dependency.field_names:
        raise PolicyError("Linux dependency step must not be conditionally disabled")

    if cache.field_names != {"name", "uses", "with"}:
        raise PolicyError("cargo-zigbuild cache step has unexpected or missing fields")
    if cache.scalars.get("uses") != CACHE_ACTION:
        raise PolicyError("cargo-zigbuild cache action is not digest-pinned")
    if cache.mappings.get("with") != {"path": CACHE_PATH, "key": CACHE_KEY}:
        raise PolicyError("cargo-zigbuild cache contract changed")

    if install.field_names != {"name", "run"}:
        raise PolicyError("cross-build install step has unexpected or missing fields")
    if install.blocks.get("run") != INSTALL_RUN:
        raise PolicyError("cross-build install step body changed")

    if gate.field_names != {"name", "env", "run"}:
        raise PolicyError("complete demo gate step has unexpected or missing fields")
    if gate.mappings.get("env") != {
        "GOQ_VERIFY_IN_PROCESS_GSTREAMER": "1",
        "GOQ_REQUIRE_LINUX_CROSS_BUILD": "1",
    }:
        raise PolicyError("complete demo gate does not require the Linux cross build")
    if gate.scalars.get("run") != "./scripts/verify-demo-build.sh":
        raise PolicyError("complete demo gate does not execute the repository gate")

    if not dependency_index < cache_index < install_index < gate_index:
        raise PolicyError(
            "dependency, cache, cross-tool install, and demo-gate steps are misordered"
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
