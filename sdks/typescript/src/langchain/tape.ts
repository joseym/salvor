/**
 * One thread's place in one salvor run: the cursor, the turnstile, and the
 * rule for deciding whether a step is a replay or a live call.
 *
 * A LangGraph invoke re-walks the graph from the top every time. This class is
 * what makes the second walk cheap: it hands each step the log position the
 * first walk used, asks salvor what is recorded there, and either returns the
 * recorded answer or performs the call and records it. The positions come from
 * counting, not from guessing, which is why the turnstile exists: two tool
 * calls in one model turn would otherwise both try to open an intent at the
 * same place, and the log's append-guard would refuse the second.
 *
 * ## The cursor
 *
 * `RunStarted` is seq 0. Every call after it is a pair, intent then completion,
 * so the cursor moves by two per step: a model call at 1 and 2, a tool call at
 * 3 and 4, the next model call at 5 and 6. The cursor starts at 1 on every
 * invoke, not at the end of the log, because a re-invoke re-walks the graph
 * from the top and has to meet the recorded steps in the order they were
 * recorded.
 *
 * ## Leaving the recorded path
 *
 * A re-invoke that asks for something the log does not hold at the cursor has
 * left the recorded path: a new turn on the thread, an edited prompt, a
 * different branch. The cursor then jumps to the end of the log and the run
 * carries on there, so the fork is appended rather than lost. The one case that
 * cannot be appended to is a log ending at an intent with no completion, and
 * that case is refused by name: an unfinished call is exactly what a person has
 * to settle before the run means anything again.
 */

import type { ClientRunDriver } from "../client_runs.js";
import type { SalvorEvent, Usage } from "../types.js";
import { SalvorMiddlewareError } from "./errors.js";
import { canonicalJson } from "./hash.js";

/** What a model step turned out to be. */
export interface ModelOutcome {
  seq: number;
  replayed: boolean;
  response: unknown;
  usage: Usage;
}

/** What a tool step turned out to be, including the key the server derived. */
export interface ToolOutcome {
  seq: number;
  replayed: boolean;
  output: unknown;
  effect: string;
  idempotencyKey: string;
}

/**
 * Where one tool call sits in the model turn that asked for it.
 *
 * `turn` identifies the turn (the AI message that listed the calls), `rank`
 * is the call's index in that message's `tool_calls`, and `total` is how many
 * calls the turn asked for. The tape uses it to admit a turn's calls in the
 * model's order rather than in whatever order LangChain happens to enter
 * `wrapToolCall` for them.
 */
export interface TurnPosition {
  turn: string;
  rank: number;
  total: number;
}

/** One AI message's tool calls, as `noteTurn` recorded them. */
interface NotedTurn {
  turn: string;
  ids: string[];
}

/** A message's tool calls, in the shape `noteTurn` needs. */
interface ToolCallBearer {
  id?: string | null;
  tool_calls?: readonly { id?: string | null }[] | null;
}

/** One turn's admission state: which rank goes next, and who is waiting. */
interface TurnGate {
  next: number;
  waiting: Map<number, () => void>;
}

const ZERO_USAGE: Usage = { inputTokens: 0, outputTokens: 0 };

/** Drives one thread's run for the length of one agent invocation. */
export class RunTape {
  readonly runId: string;
  private readonly driver: ClientRunDriver;
  private readonly recordPrompts: boolean;
  /** The recorded log at the moment this invoke opened the run, keyed by seq. */
  private readonly recorded: Map<number, SalvorEvent>;
  private readonly recordedLength: number;
  /** The next free position; every step takes this one and the one after it. */
  private cursor = 1;
  /** False once this invoke has asked for something the log does not hold. */
  private replaying: boolean;
  /** The turnstile: one open intent at a time. */
  private queue: Promise<unknown> = Promise.resolve();
  /** The last model turn `noteTurn` recorded, for `positionOf` to read ranks from. */
  private lastTurn: NotedTurn | undefined;
  /** Per-turn admission state, keyed by `TurnPosition.turn`. */
  private turns = new Map<string, TurnGate>();

  private constructor(driver: ClientRunDriver, recordPrompts: boolean) {
    this.driver = driver;
    this.runId = driver.runId;
    this.recordPrompts = recordPrompts;
    this.recorded = new Map(driver.logEnvelopes.map((event) => [event.seq, event]));
    this.recordedLength = driver.logEnvelopes.length;
    this.replaying = this.recordedLength > 0;
  }

  /**
   * Open (or re-open) the run behind a thread and take up its cursor.
   *
   * A fresh run gets its `RunStarted` here, because a client-driven run's first
   * event is the client's to write and nothing else can be appended before it.
   * A run that already has one is left alone: re-opening returns the recorded
   * log and mints a fresh lease, which is all a resuming invoke needs.
   */
  static async open(
    driver: ClientRunDriver,
    started: Record<string, unknown>,
    recordPrompts: boolean,
  ): Promise<RunTape> {
    if (driver.logEnvelopes.length === 0) {
      await driver.append([driver.envelope(0, "RunStarted", started)]);
    }
    return new RunTape(driver, recordPrompts);
  }

