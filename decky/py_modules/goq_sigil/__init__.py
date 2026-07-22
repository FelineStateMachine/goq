"""Local Decky management adapter for the Goq Sigil appliance."""

from .controller import Controller
from .errors import ControllerError

__all__ = ["Controller", "ControllerError"]
