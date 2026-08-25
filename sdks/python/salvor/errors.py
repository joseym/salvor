"""Exceptions the SDK raises.

The control plane answers every failure with one JSON shape::

    {"error": {"code": "unknown_run", "message": "...", "details": {...}}}

`SalvorAPIError` carries that `code` and `message` so callers match on a
stable token rather than parsing a sentence. The refusals that carry
structured evidence get their own subclasses: a resume blocked because a write
was recorded but never completed is `NeedsReconciliationError`, exposing the
recorded write intent; a client-driven open blocked because another driver's
lease is still current is `LeaseHeldError`, exposing how long the hold has
left.
"""

from __future__ import annotations

import json
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


class DivergenceError(SalvorAPIError):
    """Raised when a client-driven append is not the one legal next event.

    The server re-folds the durable log on every append and refuses one that is
    not the legal continuation: a wrong sequence number, a completion that does
    not correlate to its intent, a second pending intent, an event after a
    terminal event, or different bytes at an already-recorded position. The
    append-guard's precise reason is on :attr:`~SalvorAPIError.message`.
    """


class LeaseHeldError(SalvorAPIError):
    """Raised when opening a client-driven run meets a lease another driver
    still holds.

    A lease is held until it lapses, not until a newer caller asks for it: this
    is the refusal a re-open meets when the run's current driver has presented
    its token within the last :attr:`lapses_in_seconds`. Presenting that
    driver's own ``drive_token`` on the open (see
    :meth:`salvor.client_runs.ClientRunDriver.open`) returns the recorded log
    under the same token instead of raising this; a caller that means to take
    the run over waits :attr:`lapses_in_seconds` and asks again, or waits for
    the run to finish, which drops the hold immediately.
    """

    @property
    def lapses_in_seconds(self) -> int:
        """The whole seconds until the held lease lapses on its own."""
        return int(self.details.get("lapses_in_seconds", 0))


class SalvorStreamError(SalvorError):
    """Raised when the event stream drops and cannot be resumed within the
    configured retry budget."""


def api_error(
    code: str,
    message: str,
    status: int,
    details: Optional[dict[str, Any]] = None,
) -> SalvorAPIError:
    """Build the typed error for one decoded error envelope.

    The one refusal that carries structured evidence, a resume or tool step
    blocked on a dangling write, becomes :class:`NeedsReconciliationError`; a
    client-driven append the log rejects becomes :class:`DivergenceError`;
    everything else is a plain :class:`SalvorAPIError` carrying the stable code.
    """
    if code == "needs_reconciliation":
        return NeedsReconciliationError(code, message, status, details)
    if code == "divergence":
        return DivergenceError(code, message, status, details)
    if code == "lease_held":
        return LeaseHeldError(code, message, status, details)
    return SalvorAPIError(code, message, status, details)


def decode_error(status: int, body: bytes) -> SalvorAPIError:
    """Decode an error-envelope response body into the typed error it names."""
    try:
        envelope = json.loads(body).get("error", {})
    except (ValueError, AttributeError):
        envelope = {}
    code = envelope.get("code", "unknown")
    message = envelope.get("message", body.decode("utf-8", "replace"))
    details = envelope.get("details")
    return api_error(code, message, status, details)
