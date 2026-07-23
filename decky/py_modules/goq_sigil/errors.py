"""Closed errors safe to return through the Decky RPC bridge."""

from __future__ import annotations


class ControllerError(Exception):
    """An expected appliance-controller failure with a non-secret code."""

    def __init__(self, code: str, detail: str | None = None):
        super().__init__(code)
        self.code = code
        # A non-secret diagnostic describing *why* this failure occurred, when
        # the code alone would hide the root cause (e.g. a readiness timeout
        # that swallowed the last polling error). Never returned across the RPC
        # bridge — only logged host-side — so it stays independent of the
        # frontend-facing `code` contract.
        self.detail = detail
