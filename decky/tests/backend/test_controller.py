from __future__ import annotations

import asyncio
import tempfile
import unittest
from pathlib import Path
from typing import Any

from goq_sigil.controller import Controller
from goq_sigil.errors import ControllerError
from goq_sigil.runner import CommandResult, strict_json


REVISION_A = "sha256:" + "a" * 64
REVISION_B = "sha256:" + "b" * 64
TRANSACTION = "1" * 32
FINGERPRINT = "12345678…90abcdef"


def request() -> dict[str, Any]:
    return {
        "schema_version": 1,
        "expected_revision": REVISION_A,
        "settings": {
            "resolution": {"mode": "native"},
            "framerate": 60,
            "rate_control": {"mode": "cbr", "bitrate_kbps": 12000},
        },
    }


class FakeRunner:
    def __init__(self, root: Path):
        self.home = root
        self.sigil = root / "sigil"
        self.sigil.write_text("fixture", encoding="utf-8")
        self.config = root / "host.toml"
        self.systemctl = Path("/usr/bin/systemctl")
        self.unit = "sigil-host.service"
        self.active = True
        self.revision = REVISION_A
        self.instance = "2" * 32
        self.next_instance = 2
        self.calls: list[tuple[str, Any]] = []
        self.fail_ready = False

    async def systemctl_command(
        self, action: str, *, check: bool = True, timeout: float = 15.0
    ) -> CommandResult:
        self.calls.append(("systemctl", action))
        if action == "show":
            state = "active" if self.active else "inactive"
            output = (
                "LoadState=loaded\n"
                f"ActiveState={state}\n"
                f"SubState={'running' if self.active else 'dead'}\n"
                "UnitFileState=enabled\n"
            ).encode()
            return CommandResult(0, output, b"")
        if action in {"start", "restart"}:
            self.active = True
            self.next_instance += 1
            self.instance = f"{self.next_instance:032x}"
        elif action == "stop":
            self.active = False
        return CommandResult(0, b"", b"")

    async def sigil_json(
        self,
        arguments: list[str],
        *,
        stdin_value: dict[str, Any] | None = None,
        schema_version: int = 1,
        timeout: float = 10.0,
    ) -> dict[str, Any]:
        operation = tuple(arguments[:3])
        self.calls.append(("sigil", operation))
        if operation == ("appliance", "status", "--config"):
            ready = self.active and not (
                self.fail_ready and self.revision == REVISION_B
            )
            return {
                "schema_version": 2,
                "sigil_version": "0.1.0",
                "overall": "ready" if ready else "degraded",
                "identity": {"host_fingerprint": FINGERPRINT},
                "enrollment": {"state": "none", "grants": [], "epoch": 0},
                "runtime": {
                    "state": "fresh",
                    "daemon": "ready" if ready else "degraded",
                    "session": "inactive",
                    "instance_id": self.instance,
                    "loaded_config_revision": self.revision,
                },
                "config": {"revision": self.revision, "pending_transaction": None},
            }
        if operation == ("appliance", "config", "show"):
            return {
                "schema_version": 1,
                "revision": self.revision,
                "settings": request()["settings"],
                "pending_transaction": None,
            }
        if operation == ("appliance", "config", "validate"):
            return {
                "schema_version": 1,
                "base_revision": REVISION_A,
                "candidate_revision": REVISION_B,
                "changed": True,
                "settings": stdin_value["settings"],
            }
        if operation == ("appliance", "config", "set"):
            self.revision = REVISION_B
            return {
                "schema_version": 1,
                "transaction": TRANSACTION,
                "candidate_revision": REVISION_B,
                "changed": True,
                "restart_required": True,
            }
        if operation == ("appliance", "config", "commit"):
            return {
                "schema_version": 1,
                "operation": "config_commit",
                "transaction": TRANSACTION,
                "revision": REVISION_B,
            }
        if operation == ("appliance", "config", "rollback"):
            self.revision = REVISION_A
            return {
                "schema_version": 1,
                "operation": "config_rollback",
                "transaction": TRANSACTION,
                "restored_revision": REVISION_A,
            }
        if operation == ("appliance", "enrollment-reset", "--config"):
            return {
                "schema_version": 1,
                "operation": "enrollment_reset",
                "host_fingerprint": FINGERPRINT,
                "had_enrollment": True,
                "previous_epoch": 4,
                "current_epoch": 5,
                "invitations_invalidated": True,
            }
        raise AssertionError(arguments)


