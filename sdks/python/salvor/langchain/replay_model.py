"""The model that answers from the log.

A replayed answer could simply be returned from ``wrap_model_call``, and for
``ainvoke`` that would be enough. It is not enough for ``astream``. LangGraph's
message stream is fed by the callbacks a chat model raises, not by what a node
returns (the agent's model node returns state, which the message handler does
not look inside), so a middleware that skips the model entirely makes a
replayed turn arrive as nothing at all. A silent replay is the one behaviour
worth ruling out: a user watching a resumed run should see the answer, and
should be told it came from the log rather than from the provider.

So the recorded answer is delivered the same way a live one is: through the
handler, with this stand-in in the model's place. Nothing leaves the process,
no key is read and no provider is called. LangChain raises its usual model
callbacks around a call that was already made once, and the answer reaches the
stream whole, in one chunk, exactly as it was recorded. It is never
re-tokenised: the tokens happened once, on the invoke that paid for them, and
inventing a second stream of them would be a lie about what this run cost.
"""

from __future__ import annotations

from typing import Any, List, Optional, Sequence

from langchain_core.language_models.chat_models import BaseChatModel
from langchain_core.messages import AIMessage, BaseMessage
from langchain_core.outputs import ChatGeneration, ChatResult

__all__ = ["ReplayChatModel"]


class ReplayChatModel(BaseChatModel):
    """A chat model whose only answer is one message read back out of a salvor log."""

    #: The recorded answer, already carrying its replay marker.
    answer: AIMessage

    def __init__(self, answer: AIMessage, **kwargs: Any) -> None:
        super().__init__(answer=answer, **kwargs)

    @property
    def _llm_type(self) -> str:
        return "salvor-replay"

    def bind_tools(self, tools: Sequence[Any], **kwargs: Any) -> "ReplayChatModel":
        """Binding tools to a recorded answer changes nothing: the answer
        already exists, tool calls and all.

        The method is here because the agent binds before it calls, and it
        returns this same model so the binding is a no-op rather than a
        ``RunnableBinding`` that has lost the answer.
        """
        return self

    def _generate(
        self,
        messages: List[BaseMessage],
        stop: Optional[List[str]] = None,
        run_manager: Any = None,
        **kwargs: Any,
    ) -> ChatResult:
        return ChatResult(generations=[ChatGeneration(message=self.answer)])

    async def _agenerate(
        self,
        messages: List[BaseMessage],
        stop: Optional[List[str]] = None,
        run_manager: Any = None,
        **kwargs: Any,
    ) -> ChatResult:
        return ChatResult(generations=[ChatGeneration(message=self.answer)])
