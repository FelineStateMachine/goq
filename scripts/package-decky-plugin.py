#!/usr/bin/env python3
"""Build a deterministic, provenance-bound Decky plugin archive."""

from __future__ import annotations

import hashlib
import json
import os
import pathlib
import stat
import subprocess
import sys
import zipfile


REPOSITORY = "https://github.com/FelineStateMachine/goq"
PLUGIN_DIRECTORY = "goq-sigil"
FIXED_FILES = (
    "LICENSE",
    "README.md",
    "compatibility.json",
    "dist/index.js",
    "main.py",
    "package.json",
    "plugin.json",
)


def fail(message: str) -> "NoReturn":
    raise SystemExit(message)


def read_json(path: pathlib.Path) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(f"invalid JSON metadata: {path.name}: {error}")
    if not isinstance(value, dict):
        fail(f"JSON metadata must be an object: {path.name}")
    return value


def source_commit(repository: pathlib.Path) -> str:
    override = os.environ.get("GOQ_SOURCE_COMMIT")
    if override is not None:
        commit = override
    else:
        try:
            commit = subprocess.run(
                ["git", "rev-parse", "HEAD"],
                cwd=repository,
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
                timeout=5,
            ).stdout.strip()
        except (OSError, subprocess.SubprocessError) as error:
            fail(f"unable to resolve source commit: {error}")
    if len(commit) != 40 or any(character not in "0123456789abcdef" for character in commit):
        fail("source commit must be 40 lowercase hexadecimal characters")
    return commit


def collect_files(plugin: pathlib.Path) -> list[pathlib.Path]:
    files = [plugin / relative for relative in FIXED_FILES]
    module_root = plugin / "py_modules/goq_sigil"
    files.extend(sorted(module_root.glob("*.py")))
    for path in files:
        try:
            metadata = path.lstat()
        except OSError as error:
            fail(f"required plugin file is missing: {path.relative_to(plugin)}: {error}")
        if not stat.S_ISREG(metadata.st_mode) or path.is_symlink():
            fail(f"plugin input must be a regular non-symlink: {path.relative_to(plugin)}")
    return files


def main() -> int:
    repository = pathlib.Path(__file__).resolve().parent.parent
    plugin = repository / "decky"
    package = read_json(plugin / "package.json")
    compatibility = read_json(plugin / "compatibility.json")
    version = package.get("version")
    if not isinstance(version, str) or compatibility.get("plugin_version") != version:
        fail("package and compatibility plugin versions must match")
    if package.get("name") != PLUGIN_DIRECTORY:
        fail("unexpected Decky package name")

    output_root = pathlib.Path(sys.argv[1]) if len(sys.argv) == 2 else repository / "artifacts"
    if len(sys.argv) > 2:
        fail("usage: package-decky-plugin.py [output-directory]")
    output_root.mkdir(parents=True, exist_ok=True)
    output = output_root / f"{PLUGIN_DIRECTORY}-v{version}.zip"
    commit = source_commit(repository)
    files = collect_files(plugin)
    provenance = json.dumps(
        {
            "schema_version": 1,
            "repository": REPOSITORY,
            "source_commit": commit,
            "plugin_version": version,
            "compatibility": compatibility["sigil"],
        },
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8") + b"\n"

    entries: list[tuple[str, bytes]] = []
    for path in files:
        relative = path.relative_to(plugin).as_posix()
        entries.append((f"{PLUGIN_DIRECTORY}/{relative}", path.read_bytes()))
    entries.append((f"{PLUGIN_DIRECTORY}/provenance.json", provenance))
    entries.sort(key=lambda entry: entry[0])

    temporary = output.with_suffix(".zip.tmp")
    try:
        with zipfile.ZipFile(
            temporary, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9
        ) as archive:
            for name, contents in entries:
                info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
                info.compress_type = zipfile.ZIP_DEFLATED
                info.create_system = 3
                info.external_attr = 0o100644 << 16
                archive.writestr(info, contents)
        os.replace(temporary, output)
    finally:
        temporary.unlink(missing_ok=True)

    digest = hashlib.sha256(output.read_bytes()).hexdigest()
    print(f"archive={output}")
    print(f"sha256={digest}")
    print(f"source_commit={commit}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
