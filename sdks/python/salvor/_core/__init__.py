"""The sans-IO core: every rule the SDK holds, and no socket.

The SDK ships two transports over one control plane, a synchronous
:class:`salvor.Client` and an asynchronous :class:`salvor.AsyncClient`, and the
whole reason they cannot drift is that neither of them knows anything. A path, a
body key, a decode, the event tail's cursor arithmetic, the durable timer's
"a recorded event at this position is a replay" rule: all of it is here, as
plain data and plain functions over that data. A transport's job is to send a
described request and hand the bytes back.

- :mod:`~salvor._core.wire` is the contract: a described request, and the decode
  that raises the typed error.
- :mod:`~salvor._core.api` is the server-driven surface, one function per public
  client method.
- :mod:`~salvor._core.driver` is the client-driven run: its wire shapes, its
  result types, and the durable-timer arithmetic.
- :mod:`~salvor._core.sse` is the stream line protocol, the run event tail's
  cursor, and the model step's frame rules.

Nothing here imports httpx, which is what makes it testable without a server and
readable without one either.
"""

from . import api, driver, sse, wire
from .wire import Call, decode_json, request_kwargs

__all__ = ["api", "driver", "sse", "wire", "Call", "decode_json", "request_kwargs"]