  /** The driver underneath, for a caller that wants the log or the lease. */
  get run(): ClientRunDriver {
    return this.driver;
  }

  /**
   * Record a model call, replaying the recorded answer when there is one.
   *
   * `perform` is called only when salvor says the position is not settled, so a
   * re-invoke of a finished thread never reaches the provider at all. It is
   * called inside the turnstile, which is why a slow model call holds the
   * position: the intent is open until the answer is recorded, and the log
   * accepts nothing else while it is.
   */
  modelCall(
    hash: string,
    body: unknown,
    perform: () => Promise<{ response: unknown; usage: Usage }>,
  ): Promise<ModelOutcome> {
    return this.turnstile(async () => {
      const seq = this.slot(
        (event) =>
          event.kind === "ModelCallRequested" && event.payload.request_hash === hash,
        "a model call",
      );
      const opened = await this.driver.clientModelIntent(
        seq,
        hash,
        this.recordPrompts ? body : undefined,
      );
      if (opened.settled) {
        return {
          seq,
          replayed: true,
          response: opened.response,
          usage: opened.usage ?? ZERO_USAGE,
        };
      }
      const answered = await perform();
      await this.driver.clientModelCompletion(seq, answered.response, answered.usage);
      return { seq, replayed: false, ...answered };
    });
  }

  /**
   * Record a tool call, replaying the recorded output when there is one.
   *
   * The effect class and the idempotency key both come back from the server,
   * derived from the operator's declaration and from `(run, seq, tool)`. The
   * middleware never chooses either, which is the whole point of the
   * client-tool surface: the party that performs a write does not get to pick
   * the key that would let a duplicate through.
   *
   * `perform` is handed that same key (with the seq it landed at) before it
   * runs, not after, because the tool body it eventually calls is what needs
   * it: see `currentToolCall()` in `current_call.ts`, which is what makes the
   * key reachable there without changing the tool's own signature.
   *
   * `position` says where this call sits in the turn that asked for it (see
   * `positionOf`), and is what makes a parallel turn replayable rather than
   * merely serialized: see `admitRank`.
   */
  toolCall(
    tool: string,
    input: unknown,
    perform: (opened: { seq: number; idempotencyKey: string }) => Promise<unknown>,
    position: TurnPosition,
  ): Promise<ToolOutcome> {
    return this.admitRank(position).then(() =>
      this.turnstile(async () => {
        const wanted = canonicalJson(input);
        const seq = this.slot(
          (event) =>
            event.kind === "ToolCallRequested" &&
            event.payload.tool === tool &&
            canonicalJson(event.payload.input) === wanted,
          `a call to the tool \`${tool}\``,
        );
        const opened = await this.driver.clientToolIntent(seq, tool, input);
        if (opened.settled) {
          return {
            seq,
            replayed: true,
            output: await this.recordedOutput(seq + 1),
            effect: opened.effect,
            idempotencyKey: opened.idempotencyKey,
          };
        }
        const output = await perform({ seq, idempotencyKey: opened.idempotencyKey });
        await this.driver.clientToolCompletion(seq, output);
        return {
          seq,
          replayed: false,
          output,
          effect: opened.effect,
          idempotencyKey: opened.idempotencyKey,
        };
      }).finally(() => this.releaseRank(position)),
    );
  }

  // -- turn positions ----------------------------------------------------------

  /**
   * Note the AI message a model call produced, so a later `positionOf` can
   * answer for the tool calls it listed.
   *
   * Called from `wrapModelCall` for every model turn, live or replayed. A
   * message with no tool calls, or one where any call is missing an id,
   * clears the noted turn instead of recording it: a call cannot be ranked
   * against a turn that cannot name its calls, and the next `positionOf`
   * should say so rather than match the wrong (stale) turn.
   */
  noteTurn(message: ToolCallBearer): void {
    const calls = message.tool_calls ?? [];
    const ids = calls.map((call) => call.id ?? undefined);
    if (ids.length === 0 || ids.some((id) => id === undefined)) {
      this.lastTurn = undefined;
      return;
    }
    this.lastTurn = {
      turn: message.id ?? ids.join("|"),
      ids: ids as string[],
    };
  }

  /**
   * Where `callId` sits in the last noted turn, or an error naming the call.
   *
   * The model's `tool_calls` order is the only order this middleware trusts
   * (see `admitRank`), so a call whose id the last turn does not list is not
   * admitted on arrival as a fallback: that would silently reintroduce the
   * ordering `admitRank` exists to rule out. It is refused instead, because a
   * call this cannot rank is a call `wrapModelCall` was not asked before, or a
   * middleware wired ahead of this one that changed the call's id.
   */
  positionOf(callId: string, tool: string): TurnPosition {
    const turn = this.lastTurn;
    const rank = turn?.ids.indexOf(callId) ?? -1;
    if (!turn || rank === -1) {
      throw new SalvorMiddlewareError(
        `run ${this.runId}: the call to \`${tool}\` (id \`${callId}\`) is not among ` +
          "the tool calls in the last recorded model turn, so its position in the " +
          "run cannot be pinned. This means either the model turn that asked for " +
          "it was never recorded, or another middleware ahead of this one changed " +
          "the call's id.",
      );
    }
    return { turn: turn.turn, rank, total: turn.ids.length };
  }

