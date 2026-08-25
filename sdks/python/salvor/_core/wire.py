"""The sans-IO request/response contract every transport speaks.

A :class:`Call` says everything about one control-plane request that does not
depend on how it is sent: the method, the path, what goes in the body, and the
function that turns the decoded JSON into the typed result. Sending it is the
only thing a transport adds, which is why the synchronous and asynchronous
clients are the same shape with ``await`` in front.

Nothing in this package imports httpx. That is the point: the rules live here,
where they can be read and tested without a socket, and each transport is a
handful of lines that move bytes.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any, Callable, Optional

from ..errors import SalvorAPIError, decode_error


class _NoBody:
    """The absence of a request body, which is not the same as a body that IS
    ``null``: ``start_run`` sends ``{"input": null}`` on purpose, and a GET
    sends nothing at all."""

    def __repr__(self) -> str:  # pragma: no cover - debugging aid
        return "<no body>"


NO_BODY = _NoBody()


@dataclass
class Call:
    """One control-plane request, described rather than performed.

    ``parse`` receives the decoded JSON object of a 2xx response and returns
    whatever the public method promises. A non-2xx response never reaches it:
    :func:`decode_json` raises the typed error first.
    """

    method: str
    path: str
    parse: Callable[[dict[str, Any]], Any]
    params: Optional[dict[str, Any]] = None
    headers: Optional[dict[str, str]] = None
    json_body: Any = NO_BODY
    content: Optional[bytes] = None


def request_kwargs(call: Call) -> dict[str, Any]:
    """The keyword arguments for one httpx request call.

    Absent parts are omitted rather than passed as ``None``, so a call that
    carries no body sends no body and a call whose body IS ``null`` sends
    ``null``.
    """
    kwargs: dict[str, Any] = {}
    if call.params is not None:
        kwargs["params"] = call.params
    if call.headers is not None:
        kwargs["headers"] = call.headers
    if call.json_body is not NO_BODY:
        kwargs["json"] = call.json_body
    if call.content is not None:
        kwargs["content"] = call.content
    return kwargs


def decode_json(status: int, body: bytes) -> dict[str, Any]:
    """Decode one response body, raising the typed error a non-2xx names.

    An empty 2xx body decodes to an empty object, which is what the endpoints
    that answer ``204`` and the ones that answer ``{}`` both mean.
    """
    if status // 100 != 2:
        raise decode_error(status, body)
    if not body:
        return {}
    return json.loads(body)


def error(status: int, body: bytes) -> SalvorAPIError:
    """The typed error for one error-envelope body."""
    return decode_error(status, body)


def identity(obj: dict[str, Any]) -> dict[str, Any]:
    """The parse for an endpoint whose decoded body IS the result."""
    return obj


def discard(obj: dict[str, Any]) -> None:
    """The parse for an endpoint whose body carries a receipt nobody reads:
    the call either succeeded or raised on the way here."""
    return None
