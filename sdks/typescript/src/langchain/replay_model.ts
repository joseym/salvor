/**
 * The model that answers from the log.
 *
 * A replayed answer could simply be returned from `wrapModelCall`, and for
 * `invoke` that would be enough. It is not enough for `stream`. LangGraph's
 * message stream is fed by the callbacks a chat model raises, not by what a
 * node returns (the agent's model node returns a `Command`, which the message
 * handler does not look inside), so a middleware that skips the model entirely
 * makes a replayed turn arrive as nothing at all. A silent replay is the one
 * behaviour worth ruling out: a user watching a resumed run should see the
 * answer, and should be told it came from the log rather than from the
 * provider.
 *
 * So the recorded answer is delivered the same way a live one is: through the
 * handler, with this stand-in in the model's place. Nothing leaves the process,
 * no key is read and no provider is called. LangChain raises its usual model
 * callbacks around a call that was already made once, and the answer reaches
 * the stream whole, in one chunk, exactly as it was recorded. It is never
 * re-tokenised: the tokens happened once, on the invoke that paid for them, and
 * inventing a second stream of them would be a lie about what this run cost.
 */

import type { AIMessage, BaseMessage } from "@langchain/core/messages";
import { BaseChatModel } from "@langchain/core/language_models/chat_models";
import type { ChatResult } from "@langchain/core/outputs";

/** A chat model whose only answer is one message read back out of a salvor log. */
export class ReplayChatModel extends BaseChatModel {
  private readonly answer: AIMessage;

  constructor(answer: AIMessage) {
    super({});
    this.answer = answer;
  }

  _llmType(): string {
    return "salvor-replay";
  }

  _combineLLMOutput(): never[] {
    return [];
  }

  /**
   * Binding tools to a recorded answer changes nothing: the answer already
   * exists, tool calls and all. The method is here because the agent binds
   * before it calls, and it returns this same model so the binding is a no-op
   * rather than a copy that has lost the answer.
   */
  bindTools(_tools: unknown[], _options?: unknown): this {
    return this;
  }

  async _generate(_messages: BaseMessage[]): Promise<ChatResult> {
    const message = this.answer;
    const text =
      typeof message.content === "string"
        ? message.content
        : JSON.stringify(message.content);
    return { generations: [{ text, message }], llmOutput: {} };
  }
}