  // -- the turnstile ---------------------------------------------------------

  /**
   * Run `work` with nothing else from this run in flight.
   *
   * Every model call, and every tool call once `admitRank` has let it
   * through, joins this same queue: one open intent at a time, in the order
   * work is added to it. Rank admission is what decides that order for a
   * turn's tool calls; the queue itself just keeps two opens from racing.
   *
   * A step that throws still releases the turnstile: the next step will meet
   * whatever the failed one left in the log and be told about it there.
   */
  private turnstile<T>(work: () => Promise<T>): Promise<T> {
    const next = this.queue.then(work, work);
    this.queue = next.then(
      () => undefined,
      () => undefined,
    );
    return next;
  }

  /**
   * Wait until every earlier call in `position.turn` has left the turnstile.
   *
   * LangChain JS enters `wrapToolCall` for a turn's parallel calls
   * synchronously, in `tool_calls` order, today; the Python runtime's port of
   * this middleware found its equivalent hooks arriving out of order instead
   * (see `salvor/langchain/tape.py`'s `_await_turn`). Rather than lean on a
   * guarantee LangChain does not document for either language, both admit by
   * rank: a call waits here until the rank before it has been admitted, so
   * the recorded order is the model's `tool_calls` order regardless of which
   * order the runtime happens to schedule the calls in.
   *
   * `releaseRank`, in the caller's `finally`, is what lets the next rank
   * through; it runs whether this call's own turnstile step succeeded or
   * threw, so one failed call in a turn does not strand the ranks after it.
   */
  private admitRank(position: TurnPosition): Promise<void> {
    let gate = this.turns.get(position.turn);
    if (!gate) {
      gate = { next: 0, waiting: new Map() };
      this.turns.set(position.turn, gate);
    }
    if (gate.next === position.rank) return Promise.resolve();
    return new Promise((resolve) => gate!.waiting.set(position.rank, resolve));
  }

  /** Hand the turn on to the next rank, and forget a turn that is done. */
  private releaseRank(position: TurnPosition): void {
    const gate = this.turns.get(position.turn);
    if (!gate) return;
    gate.next = position.rank + 1;
    const waiter = gate.waiting.get(gate.next);
    if (waiter) {
      gate.waiting.delete(gate.next);
      waiter();
    }
    if (gate.next >= position.total) {
      this.turns.delete(position.turn);
    }
  }

  // -- the cursor ------------------------------------------------------------

  /**
   * The position for the step about to happen, and two positions consumed.
   *
   * While this invoke is still on the recorded path, the recorded event at the
   * cursor has to be the step being asked for. When it is not, the graph has
   * gone somewhere the log does not describe, and the rest of this invoke is
   * appended at the end of the log instead.
   */
  private slot(matches: (event: SalvorEvent) => boolean, what: string): number {
    if (this.replaying) {
      const recorded = this.recorded.get(this.cursor);
      if (recorded === undefined) {
        this.replaying = false;
      } else if (!matches(recorded)) {
        this.replaying = false;
        const tail = this.recorded.get(this.recordedLength - 1);
        if (tail && (tail.kind === "ToolCallRequested" || tail.kind === "ModelCallRequested")) {
          throw new SalvorMiddlewareError(
            `run ${this.runId} asked for ${what} at seq ${this.cursor}, but the log ` +
              `holds a ${recorded.kind} there, and its last event (seq ${tail.seq}, ` +
              `${tail.kind}) is a call that was never completed. Settle that call ` +
              `first (\`salvor run resolve ${this.runId} <output>\`, or the resolve ` +
              `endpoint) and invoke again.`,
          );
        }
        this.cursor = this.recordedLength;
      }
    }
    const seq = this.cursor;
    this.cursor += 2;
    return seq;
  }

  /**
   * The output recorded at `seq`, from this invoke's snapshot of the log when it
   * is there and from a fresh read when it is not (a completion another drive
   * wrote after this one opened the run).
   */
  private async recordedOutput(seq: number): Promise<unknown> {
    const known = this.recorded.get(seq);
    if (known?.kind === "ToolCallCompleted") return known.payload.output;
    const tail = await this.driver.log(seq);
    const completion = tail[0];
    if (completion?.seq !== seq || completion.kind !== "ToolCallCompleted") {
      throw new SalvorMiddlewareError(
        `run ${this.runId} reports the tool call at seq ${seq - 1} settled, but ` +
          `seq ${seq} holds no completion to replay.`,
      );
    }
    return completion.payload.output;
  }
}
