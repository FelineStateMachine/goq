"""Bounded subprocess execution for the fixed Sigil and systemd interfaces."""

from __future__ import annotations

import asyncio
import json
import os
import stat
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Sequence

from .errors import ControllerError


MAX_OUTPUT_BYTES = 128 * 1024
MAX_REQUEST_BYTES = 16 * 1024


@dataclass(frozen=True)
class CommandResult:
    returncode: int
    stdout: bytes
    stderr: bytes


def _strict_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON key")
        result[key] = value
    return result


def strict_json(raw: bytes, schema_version: int) -> dict[str, Any]:
    if not raw or len(raw) > MAX_OUTPUT_BYTES:
        raise ControllerError("invalid_response")
    try:
        value = json.loads(raw, object_pairs_hook=_strict_object)
    except (UnicodeDecodeError, ValueError, json.JSONDecodeError) as error:
        raise ControllerError("invalid_response") from error
    if not isinstance(value, dict) or value.get("schema_version") != schema_version:
        raise ControllerError("incompatible_sigil")
    return value


class Runner:
    """Runs only absolute, caller-assembled argv without invoking a shell."""

    def __init__(self, user_home: str | Path, *, uid: int | None = None):
        self.home = Path(user_home)
        self.uid = os.geteuid() if uid is None else uid
        self.sigil = self.home / ".local/libexec/sigil-spark/current/sigil"
        self.config = self.home / ".config/sigil-spark/host.toml"
        self.unit = "sigil-host.service"
        self.systemctl = Path("/usr/bin/systemctl")

    def environment(self) -> dict[str, str]:
        if self.uid == 0:
            raise ControllerError("unsafe_user")
        runtime = Path(f"/run/user/{self.uid}")
        bus = runtime / "bus"
        try:
            runtime_stat = runtime.stat()
            bus_stat = bus.stat()
        except OSError as error:
            raise ControllerError("user_session_unavailable") from error
        if (
            not stat.S_ISDIR(runtime_stat.st_mode)
            or runtime_stat.st_uid != self.uid
            or runtime_stat.st_mode & 0o077
            or not stat.S_ISSOCK(bus_stat.st_mode)
            or bus_stat.st_uid != self.uid
        ):
            raise ControllerError("unsafe_user_session")
        return {
            "HOME": str(self.home),
            "XDG_RUNTIME_DIR": str(runtime),
            "DBUS_SESSION_BUS_ADDRESS": f"unix:path={bus}",
            "LANG": "C.UTF-8",
        }

    async def run(
        self,
        argv: Sequence[str | Path],
        *,
        stdin: bytes | None = None,
        timeout: float = 10.0,
        check: bool = True,
    ) -> CommandResult:
        if not argv or not Path(argv[0]).is_absolute():
            raise ControllerError("unsafe_command")
        if stdin is not None and len(stdin) > MAX_REQUEST_BYTES:
            raise ControllerError("invalid_request")
        process = await asyncio.create_subprocess_exec(
            *(str(value) for value in argv),
            stdin=asyncio.subprocess.PIPE if stdin is not None else asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=self.environment(),
        )

        async def stop_child() -> None:
            if process.returncode is not None:
                return
            process.terminate()
            try:
                await asyncio.wait_for(process.wait(), timeout=1.0)
            except TimeoutError:
                process.kill()
                await process.wait()

        async def read_bounded(stream: asyncio.StreamReader | None) -> bytes:
            if stream is None:
                return b""
            chunks: list[bytes] = []
            total = 0
            while True:
                chunk = await stream.read(min(4096, MAX_OUTPUT_BYTES + 1 - total))
                if not chunk:
                    return b"".join(chunks)
                chunks.append(chunk)
                total += len(chunk)
                if total > MAX_OUTPUT_BYTES:
                    raise ControllerError("output_limit")

        try:
            if stdin is not None and process.stdin is not None:
                process.stdin.write(stdin)
                await process.stdin.drain()
                process.stdin.close()
            stdout, stderr, _ = await asyncio.wait_for(
                asyncio.gather(
                    read_bounded(process.stdout),
                    read_bounded(process.stderr),
                    process.wait(),
                ),
                timeout=timeout,
            )
        except (TimeoutError, ControllerError) as error:
            await stop_child()
            if isinstance(error, ControllerError):
                raise
            raise ControllerError("command_timeout") from error
        except BaseException:
            await stop_child()
            raise

        result = CommandResult(process.returncode or 0, stdout, stderr)
        if check and result.returncode != 0:
            raise ControllerError("command_failed")
        return result

    async def sigil_json(
        self,
        arguments: Sequence[str],
        *,
        stdin_value: dict[str, Any] | None = None,
        schema_version: int = 1,
        timeout: float = 10.0,
    ) -> dict[str, Any]:
        if not self.sigil.is_file() or not os.access(self.sigil, os.X_OK):
            raise ControllerError("sigil_not_installed")
        stdin = None
        if stdin_value is not None:
            try:
                stdin = json.dumps(
                    stdin_value, separators=(",", ":"), ensure_ascii=True
                ).encode("ascii")
            except (TypeError, ValueError) as error:
                raise ControllerError("invalid_request") from error
        result = await self.run(
            [self.sigil, *arguments], stdin=stdin, timeout=timeout
        )
        return strict_json(result.stdout, schema_version)

    async def systemctl_command(
        self, action: str, *, check: bool = True, timeout: float = 15.0
    ) -> CommandResult:
        if action not in {"start", "stop", "restart", "is-active", "show"}:
            raise ControllerError("unsafe_command")
        arguments = [self.systemctl, "--user", action, self.unit]
        if action == "show":
            arguments.extend(
                [
                    "--property=LoadState",
                    "--property=ActiveState",
                    "--property=SubState",
                    "--property=UnitFileState",
                    "--no-pager",
                ]
            )
        return await self.run(arguments, timeout=timeout, check=check)
