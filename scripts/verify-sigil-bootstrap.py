#!/usr/bin/env python3
"""Verify the public Sigil bootstrap's pinned release trust contract."""

from __future__ import annotations

import argparse
import base64
import binascii
import pathlib
import re
import sys


REPOSITORY = "FelineStateMachine/goq"
UNCONFIGURED = "unconfigured"
ASSET_TARGET_CONTRACT = "linux-glibc2.17-x86_64"
MINISIGN_VERSION = "0.12"
MINISIGN_URL = (
    "https://github.com/jedisct1/minisign/releases/download/0.12/"
    "minisign-0.12-linux.tar.gz"
)
MINISIGN_SHA256 = (
    "9a599b48ba6eb7b1e80f12f36b94ceca7c00b7a5173c95c3efc88d9822957e73"
)
TAG_PATTERN = re.compile(
    r"^v[0-9]+\.[0-9]+\.[0-9]+(?:[.-][0-9A-Za-z][0-9A-Za-z.-]*)?$"
)
READONLY_ASSIGNMENT_PATTERN = re.compile(
    r'^readonly (?P<name>[a-z_][a-z0-9_]*)="(?P<value>[^"\n]*)"$', re.MULTILINE
)


class VerificationError(RuntimeError):
    """The bootstrap and repository trust pins are inconsistent."""


def require(condition: bool, message: str) -> None:
    if not condition:
        raise VerificationError(message)


def read_regular_file(path: pathlib.Path, description: str) -> str:
    require(path.is_file() and not path.is_symlink(), f"{description} is missing or unsafe")
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise VerificationError(f"{description} is unreadable: {error}") from error


def parse_bootstrap_pins(source: str) -> dict[str, str]:
    assignments: dict[str, list[str]] = {}
    for match in READONLY_ASSIGNMENT_PATTERN.finditer(source):
        assignments.setdefault(match.group("name"), []).append(match.group("value"))

    pins: dict[str, str] = {}
    for name in (
        "repository",
        "publisher_key",
        "release_tag",
        "asset_target_contract",
        "minisign_version",
        "minisign_url",
        "minisign_sha256",
    ):
        values = assignments.get(name, [])
        require(len(values) == 1, f"bootstrap must declare readonly {name} exactly once")
        pins[name] = values[0]
    require(
        pins["repository"] == REPOSITORY,
        "bootstrap repository pin is not the Goq release repository",
    )
    require(
        pins["asset_target_contract"] == ASSET_TARGET_CONTRACT,
        "bootstrap asset target contract is not the reviewed build ABI",
    )
    require(
        pins["minisign_version"] == MINISIGN_VERSION,
        "bootstrap Minisign version is not the reviewed verifier version",
    )
    require(
        pins["minisign_url"] == MINISIGN_URL,
        "bootstrap Minisign URL is not the reviewed verifier asset",
    )
    require(
        pins["minisign_sha256"] == MINISIGN_SHA256,
        "bootstrap Minisign checksum is not the reviewed verifier digest",
    )
    return pins


def parse_repository_public_key(source: str) -> str:
    stripped = source.strip()
    if stripped == UNCONFIGURED:
        return UNCONFIGURED
    lines = [line.strip() for line in source.splitlines() if line.strip()]
    require(bool(lines), "repository Sigil public key is unconfigured or malformed")
    key = lines[-1]
    require(
        all(line.startswith("untrusted comment:") for line in lines[:-1]),
        "repository Sigil public key contains unexpected content",
    )
    validate_public_key(key, "repository Sigil public key")
    return key


def validate_public_key(value: str, description: str) -> None:
    try:
        decoded = base64.b64decode(value, validate=True)
    except (binascii.Error, ValueError) as error:
        raise VerificationError(f"{description} is malformed") from error
    require(
        len(decoded) == 42,
        f"{description} is malformed: decoded Minisign key must be exactly 42 bytes",
    )
    require(
        decoded[:2] == b"Ed",
        f"{description} is malformed: unsupported Minisign signature algorithm",
    )
    require(
        base64.b64encode(decoded).decode("ascii") == value,
        f"{description} is malformed: public key encoding is not canonical",
    )


def verify_contract(bootstrap_source: str, public_key_source: str) -> str:
    pins = parse_bootstrap_pins(bootstrap_source)
    repository_key = parse_repository_public_key(public_key_source)
    embedded_key = pins["publisher_key"]
    release_tag = pins["release_tag"]

    bootstrap_closed = embedded_key == UNCONFIGURED and release_tag == UNCONFIGURED
    repository_closed = repository_key == UNCONFIGURED
    if bootstrap_closed and repository_closed:
        return "closed"

    require(
        embedded_key != UNCONFIGURED
        and release_tag != UNCONFIGURED
        and repository_key != UNCONFIGURED,
        "Sigil bootstrap trust pins are only partially configured",
    )
    validate_public_key(embedded_key, "bootstrap publisher key")
    require(
        embedded_key == repository_key,
        "bootstrap publisher key does not match release/sigil-minisign.pub",
    )
    require(
        TAG_PATTERN.fullmatch(release_tag) is not None,
        "bootstrap release tag is malformed",
    )
    return "open"


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Verify that install-sigil and the repository Minisign key agree."
    )
    parser.add_argument("--bootstrap", required=True, type=pathlib.Path)
    parser.add_argument("--public-key-file", required=True, type=pathlib.Path)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        state = verify_contract(
            read_regular_file(args.bootstrap, "Sigil bootstrap"),
            read_regular_file(args.public_key_file, "repository Sigil public key"),
        )
    except VerificationError as error:
        print(f"Sigil bootstrap verification failed: {error}", file=sys.stderr)
        return 1
    print("sigil_bootstrap_verification=ok")
    print(f"sigil_bootstrap_channel={state}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
