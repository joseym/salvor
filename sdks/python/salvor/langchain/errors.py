"""The errors this middleware raises on its own account."""

from __future__ import annotations

from ..errors import SalvorError

__all__ = ["SalvorMiddlewareError"]


class SalvorMiddlewareError(SalvorError):
    """Something the middleware itself refuses, as opposed to something the
    control plane refused (which stays a :class:`~salvor.errors.SalvorAPIError`).

    Every message names the thread or the tool it is about and what would fix
    it, because these all surface inside somebody else's agent loop, far from
    this file.
    """
