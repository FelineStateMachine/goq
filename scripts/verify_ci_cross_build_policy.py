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


def _mapping_key(line: str, indent: int) -> str | None:
    if _indent(line) != indent or not _is_content(line):
        return None
    match = re.fullmatch(rf" {{{indent}}}([A-Za-z0-9_-]+):(?:\s.*)?", line)
    return match.group(1) if match else None


def _job_lines(workflow: str, job_name: str) -> list[str]:
    lines = workflow.splitlines()
    jobs_indices = [
        index
        for index, line in enumerate(lines)
        if line == "jobs:"
    ]
    if len(jobs_indices) != 1:
        raise PolicyError("workflow must contain exactly one top-level jobs mapping")

    jobs_start = jobs_indices[0] + 1
    jobs_end = len(lines)
    for index in range(jobs_start, len(lines)):
        if _is_content(lines[index]) and _indent(lines[index]) == 0:
            jobs_end = index
            break

    matches = [
        index
        for index in range(jobs_start, jobs_end)
        if re.fullmatch(rf"  {re.escape(job_name)}:\s*(?:#.*)?", lines[index])
    ]
    if len(matches) != 1:
        raise PolicyError(f"jobs.{job_name} must exist exactly once")

    job_start = matches[0]
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
        match = re.fullmatch(r"        ([A-Za-z0-9_-]+):(?:\s*(.*))?", line)
        if not match:
            raise PolicyError(f"cannot parse field in step {step.name!r}: {line}")
        key = match.group(1)
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
                    r"          ([A-Za-z0-9_-]+):\s*(.+?)\s*", child
                )
                if not child_match:
                    raise PolicyError(
                        f"cannot parse {key!r} mapping in step {step.name!r}: {child}"
                    )
                child_key = child_match.group(1)
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
    if any(
        _mapping_key(line, 4) == "if"
        for line in job
    ):
        raise PolicyError("jobs.demo-gate must not be conditionally disabled")

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


def verify(workflow: str) -> None:
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
    args = parser.parse_args()
    try:
        verify(args.workflow.read_text(encoding="utf-8"))
    except (OSError, PolicyError) as error:
        print(f"CI cross-build policy failed: {error}", file=sys.stderr)
        return 1
    print("ci_cross_build_policy=ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
