"""What a model call is, reduced to the parts that decide the answer.

A recorded model call is keyed by one string, its ``request_hash``, and a
resumed invoke replays the recorded answer only when it re-derives that exact
string. So this module has one job: turn LangChain's ``ModelRequest``, a live
object graph full of class instances, bound callbacks and per-invoke ids, into
a plain value that holds everything which changes the answer and nothing which
does not.

What is in: the model's identity and its answer-shaping settings, the system
message, every message in order (role, content, tool calls, tool results), the
tools offered with their schemas, the tool choice, the response format, and the
per-request model settings.

What is deliberately out: message ids (LangGraph mints a fresh one for the
human message on every invoke), ``additional_kwargs`` and ``response_metadata``
(provider bookkeeping, and the place this middleware writes its own replay
marker, so hashing it would make the second invoke disagree with the first),
usage counts, and callbacks. A field that varies between two identical invokes
cannot be in the key, or nothing would ever replay.

The shape is the TypeScript middleware's ``canonicalRequest``, key for key, so
a thread recorded by one SDK and resumed by the other meets its own answers.
Two things about that could not be carried across and are named here rather
than hidden:

* ``ModelRequest`` in Python has no ``undefined``, so ``tool_choice`` and
  ``response_format`` are omitted when they are ``None``, which is the value
  LangChain gives them when the app set neither. TypeScript omits them when
  they are ``undefined``, for the same reason.
* the answer-shaping fields are named the way Python's providers name them
  (``max_tokens``, not ``maxTokens``), because that is what is actually on a
  Python chat model. A model whose settings live under a name only one language
  uses simply contributes nothing to the other's hash.
"""

from __future__ import annotations

from typing import Any, Dict

from .hash import hash_value, json_stringify

__all__ = ["canonical_request", "request_hash"]


def canonical_request(request: Any) -> Dict[str, Any]:
    """The canonical value a model call is hashed over.

    Public because it is also what ``record_prompts`` records on the intent:
    the same shape either way, so the body an inspector shows is provably the
    body the hash was taken of.
    """
    value = {
        "model": _model_identity(getattr(request, "model", None)),
        "messages": [
            _canonical_message(message)
            for message in (getattr(request, "messages", None) or [])
        ],
    }  # type: Dict[str, Any]
    system = _system_text(request)
    if system:
        value["system"] = system
    tools = getattr(request, "tools", None)
    if tools:
        value["tools"] = [_canonical_tool(tool) for tool in tools]
    tool_choice = getattr(request, "tool_choice", None)
    if tool_choice is not None:
        value["tool_choice"] = plain(tool_choice)
    response_format = getattr(request, "response_format", None)
    if response_format is not None:
        value["response_format"] = plain(response_format)
    model_settings = getattr(request, "model_settings", None)
    if model_settings is not None:
        value["model_settings"] = plain(model_settings)
    return value


def request_hash(request: Any) -> str:
    """The ``sha256:`` hash of :func:`canonical_request`, the recorded
    correlation key."""
    return hash_value(canonical_request(request))


def _system_text(request: Any) -> str:
    """The system instruction, from whichever of the two fields carries it."""
    system_message = getattr(request, "system_message", None)
    content = getattr(system_message, "content", None)
    if isinstance(content, str) and content:
        return content
    if isinstance(content, list):
        return json_stringify(plain(content))
    prompt = getattr(request, "system_prompt", None)
    return prompt if isinstance(prompt, str) else ""


def _canonical_message(message: Any) -> Dict[str, Any]:
    """One message, reduced to what a provider would actually be sent: who said
    it, what they said, which tool calls it asked for, and which call a tool
    result answers.

    Ids of the messages themselves are left out on purpose (see the module
    docs); the ids INSIDE ``tool_calls`` are kept, because those come back from
    the model, are recorded with its answer, and replay identically.
    """
    value = {
        "role": getattr(message, "type", ""),
        "content": plain(getattr(message, "content", None)),
    }  # type: Dict[str, Any]
    name = getattr(message, "name", None)
    if name:
        value["name"] = name
    tool_calls = getattr(message, "tool_calls", None)
    if tool_calls:
        value["tool_calls"] = [
            {
                "name": call.get("name"),
                "args": plain(call.get("args")),
                "id": call.get("id") or None,
            }
            for call in (plain(tool_calls) or [])
        ]
    tool_call_id = getattr(message, "tool_call_id", None)
    if isinstance(tool_call_id, str):
        value["tool_call_id"] = tool_call_id
    # A tool message's `status` is deliberately out. It is LangChain's own
    # classification of a result, set by whichever object built the message,
    # and it is not part of what a completion records. Hashing it would make
    # replay depend on something the log does not hold: the live message would
    # carry "success" from the tool's own invoke and the replayed one would
    # carry whatever this middleware chose to rebuild it with, and the two
    # would never agree. The result itself, which is what the model reads, is
    # in `content`.
    return value


