"""Transactional, local-only orchestration over Sigil's appliance CLI."""

from __future__ import annotations

import asyncio
import re
from pathlib import Path
from typing import Any

from .errors import ControllerError
from .runner import Runner


FINGERPRINT = re.compile(r"^[0-9a-f]{8}…[0-9a-f]{8}$")
TRANSACTION = re.compile(r"^[0-9a-f]{32}$")
READY_TIMEOUT_SECONDS = 30.0
POLL_SECONDS = 0.25


class Controller:
    def __init__(self, runner: Runner):
        self.runner = runner
        self._mutation_lock = asyncio.Lock()

    async def _status(self) -> dict[str, Any]:
        return await self.runner.sigil_json(
            [
                "appliance",
                "status",
                "--config",
                str(self.runner.config),
                "--json",
                "--schema-version",
                "2",
            ],
            schema_version=2,
        )

    async def _service(self) -> dict[str, str]:
        result = await self.runner.systemctl_command("show", check=False)
        values: dict[str, str] = {}
        for raw_line in result.stdout.decode("utf-8", "strict").splitlines():
            key, separator, value = raw_line.partition("=")
            if separator and key in {
                "LoadState",
                "ActiveState",
                "SubState",
                "UnitFileState",
            }:
                values[key] = value[:64]
        return {
            "installed": values.get("LoadState") == "loaded",
            "active": values.get("ActiveState") == "active",
            "active_state": values.get("ActiveState", "unknown"),
            "sub_state": values.get("SubState", "unknown"),
            "unit_file_state": values.get("UnitFileState", "unknown"),
        }

    async def get_snapshot(self) -> dict[str, Any]:
        service = await self._service()
        if not self.runner.sigil.is_file():
            return {
                "schema_version": 1,
                "installed": False,
                "compatible": False,
                "service": service,
                "appliance": None,
                "capabilities": {
                    "stream_diagnostics": False,
                    "factory_reset": False,
                },
            }
        try:
            appliance = await self._status()
            compatible = True
            error = None
        except ControllerError as failure:
            appliance = None
            compatible = failure.code != "incompatible_sigil"
            error = failure.code
        response: dict[str, Any] = {
            "schema_version": 1,
            "installed": True,
            "compatible": compatible,
            "service": service,
            "appliance": appliance,
            "capabilities": {
                "stream_diagnostics": False,
                "factory_reset": False,
            },
        }
        if error is not None:
            response["error"] = error
        return response

    async def get_config(self) -> dict[str, Any]:
        return await self.runner.sigil_json(
            [
                "appliance",
                "config",
                "show",
                "--config",
                str(self.runner.config),
                "--json",
            ]
        )

    async def validate_config(self, request: dict[str, Any]) -> dict[str, Any]:
        self._validate_config_request(request)
        return await self.runner.sigil_json(
            [
                "appliance",
                "config",
                "validate",
                "--config",
                str(self.runner.config),
                "--json",
            ],
            stdin_value=request,
        )

    async def restart_service(self) -> dict[str, Any]:
        async with self._mutation_lock:
            previous_instance = await self._current_instance()
            await self.runner.systemctl_command("restart")
            status = await self._wait_ready(previous_instance=previous_instance)
            return {"schema_version": 1, "operation": "restart", "status": status}

    async def apply_config(self, request: dict[str, Any]) -> dict[str, Any]:
        self._validate_config_request(request)
        async with self._mutation_lock:
            validation = await self.validate_config(request)
            if not validation.get("changed"):
                return {
                    "schema_version": 1,
                    "operation": "config_apply",
                    "changed": False,
                    "revision": validation.get("candidate_revision"),
                }

            was_active = (await self._service())["active"]
            baseline_instance = await self._current_instance()
            transaction: str | None = None
            candidate_instance: str | None = None
            candidate_revision: str | None = None
            commit_response_failure: BaseException | None = None
            try:
                await self._stop_and_wait()
                installed = await self.runner.sigil_json(
                    [
                        "appliance",
                        "config",
                        "set",
                        "--config",
                        str(self.runner.config),
                        "--json",
                    ],
                    stdin_value=request,
                )
                transaction = installed.get("transaction")
                if not isinstance(transaction, str) or not TRANSACTION.fullmatch(transaction):
                    raise ControllerError("invalid_response")
                candidate_revision = self._revision(installed.get("candidate_revision"))
                await self.runner.systemctl_command("start")
                ready = await self._wait_ready(
                    expected_revision=candidate_revision,
                    previous_instance=baseline_instance,
                )
                instance = ready.get("runtime", {}).get("instance_id")
                if not isinstance(instance, str) or not TRANSACTION.fullmatch(instance):
                    raise ControllerError("health_not_proven")
                candidate_instance = instance
                await self._stop_and_wait()
                try:
                    committed = await self.runner.sigil_json(
                        [
                            "appliance",
                            "config",
                            "commit",
                            "--config",
                            str(self.runner.config),
                            "--transaction",
                            transaction,
                            "--expected-instance",
                            instance,
                            "--json",
                        ]
                    )
                    committed_revision = self._revision(committed.get("revision"))
                    if committed_revision != candidate_revision:
                        raise ControllerError("invalid_response")
                except BaseException as failure:
                    committed_without_response = await asyncio.shield(
                        self._config_is_committed(candidate_revision)
                    )
                    if not committed_without_response:
                        raise
                    committed_revision = candidate_revision
                    commit_response_failure = failure
            except BaseException as failure:
                rollback_ok = await asyncio.shield(
                    self._recover_apply(
                        transaction,
                        was_active,
                        baseline_instance,
                        request["expected_revision"],
                    )
                )
                if isinstance(failure, asyncio.CancelledError):
                    raise
                if not rollback_ok:
                    raise ControllerError("rollback_failed") from failure
                raise ControllerError("apply_rolled_back") from failure

            if was_active:
                try:
                    await self._start_and_wait(candidate_instance, committed_revision)
                except BaseException as failure:
                    # The transaction is already committed. Never claim that it rolled back
                    # or stop the proven candidate after a UI/service-health failure.
                    try:
                        await asyncio.shield(self.runner.systemctl_command("start"))
                    except Exception:
                        pass
                    if isinstance(failure, asyncio.CancelledError):
                        raise
                    raise ControllerError("post_commit_service_unhealthy") from failure
            if isinstance(commit_response_failure, asyncio.CancelledError):
                raise commit_response_failure
            return {
                "schema_version": 1,
                "operation": "config_apply",
                "changed": True,
                "revision": committed_revision,
                "commit_response_recovered": commit_response_failure is not None,
            }

    async def rollback_pending(self, transaction: str) -> dict[str, Any]:
        if not TRANSACTION.fullmatch(transaction):
            raise ControllerError("invalid_request")
        async with self._mutation_lock:
            was_active = (await self._service())["active"]
            previous_instance = await self._current_instance()
            before = await self.get_config()
            pending = before.get("pending_transaction")
            if not isinstance(pending, dict) or pending.get("transaction") != transaction:
                raise ControllerError("transaction_not_found")
            base_revision = self._revision(pending.get("base_revision"))
            await self._stop_and_wait()
            try:
                result = await self.runner.sigil_json(
                    [
                        "appliance",
                        "config",
                        "rollback",
                        "--config",
                        str(self.runner.config),
                        "--transaction",
                        transaction,
                        "--json",
                    ]
                )
                restored_revision = self._revision(result.get("restored_revision"))
                if restored_revision != base_revision:
                    raise ControllerError("invalid_response")
            except BaseException as failure:
                completed = await asyncio.shield(
                    self._rollback_completed(transaction, base_revision)
                )
                if completed and was_active:
                    try:
                        await asyncio.shield(
                            self._start_and_wait(previous_instance, base_revision)
                        )
                    except Exception as recovery_error:
                        raise ControllerError("rollback_service_unhealthy") from recovery_error
                if isinstance(failure, asyncio.CancelledError):
                    raise
                if completed:
                    raise ControllerError("rollback_response_lost") from failure
                # The candidate may still be live in the config file. Staying stopped is
                # the only fail-closed outcome until the user retries explicit rollback.
                raise ControllerError("rollback_failed_safe") from failure
            if was_active:
                await self._start_and_wait(previous_instance, restored_revision)
            return result

    async def reset_enrollment(self, expected_host_fingerprint: str) -> dict[str, Any]:
        if not FINGERPRINT.fullmatch(expected_host_fingerprint):
            raise ControllerError("invalid_request")
        async with self._mutation_lock:
            was_active = (await self._service())["active"]
            previous_instance = await self._current_instance()
            failure: BaseException | None = None
            result: dict[str, Any] | None = None
            try:
                await self._stop_and_wait()
                result = await self.runner.sigil_json(
                    [
                        "appliance",
                        "enrollment-reset",
                        "--config",
                        str(self.runner.config),
                        "--expected-host-fingerprint",
                        expected_host_fingerprint,
                        "--json",
                    ]
                )
            except BaseException as error:
                failure = error
            restore_ok = True
            if was_active:
                try:
                    await asyncio.shield(
                        self._start_and_wait(previous_instance, None)
                    )
                except Exception:
                    restore_ok = False
            if failure is not None:
                if isinstance(failure, asyncio.CancelledError):
                    raise failure
                if not restore_ok:
                    raise ControllerError("reset_restore_failed") from failure
                raise failure
            if not restore_ok:
                raise ControllerError("reset_restore_failed")
            if result is None:
                raise ControllerError("invalid_response")
            return result

    async def _recover_apply(
        self,
        transaction: str | None,
        was_active: bool,
        previous_instance: str | None,
        base_revision: str,
    ) -> bool:
        try:
            await self._stop_and_wait()
            if transaction is None:
                shown = await self.get_config()
                pending = shown.get("pending_transaction")
                if isinstance(pending, dict):
                    candidate = pending.get("transaction")
                    if isinstance(candidate, str) and TRANSACTION.fullmatch(candidate):
                        transaction = candidate
            if transaction is not None:
                rolled_back = await self.runner.sigil_json(
                    [
                        "appliance",
                        "config",
                        "rollback",
                        "--config",
                        str(self.runner.config),
                        "--transaction",
                        transaction,
                        "--json",
                    ]
                )
                base_revision = self._revision(rolled_back.get("restored_revision"))
            if was_active:
                await self._start_and_wait(previous_instance, base_revision)
            return True
        except BaseException:
            return False

    async def _rollback_completed(self, transaction: str, base_revision: str) -> bool:
        try:
            shown = await self.get_config()
            return (
                shown.get("revision") == base_revision
                and shown.get("pending_transaction") is None
            )
        except ControllerError:
            return False

    async def _config_is_committed(self, candidate_revision: str) -> bool:
        try:
            shown = await self.get_config()
            return (
                shown.get("revision") == candidate_revision
                and shown.get("pending_transaction") is None
            )
        except ControllerError:
            return False

    async def _current_instance(self) -> str | None:
        try:
            instance = (await self._status()).get("runtime", {}).get("instance_id")
            if instance is None:
                # A successful status read with no instance proves there is no stale
                # daemon snapshot to confuse with the process about to start.
                return ""
            return (
                instance
                if isinstance(instance, str) and TRANSACTION.fullmatch(instance)
                else None
            )
        except ControllerError:
            return None

    async def _start_and_wait(
        self, previous_instance: str | None, expected_revision: str | None
    ) -> dict[str, Any]:
        await self.runner.systemctl_command("start")
        return await self._wait_ready(
            expected_revision=expected_revision,
            previous_instance=previous_instance,
        )

    async def _stop_and_wait(self) -> None:
        await self.runner.systemctl_command("stop")
        deadline = asyncio.get_running_loop().time() + READY_TIMEOUT_SECONDS
        while asyncio.get_running_loop().time() < deadline:
            if not (await self._service())["active"]:
                return
            await asyncio.sleep(POLL_SECONDS)
        raise ControllerError("service_stop_timeout")

    async def _wait_ready(
        self,
        *,
        expected_revision: Any = None,
        previous_instance: Any = None,
    ) -> dict[str, Any]:
        deadline = asyncio.get_running_loop().time() + READY_TIMEOUT_SECONDS
        # Remember why the last poll fell short so a timeout reports the root
        # cause instead of swallowing it: a status call that itself failed
        # (last_error) or a status that came back but never proved ready
        # (last_gap).
        last_error: ControllerError | None = None
        last_gap: str | None = None
        while asyncio.get_running_loop().time() < deadline:
            try:
                status = await self._status()
                runtime = status.get("runtime", {})
                instance = runtime.get("instance_id")
                revision_ok = (
                    expected_revision is None
                    or runtime.get("loaded_config_revision") == expected_revision
                )
                instance_ok = (
                    isinstance(previous_instance, str) and instance != previous_instance
                )
                if (
                    status.get("overall") in {"ready", "active"}
                    and runtime.get("state") == "fresh"
                    and runtime.get("daemon") == "ready"
                    and isinstance(instance, str)
                    and revision_ok
                    and instance_ok
                ):
                    return status
                last_error = None
                last_gap = self._describe_readiness_gap(
                    status, runtime, expected_revision, previous_instance
                )
            except ControllerError as error:
                last_error = error
                last_gap = None
            await asyncio.sleep(POLL_SECONDS)
        detail = last_error.code if last_error is not None else last_gap
        raise ControllerError("service_ready_timeout", detail) from last_error

    @staticmethod
    def _describe_readiness_gap(
        status: dict[str, Any],
        runtime: dict[str, Any],
        expected_revision: Any,
        previous_instance: Any,
    ) -> str:
        """Name the first unmet readiness condition, for timeout diagnostics."""
        overall = status.get("overall")
        if overall not in {"ready", "active"}:
            return f"overall={overall!r}"
        if runtime.get("state") != "fresh":
            return f"runtime.state={runtime.get('state')!r}"
        if runtime.get("daemon") != "ready":
            return f"runtime.daemon={runtime.get('daemon')!r}"
        instance = runtime.get("instance_id")
        if not isinstance(instance, str):
            return "runtime.instance_id missing"
        if (
            expected_revision is not None
            and runtime.get("loaded_config_revision") != expected_revision
        ):
            return (
                f"loaded_config_revision={runtime.get('loaded_config_revision')!r} "
                f"!= expected {expected_revision!r}"
            )
        if isinstance(previous_instance, str) and instance == previous_instance:
            return "runtime.instance_id unchanged from previous"
        return "readiness conditions not met"

    @staticmethod
    def _revision(value: Any) -> str:
        if not isinstance(value, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", value):
            raise ControllerError("invalid_response")
        return value

    @staticmethod
    def _validate_config_request(request: dict[str, Any]) -> None:
        if not isinstance(request, dict) or set(request) != {
            "schema_version",
            "expected_revision",
            "settings",
        }:
            raise ControllerError("invalid_request")
        if request["schema_version"] != 1:
            raise ControllerError("invalid_request")
        revision = request["expected_revision"]
        if not isinstance(revision, str) or not re.fullmatch(
            r"sha256:[0-9a-f]{64}", revision
        ):
            raise ControllerError("invalid_request")
        settings = request["settings"]
        if not isinstance(settings, dict) or set(settings) != {
            "resolution",
            "framerate",
            "rate_control",
        }:
            raise ControllerError("invalid_request")
        framerate = settings["framerate"]
        if (
            not isinstance(framerate, int)
            or isinstance(framerate, bool)
            or not 1 <= framerate <= 240
        ):
            raise ControllerError("invalid_request")
        resolution = settings["resolution"]
        if not isinstance(resolution, dict) or resolution.get("mode") not in {
            "native",
            "fixed",
        }:
            raise ControllerError("invalid_request")
        if resolution["mode"] == "native":
            if set(resolution) != {"mode"}:
                raise ControllerError("invalid_request")
        else:
            if set(resolution) != {"mode", "width", "height"}:
                raise ControllerError("invalid_request")
            for field in ("width", "height"):
                value = resolution[field]
                if (
                    not isinstance(value, int)
                    or isinstance(value, bool)
                    or value < 64
                    or value % 2
                ):
                    raise ControllerError("invalid_request")
            if resolution["width"] > 7680 or resolution["height"] > 4320:
                raise ControllerError("invalid_request")
        rate_control = settings["rate_control"]
        if rate_control is None:
            return
        if not isinstance(rate_control, dict):
            raise ControllerError("invalid_request")
        mode = rate_control.get("mode")
        if mode == "cbr":
            if set(rate_control) != {"mode", "bitrate_kbps"}:
                raise ControllerError("invalid_request")
            value = rate_control["bitrate_kbps"]
            if not isinstance(value, int) or isinstance(value, bool) or not 1000 <= value <= 100000:
                raise ControllerError("invalid_request")
        elif mode == "cqp":
            if set(rate_control) != {"mode", "quantizer"}:
                raise ControllerError("invalid_request")
            value = rate_control["quantizer"]
            if not isinstance(value, int) or isinstance(value, bool) or not 1 <= value <= 51:
                raise ControllerError("invalid_request")
        else:
            raise ControllerError("invalid_request")
