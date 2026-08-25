"""Turning LangChain messages into what a salvor log holds, and back.

Three conversions live here, all of them shared between the middleware and
:func:`~salvor.langchain.finish_thread`, and all of them chosen so that what
goes into the log is LangChain's own storage form rather than something this
package invented. A recorded answer read back a year from now is read back by
``messages_from_dict``, the same function that reads back a checkpoint.
"""

from __future__ import annotations

import json
from typing import Any, Dict, Optional, TypeVar

from langchain_core.messages import (
    AIMessage,
    ToolMessage,
    message_to_dict,
    messages_from_dict,
)

from ..models import Usage
from .errors import SalvorMiddlewareError
from .request import plain

__all__ = [
    "as_tool_content",
    "mark",
    "stored_ai_message",
    "stored_form",
    "tool_output",
    "usage_of",
]

MessageT = TypeVar("MessageT")


def stored_form(message: Any) -> Dict[str, Any]:
    """What goes into the log for a model answer: LangChain's stored-message
    form, ``{"type": "ai", "data": {...}}``.

    The Python twin of the TypeScript middleware's ``AIMessage.toDict()``, and
    the shape :func:`stored_ai_message` reads back.
    """
    return message_to_dict(message)


def stored_ai_message(stored: Any, run: Optional[str] = None) -> AIMessage:
    """The recorded response, back as the message LangChain returned.

    What goes into the log is the stored form, so the answer comes back with
    its content, its tool calls, its ids and its usage intact.
    """
    if (
        not isinstance(stored, dict)
        or stored.get("type") != "ai"
        or not isinstance(stored.get("data"), dict)
    ):
        raise SalvorMiddlewareError(
            "run {run} recorded a model response this middleware cannot read "
            'back. It expects a LangChain stored message (`{{"type": "ai", '
            '"data": {{...}}}}`), which is what it writes; a run driven by '
            "other code records other shapes.".format(run=run or "?")
        )
    message = messages_from_dict([stored])[0]
    if not isinstance(message, AIMessage):  # pragma: no cover - guarded above
        raise SalvorMiddlewareError(
            "run {run} recorded a {kind} message where a model answer belongs.".format(
                run=run or "?", kind=getattr(message, "type", "?")
            )
        )
    return message


def mark(message: MessageT, seq: int, run: str) -> MessageT:
    """Put the replay marker on a message.

    It goes on ``response_metadata``, which is the one place a message carries
    provenance rather than content, and it is deliberately excluded from the
    request hash so that a replayed message fed back into the next model call
    hashes exactly as the live one did.

    A replayed answer arrives whole. Under streaming that means one message
    event with the full content, not a re-tokenised imitation of the original
    stream: the tokens happened once, and nothing here pretends otherwise.
    """
    metadata = dict(getattr(message, "response_metadata", None) or {})
    metadata["salvor"] = {"replayed": True, "seq": seq, "run": run}
    message.response_metadata = metadata  # type: ignore[attr-defined]
    return message


def usage_of(message: Any) -> Usage:
    """The token counts a run's budgets are held to, from wherever the model
    put them."""
    metadata = getattr(message, "usage_metadata", None)
    if metadata:
        return Usage(
            input_tokens=_count(metadata, "input_tokens"),
            output_tokens=_count(metadata, "output_tokens"),
        )
    response_metadata = getattr(message, "response_metadata", None) or {}
    reported = response_metadata.get("token_usage") or response_metadata.get("usage")
    if isinstance(reported, dict):
        return Usage(
            input_tokens=_count(reported, "input_tokens", "prompt_tokens"),
            output_tokens=_count(reported, "output_tokens", "completion_tokens"),
        )
    return Usage(input_tokens=0, output_tokens=0)


def _count(source: Any, *names: str) -> int:
    for name in names:
        value = source.get(name) if isinstance(source, dict) else getattr(source, name, None)
        if value is not None:
            try:
                return int(value)
            except (TypeError, ValueError):
                return 0
    return 0


def tool_output(message: ToolMessage) -> Any:
    """What a tool call returned, as the value the operator's ``output_schema``
    describes.

    LangChain turns a tool's result into a tool message by stringifying it, so
    the result is recovered by parsing the content back when the parse round
    trips exactly. When it does not, the content is recorded as the string it
    is: better a completion the operator's schema refuses, and says so, than a
    silently reshaped result that replays as different bytes than the live call
    produced.

    "Exactly" is measured against :func:`as_tool_content`, which is the call
    LangChain Python itself made to build the content
    (``langchain_core.tools.base._stringify``). The TypeScript middleware
    measures it against ``JSON.stringify``, and the two spell the same object
    differently: Python puts a space after every comma and colon. Each one asks
    the same question of its own runtime, which is the question that matters,
    because the answer decides whether a replayed tool message carries the same
    bytes the live one did and so hashes into the same next model call.
    """
    content = message.content
    if not isinstance(content, str):
        return plain(content)
    try:
        parsed = json.loads(content)
    except ValueError:
        return content  # not JSON; the content is the result
    if as_tool_content(parsed) == content:
        return parsed
    return content


def as_tool_content(output: Any) -> str:
    """A recorded tool result, back as the text a tool message carries.

    The same call LangChain makes when it turns a tool's return value into a
    tool message, so a replayed tool message is byte-for-byte the live one and
    the next model call hashes to what the log recorded.
    """
    if isinstance(output, str):
        return output
    try:
        return json.dumps(output, ensure_ascii=False)
    except (TypeError, ValueError):
        return str(output)