def _canonical_tool(tool: Any) -> Dict[str, Any]:
    """One tool as the model sees it: name, description, parameter schema.

    The schema is in because it changes the arguments the model produces; a
    team that edits a tool's schema mid-flight has changed the question, and a
    resumed thread is right to say so rather than replay an answer to the old
    one.
    """
    if isinstance(tool, dict):
        # A provider-native tool declaration, passed through as the app wrote
        # it. There is nothing to normalise: it already is a plain value.
        return plain(tool)
    value = {"name": getattr(tool, "name", None)}  # type: Dict[str, Any]
    description = getattr(tool, "description", None)
    if description:
        value["description"] = description
    schema = _tool_schema(tool)
    if schema is not None:
        value["schema"] = schema
    return value


def _tool_schema(tool: Any) -> Any:
    """A tool's parameters as JSON Schema, or ``None`` when this build cannot
    render them.

    A schema that will not render is left out rather than hashed in some
    unstable form: a key that varied by library version would break every
    resume across an upgrade.
    """
    for attribute in ("tool_call_schema", "args_schema", "input_schema"):
        try:
            candidate = getattr(tool, attribute, None)
        except Exception:  # pragma: no cover - a property that refuses to be read
            continue
        if candidate is None:
            continue
        if isinstance(candidate, dict):
            return plain(candidate)
        rendered = getattr(candidate, "model_json_schema", None)
        if callable(rendered):
            try:
                return plain(rendered())
            except Exception:
                continue
    return None


def _model_identity(model: Any) -> Dict[str, Any]:
    """The model's identity and the settings that shape its answer.

    A model handed to ``wrap_model_call`` may be wrapped in a
    ``RunnableBinding`` (that is what ``.bind_tools()`` and ``.with_config()``
    return), so the wrapper is peeled before anything is read. Only scalar and
    sequence settings are taken: an object on a model instance is a client, a
    cache or a callback manager, none of which decide the answer and all of
    which differ between two processes.
    """
    current = model
    for _ in range(8):
        bound = getattr(current, "bound", None)
        if bound is None:
            break
        current = bound
    if current is None:
        return {}
    value = {}  # type: Dict[str, Any]
    try:
        llm_type = current._llm_type
    except Exception:
        llm_type = None  # a model that will not name itself is identified by its fields
    if isinstance(llm_type, str):
        value["type"] = llm_type
    for key in ANSWER_SHAPING_FIELDS:
        try:
            field = getattr(current, key, None)
        except Exception:  # pragma: no cover - a property that refuses to be read
            continue
        if field is None:
            continue
        if isinstance(field, bool) or isinstance(field, (str, int, float)):
            value[key] = field
        elif isinstance(field, (list, tuple)):
            value[key] = plain(field)
    return value


#: Model fields that change the answer, across the providers LangChain ships.
#: A field a given provider does not have is simply absent from the hash.
ANSWER_SHAPING_FIELDS = (
    "model",
    "model_name",
    "model_id",
    "deployment",
    "deployment_name",
    "temperature",
    "top_p",
    "top_k",
    "max_tokens",
    "max_output_tokens",
    "max_completion_tokens",
    "stop",
    "stop_sequences",
    "seed",
    "presence_penalty",
    "frequency_penalty",
    "thinking",
    "reasoning_effort",
)


def plain(value: Any) -> Any:
    """A plain JSON value: dicts, lists, strings, numbers, booleans and
    ``None``.

    The Python answer to the TypeScript file's round trip through
    ``JSON.stringify``. A pydantic model (which every LangChain message, tool
    schema and content block is) becomes its JSON-mode fields; a set becomes a
    sorted list, because a set has no order of its own and a hash needs one;
    anything else that cannot be rendered becomes its ``str``, which is what
    the TypeScript file does with a value ``JSON.stringify`` refuses.
    """
    if value is None or isinstance(value, (str, bool, int, float)):
        return value
    if isinstance(value, dict):
        return {
            str(key): plain(item)
            for key, item in value.items()
            if not callable(item)
        }
    if isinstance(value, (list, tuple)):
        return [plain(item) for item in value]
    if isinstance(value, (set, frozenset)):
        return sorted(plain(item) for item in value)
    dump = getattr(value, "model_dump", None)
    if callable(dump):
        try:
            return plain(dump(mode="json"))
        except Exception:
            try:
                return plain(dump())
            except Exception:
                pass
    isoformat = getattr(value, "isoformat", None)
    if callable(isoformat):
        try:
            return str(isoformat())
        except Exception:  # pragma: no cover - an object lying about its shape
            pass
    return str(value)
