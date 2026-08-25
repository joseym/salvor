/**
 * What a model call is, reduced to the parts that decide the answer.
 *
 * A recorded model call is keyed by one string, its `request_hash`, and a
 * resumed invoke replays the recorded answer only when it re-derives that exact
 * string. So this module has one job: turn LangChain's `ModelRequest`, a live
 * object graph full of class instances, bound callbacks and per-invoke ids,
 * into a plain value that holds everything which changes the answer and nothing
 * which does not.
 *
 * What is in: the model's identity and its answer-shaping settings, the system
 * message, every message in order (role, content, tool calls, tool results),
 * the tools offered with their schemas, the tool choice, the response format,
 * and the per-request model settings.
 *
 * What is deliberately out: message ids (LangGraph mints a fresh one for the
 * human message on every invoke), `additional_kwargs` and `response_metadata`
 * (provider bookkeeping, and the place this middleware writes its own replay
 * marker, so hashing it would make the second invoke disagree with the first),
 * usage counts, and callbacks. A field that varies between two identical
 * invokes cannot be in the key, or nothing would ever replay.
 */

import type { BaseMessage } from "@langchain/core/messages";
import { toJsonSchema } from "@langchain/core/utils/json_schema";
import { hashValue } from "./hash.js";

/** The subset of LangChain's `ModelRequest` this module reads. */
export interface HashableModelRequest {
  model: unknown;
  messages: BaseMessage[];
  systemPrompt?: string;
  systemMessage?: { content?: unknown };
  tools?: unknown[];
  toolChoice?: unknown;
  responseFormat?: unknown;
  modelSettings?: Record<string, unknown>;
}

/**
 * The canonical value a model call is hashed over. Exported because it is also
 * what `recordPrompts` records on the intent: the same shape either way, so the
 * body an inspector shows is provably the body the hash was taken of.
 */
export function canonicalRequest(request: HashableModelRequest): Record<string, unknown> {
  const value: Record<string, unknown> = {
    model: modelIdentity(request.model),
    messages: request.messages.map(canonicalMessage),
  };
  const system = systemText(request);
  if (system) value.system = system;
  if (request.tools?.length) value.tools = request.tools.map(canonicalTool);
  if (request.toolChoice !== undefined) value.tool_choice = request.toolChoice;
  if (request.responseFormat !== undefined) {
    value.response_format = plain(request.responseFormat);
  }
  if (request.modelSettings !== undefined) {
    value.model_settings = plain(request.modelSettings);
  }
  return value;
}

/** The `sha256:` hash of {@link canonicalRequest}, the recorded correlation key. */
export function requestHash(request: HashableModelRequest): Promise<string> {
  return hashValue(canonicalRequest(request));
}

/** The system instruction, from whichever of the two fields carries it. */
function systemText(request: HashableModelRequest): string {
  const fromMessage = request.systemMessage?.content;
  if (typeof fromMessage === "string" && fromMessage) return fromMessage;
  if (Array.isArray(fromMessage)) return JSON.stringify(fromMessage);
  return request.systemPrompt ?? "";
}

/**
 * One message, reduced to what a provider would actually be sent: who said it,
 * what they said, which tool calls it asked for, and which call a tool result
 * answers. Ids of the messages themselves are left out on purpose (see the
 * module docs); the ids INSIDE `tool_calls` are kept, because those come back
 * from the model, are recorded with its answer, and replay identically.
 */
function canonicalMessage(message: BaseMessage): Record<string, unknown> {
  const anyMessage = message as unknown as Record<string, unknown>;
  const value: Record<string, unknown> = {
    role: message.getType(),
    content: plain(message.content),
  };
  if (message.name) value.name = message.name;
  const toolCalls = anyMessage.tool_calls as
    | { name: string; args: unknown; id?: string }[]
    | undefined;
  if (toolCalls?.length) {
    value.tool_calls = toolCalls.map((call) => ({
      name: call.name,
      args: plain(call.args),
      id: call.id ?? null,
    }));
  }
  if (typeof anyMessage.tool_call_id === "string") {
    value.tool_call_id = anyMessage.tool_call_id;
  }
  // A tool message's `status` is deliberately out. It is LangChain's own
  // classification of a result, set by whichever object built the message, and
  // it is not part of what a completion records. Hashing it would make replay
  // depend on something the log does not hold: the live message would carry
  // `"success"` from the tool's own invoke and the replayed one would carry
  // whatever this middleware chose to rebuild it with, and the two would never
  // agree. The result itself, which is what the model reads, is in `content`.
  return value;
}

/**
 * One tool as the model sees it: name, description, parameter schema. The
 * schema is in because it changes the arguments the model produces; a team that
 * edits a tool's schema mid-flight has changed the question, and a resumed
 * thread is right to say so rather than replay an answer to the old one.
 */
function canonicalTool(tool: unknown): Record<string, unknown> {
  const anyTool = tool as Record<string, unknown>;
  const value: Record<string, unknown> = { name: anyTool.name };
  if (anyTool.description) value.description = anyTool.description;
  const schema = anyTool.schema ?? anyTool.parameters ?? anyTool.inputSchema;
  if (schema !== undefined) {
    try {
      value.schema = plain(toJsonSchema(schema as never));
    } catch {
      // A schema this build cannot render as JSON Schema is left out rather
      // than hashed in some unstable form: a key that varies by library
      // version would break every resume across an upgrade.
    }
  }
  return value;
}

/**
 * The model's identity and the settings that shape its answer.
 *
 * A model handed to `wrapModelCall` may be wrapped in a `RunnableBinding` (that
 * is what `.bindTools()` and `.withConfig()` return), so the wrapper is peeled
 * before anything is read. Only scalar and array settings are taken: an object
 * on a model instance is a client, a cache or a callback manager, none of which
 * decide the answer and all of which differ between two processes.
 */
function modelIdentity(model: unknown): Record<string, unknown> {
  let current = model as Record<string, unknown> | undefined;
  for (let depth = 0; depth < 8 && current?.bound !== undefined; depth += 1) {
    current = current.bound as Record<string, unknown>;
  }
  if (!current) return {};
  const value: Record<string, unknown> = {};
  const llmType = current._llmType;
  if (typeof llmType === "function") {
    try {
      value.type = (llmType as () => string).call(current);
    } catch {
      /* a model that will not name itself is identified by its fields alone */
    }
  }
  for (const key of ANSWER_SHAPING_FIELDS) {
    const field = current[key];
    if (field === undefined || field === null) continue;
    if (typeof field === "object" && !Array.isArray(field)) continue;
    if (typeof field === "function") continue;
    value[key] = field;
  }
  return value;
}

/**
 * Model fields that change the answer, across the providers LangChain ships.
 * A field a given provider does not have is simply absent from the hash.
 */
const ANSWER_SHAPING_FIELDS = [
  "model",
  "modelName",
  "modelId",
  "deployment",
  "deploymentName",
  "temperature",
  "topP",
  "topK",
  "maxTokens",
  "maxOutputTokens",
  "maxCompletionTokens",
  "stop",
  "stopSequences",
  "seed",
  "presencePenalty",
  "frequencyPenalty",
  "thinking",
  "reasoningEffort",
] as const;

/**
 * A plain JSON value, by round-tripping through `JSON.stringify`. Class
 * instances become their serializable fields, functions and `undefined`
 * disappear, and what is left is something {@link canonicalJson} can order.
 */
function plain(value: unknown): unknown {
  if (value === undefined) return undefined;
  try {
    return JSON.parse(JSON.stringify(value));
  } catch {
    return String(value);
  }
}
