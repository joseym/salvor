"""Exceptions the SDK raises.

The control plane answers every failure with one JSON shape::

    {"error": {"code": "unknown_run", "message": "...", "details": {...}}}

`SalvorAPIError` carries that `code` and `message` so callers match on a
stable token rather than parsing a sentence. The one refusal that carries
structured evidence, a resume blocked because a write was recorded but never
completed, gets its own subclass, `NeedsReconciliationError`, which exposes the
recorded write intent.
"""

from __future__ import annotations

from typing import Any, Optional


class SalvorError(Exception):
    """Base class for every error this SDK raises."""


class SalvorAPIError(SalvorError):
    """An error returned by the control plane, decoded from the error envelope.

    Attributes:
        code: The stable machine token (for example ``"unknown_run"``).
        message: The human sentence the server sent.
        status: The HTTP status code.
        details: The optional structured evidence, present today only on a
            reconciliation refusal.
    """

    def __init__(
        self,
        code: str,
        message: str,
        status: int,
        details: Optional[dict[str, Any]] = None,
    ) -> None:
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message
        self.status = status
        self.details = details or {}


class NeedsReconciliationError(SalvorAPIError):
    """Raised when a resume is refused because the run's log ends at a write
    intent with no recorded completion.

    The recorded write is on ``intent``: its ``tool``, ``input``, ``effect``,
    ``seq``, and ``recorded_at``. A human verifies externally what that write
    did, then calls :meth:`salvor.Client.resolve` with the output it produced.
    """

    def __init__(
        self,
        code: str,
        message: str,
        status: int,
        details: Optional[dict[str, Any]] = None,
    ) -> None:
        super().__init__(code, message, status, details)

    @property
    def intent(self) -> dict[str, Any]:
        """The recorded write intent that must be reconciled."""
        return self.details.get("intent", {})


class SalvorStreamError(SalvorError):
    """Raised when the event stream drops and cannot be resumed within the
    configured retry budget."""
