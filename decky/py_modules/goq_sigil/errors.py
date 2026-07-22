"""Closed errors safe to return through the Decky RPC bridge."""


class ControllerError(Exception):
    """An expected appliance-controller failure with a non-secret code."""

    def __init__(self, code: str):
        super().__init__(code)
        self.code = code
