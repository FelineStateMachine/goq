"""Decky RPC facade for the local Sigil appliance controller."""

import decky

from goq_sigil import Controller, ControllerError
from goq_sigil.runner import Runner


class Plugin:
    async def _main(self):
        self.controller = Controller(Runner(decky.DECKY_USER_HOME))
        decky.logger.info("Goq Sigil controller loaded")

    async def _unload(self):
        # systemd owns Sigil. A Decky reload must never stop the daemon.
        decky.logger.info("Goq Sigil controller unloaded")

    async def _call(self, operation, *arguments):
        try:
            value = await operation(*arguments)
            return {"ok": True, "value": value}
        except ControllerError as error:
            if error.detail:
                decky.logger.warning(
                    "Sigil controller operation failed: %s (%s)",
                    error.code,
                    error.detail,
                )
            else:
                decky.logger.warning(
                    "Sigil controller operation failed: %s", error.code
                )
            return {"ok": False, "error": error.code}
        except Exception:
            decky.logger.exception("Unexpected Sigil controller failure")
            return {"ok": False, "error": "internal_error"}

    async def get_snapshot(self):
        return await self._call(self.controller.get_snapshot)

    async def get_config(self):
        return await self._call(self.controller.get_config)

    async def validate_config(self, request):
        return await self._call(self.controller.validate_config, request)

    async def apply_config(self, request):
        return await self._call(self.controller.apply_config, request)

    async def restart_service(self):
        return await self._call(self.controller.restart_service)

    async def rollback_pending(self, transaction):
        return await self._call(self.controller.rollback_pending, transaction)

    async def reset_enrollment(self, expected_host_fingerprint):
        return await self._call(
            self.controller.reset_enrollment, expected_host_fingerprint
        )