class FastController(Controller):
    async def _wait_ready(self, **kwargs):
        if self.runner.fail_ready and self.runner.revision == REVISION_B:
            raise ControllerError("service_ready_timeout")
        return await super()._wait_ready(**kwargs)


class ControllerTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.runner = FakeRunner(Path(self.temporary.name))
        self.controller = FastController(self.runner)

    def tearDown(self):
        self.temporary.cleanup()

    async def test_snapshot_combines_fixed_service_and_redacted_appliance_contract(self):
        snapshot = await self.controller.get_snapshot()
        self.assertTrue(snapshot["installed"])
        self.assertTrue(snapshot["service"]["active"])
        self.assertEqual(
            snapshot["appliance"]["identity"]["host_fingerprint"], FINGERPRINT
        )
        self.assertFalse(snapshot["capabilities"]["factory_reset"])

    async def test_apply_uses_health_bound_commit_and_restores_active_service(self):
        result = await self.controller.apply_config(request())
        self.assertTrue(result["changed"])
        self.assertEqual(result["revision"], REVISION_B)
        significant = [
            value
            for kind, value in self.runner.calls
            if kind == "systemctl" and value != "show"
        ]
        self.assertEqual(significant, ["stop", "start", "stop", "start"])
        sigil_operations = [
            value for kind, value in self.runner.calls if kind == "sigil"
        ]
        self.assertIn(("appliance", "config", "validate"), sigil_operations)
        self.assertIn(("appliance", "config", "set"), sigil_operations)
        self.assertIn(("appliance", "config", "commit"), sigil_operations)
        self.assertNotIn(("appliance", "config", "rollback"), sigil_operations)

    async def test_candidate_health_failure_rolls_back_and_restores_service(self):
        self.runner.fail_ready = True
        with self.assertRaisesRegex(ControllerError, "apply_rolled_back"):
            await self.controller.apply_config(request())
        self.assertEqual(self.runner.revision, REVISION_A)
        self.assertTrue(self.runner.active)
        self.assertIn(
            ("sigil", ("appliance", "config", "rollback")), self.runner.calls
        )

    async def test_reset_stops_before_mutation_and_restores_prior_state(self):
        result = await self.controller.reset_enrollment(FINGERPRINT)
        self.assertEqual(result["current_epoch"], 5)
        stop = self.runner.calls.index(("systemctl", "stop"))
        reset = self.runner.calls.index(
            ("sigil", ("appliance", "enrollment-reset", "--config"))
        )
        start = self.runner.calls.index(("systemctl", "start"))
        self.assertLess(stop, reset)
        self.assertLess(reset, start)

    async def test_mutations_are_serialized(self):
        entered = asyncio.Event()
        release = asyncio.Event()

        async def hold_restart():
            async with self.controller._mutation_lock:
                entered.set()
                await release.wait()

        first = asyncio.create_task(hold_restart())
        await entered.wait()
        second = asyncio.create_task(self.controller.reset_enrollment(FINGERPRINT))
        await asyncio.sleep(0)
        self.assertFalse(second.done())
        release.set()
        await first
        await second

    async def test_invalid_frontend_shapes_never_reach_sigil(self):
        invalid = request()
        invalid["settings"]["resolution"] = {
            "mode": "fixed",
            "width": 1279,
            "height": 800,
        }
        with self.assertRaisesRegex(ControllerError, "invalid_request"):
            await self.controller.validate_config(invalid)


class JsonTests(unittest.TestCase):
    def test_duplicate_keys_are_rejected(self):
        with self.assertRaisesRegex(ControllerError, "invalid_response"):
            strict_json(b'{"schema_version":1,"schema_version":1}', 1)

    def test_wrong_schema_is_rejected(self):
        with self.assertRaisesRegex(ControllerError, "incompatible_sigil"):
            strict_json(b'{"schema_version":2}', 1)


if __name__ == "__main__":
    unittest.main()
