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
 *
 * A fork is remembered rather than only acted on. `forkedAt` holds the seq the
 * tape walked off the recorded path at, and `announceFork` hands out exactly
 * one permit to say so, so the middleware can mark every message it returns
 * afterwards and warn once instead of forking in silence.
 *
 * ## A tool that will not self-complete
 *
 * `trust_completion = false` on a tool's declaration means the operator will
 * not let a client close that call by reporting on it: salvor refuses the
 * completion outright, whatever it says. `toolCall` still opens the intent
 * and still runs the tool body under it, because the body's effect already
 * happened by the time anyone could decide otherwise; what it does not do is
 * post the result. It throws {@link ToolNeedsResolution} instead, and the
 * intent is left open, the same shape a crash between intent and completion
 * leaves it in. A re-invoke before a person resolves it meets that same open
 * intent and is refused by the "never completed" check above; a re-invoke
 * after resolution meets a completion at the intent's seq, recorded by the
 * resolve call rather than by this tape, and replays it exactly as it would
 * replay one of its own.
 *
 * ## The lease
 *
 * Every write this tape makes presents the drive token the run was opened
 * with. Two, and only two, things can make the server refuse it:
 *
 * - `invalid_drive_token`: something else now holds the run's lease, so this
 *   tape's token is superseded. That something is another driver of the same
 *   thread, live, right now, which is exactly the condition one driver per
 *   thread exists to rule out. `lease()` refuses this by name at once,
 *   without re-opening and without a second attempt: there is no order in
 *   which two live drivers can both be right about what comes next, so
 *   trading the lease back and forth would only let them record each other's
 *   calls out of turn.
 * - `unknown_run`: this server has no lease for the run AND its log does not
 *   (yet, from this process's view) say it is client-driven, which is what a
 *   restart looks like: the lease registry does not survive the process, and
 *   this tape's own copy of that fact is now stale. Nobody else is driving in
 *   this case, so `lease()` re-opens once, rebuilds what it knows of the log
 *   from what comes back, and retries the step where it stood.
 *
 * The two are told apart because they mean opposite things: retrying on
 * `invalid_drive_token` would hand the run to whichever of two live drivers
 * asks last, and refusing outright on `unknown_run` would make an invoke fail
 * a durability feature exists to survive. A retry after the one re-open
 * `unknown_run` earns is still possible to lose to a genuinely new driver
 * that opened the restarted server first; that second refusal is
 * `invalid_drive_token` too, and gets the same immediate, unretried refusal.
 *
 * A lease is also something to hold on to and something to give back, and this
 * tape does both. `keepAlive` beats while a tool body or a live model call is
 * running, because that is the one stretch of an invoke where nothing presents
 * the drive token and a body of minutes would otherwise lose the run it is
 * working on. `release` hands the lease back when the invoke ends, so the next
 * process to invoke the thread takes it on its next request rather than
 * waiting out a TTL for a driver that has already gone home.
 */

import type { ClientRunDriver } from "../client_runs.js";
import { SalvorApiError } from "../errors.js";
import type { SalvorEvent, Usage } from "../types.js";
import { SalvorMiddlewareError, ToolNeedsResolution } from "./errors.js";
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

/**
 * How a tape reaches its run: which thread it stands for, what it records, and
 * how to take the run up again when the lease it holds stops being the run's.
 */
export interface RunTapeOptions {
  /** The LangGraph thread this run is the record of, for errors that name it. */
  threadId: string;
  /** Record each model request's body on its intent. */
  recordPrompts: boolean;
  /**
   * Open the run again and hand back a driver holding a fresh lease. Called
   * only when the server has forgotten the run entirely (`unknown_run`, what
   * a restart looks like); a step refused because another driver actively
   * holds the lease (`invalid_drive_token`) is never retried this way. See
   * `lease`.
   */
  reopen: () => Promise<ClientRunDriver>;
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

/**
 * How long a body runs before the first beat goes out, while the TTL is still
 * unknown.
 *
 * The open response does not carry the lease TTL and a heartbeat's answer
 * does, so the only way to learn it is to beat once. This number is the
 * compromise that follows: early enough to be in time under a TTL of one
 * second, late enough that an ordinary tool body (a lookup, a fetch) finishes
 * and never beats at all.
 */
const PROBE_BEAT_MS = 250;

/** The floor on a beat interval, whatever a very short TTL divides down to. */
const MIN_BEAT_MS = 50;

/** Drives one thread's run for the length of one agent invocation. */
export class RunTape {
  readonly runId: string;
  /** The LangGraph thread this run is the record of. */
  readonly threadId: string;
  /** Replaced, not fixed: a lost lease is taken up again by re-opening. */
  private driver: ClientRunDriver;
  private readonly recordPrompts: boolean;
  private readonly reopenRun: () => Promise<ClientRunDriver>;
  /** The recorded log as of the last time this tape opened the run, keyed by seq. */
  private recorded: Map<number, SalvorEvent>;
  private recordedLength: number;
  /** The next free position; every step takes this one and the one after it. */
  private cursor = 1;
  /** False once this invoke has asked for something the log does not hold. */
  private replaying: boolean;
  /** The seq this invoke walked off the recorded path at, if it did. */
  private forked: number | undefined;
  /** Whether the one fork warning this invoke gets has been handed out. */
  private forkAnnounced = false;
  /** The turnstile: one open intent at a time. */
  private queue: Promise<unknown> = Promise.resolve();
  /** The last model turn `noteTurn` recorded, for `positionOf` to read ranks from. */
  private lastTurn: NotedTurn | undefined;
  /** Per-turn admission state, keyed by `TurnPosition.turn`. */
  private turns = new Map<string, TurnGate>();
  /**
   * The lease TTL in milliseconds, as the last heartbeat reported it, or
   * undefined until one has been answered. Remembered for the life of the
   * tape, so only the first long body of an invoke pays the probe beat.
   */
  private leaseTtlMs: number | undefined;

  private constructor(driver: ClientRunDriver, options: RunTapeOptions) {
    this.driver = driver;
    this.runId = driver.runId;
    this.threadId = options.threadId;
    this.recordPrompts = options.recordPrompts;
    this.reopenRun = options.reopen;
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
    options: RunTapeOptions,
  ): Promise<RunTape> {
    const tape = new RunTape(driver, options);
    if (driver.logEnvelopes.length === 0) {
      // Through the tape rather than the driver, so the very first write of a
      // run is under the same lease rule as every write after it: a run opened
      // and taken away before its `RunStarted` landed is the same fact as one
      // taken away halfway through, and deserves the same one retry.
      await tape.lease(() =>
        tape.driver.append([tape.driver.envelope(0, "RunStarted", started)]),
      );
    }
    return tape;
  }

  /** The driver underneath, for a caller that wants the log or the lease. */
  get run(): ClientRunDriver {
    return this.driver;
  }

  /**
   * The seq this invoke left the recorded path at, or undefined while it is
   * still on it. Set once and never cleared: everything after a fork is off
   * the recorded path too, which is why the middleware can mark every later
   * message from this one field.
   */
  get forkedAt(): number | undefined {
    return this.forked;
  }

  /**
   * The one permit this invoke gets to say it forked: true the first time it
   * is called after a fork, false every time after. A fork is one event in the
   * life of an invoke, not one per step taken after it, and a warning repeated
   * per step is a warning nobody reads.
   */
  announceFork(): boolean {
    if (this.forked === undefined || this.forkAnnounced) return false;
    this.forkAnnounced = true;
    return true;
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
      const opened = await this.lease(() =>
        this.driver.clientModelIntent(seq, hash, this.recordPrompts ? body : undefined),
      );
      if (opened.settled) {
        return {
          seq,
          replayed: true,
          response: opened.response,
          usage: opened.usage ?? ZERO_USAGE,
        };
      }
      // Under the beats: a model call the app performs itself can outlast the
      // lease TTL (a long completion, a stream it is rendering), and nothing
      // else presents the token while it does.
      const answered = await this.keepAlive(perform);
      await this.lease(() =>
        this.driver.clientModelCompletion(seq, answered.response, answered.usage),
      );
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
   *
   * `trustCompletion` is the operator's own word for this tool (the
   * declaration's `trust_completion`), not something this tape decides. When
   * it is `false` the first invoke to reach this call still runs the body,
   * under the same intent and the same key as always, but never reports the
   * result: salvor refuses a client completion for such a tool regardless, so
   * reporting it would only trade a clear stop for a bare `403`. Instead this
   * throws {@link ToolNeedsResolution} carrying the unrecorded output, and the
   * intent is left open exactly as a crash between intent and completion
   * would leave it, for a person to settle with {@link ClientRunDriver.resolve}
   * (or the CLI) before the thread is invoked again.
   *
   * A LATER invoke that meets that same open intent, still unresolved, does
   * not run the body a second time: for an untrusted write, retrying the
   * call itself is exactly the thing `trust_completion = false` exists to
   * rule out. It is refused instead, by the same "never completed" refusal a
   * mismatched replay throws (see `slot`), because that is what this is: a
   * call recorded as requested with nothing this tape may treat as its
   * completion.
   */
  toolCall(
    tool: string,
    input: unknown,
    perform: (opened: { seq: number; idempotencyKey: string }) => Promise<unknown>,
    position: TurnPosition,
    trustCompletion: boolean,
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
        // Set before the intent call below, from this invoke's own opening
        // snapshot: true only when this exact position already held this
        // tool's intent before this invoke started, which is what tells a
        // dangling untrusted call apart from one this invoke is opening for
        // the first time.
        const leftOverFromEarlierInvoke = this.recorded.get(seq)?.kind === "ToolCallRequested";
        const opened = await this.lease(() =>
          this.driver.clientToolIntent(seq, tool, input),
        );
        if (opened.settled) {
          return {
            seq,
            replayed: true,
            output: await this.recordedOutput(seq + 1),
            effect: opened.effect,
            idempotencyKey: opened.idempotencyKey,
          };
        }
        if (!trustCompletion && leftOverFromEarlierInvoke) {
          throw new SalvorMiddlewareError(
            `run ${this.runId} (thread \`${this.threadId}\`) met the intent for \`${tool}\` ` +
              `at seq ${seq} that an earlier invoke left open: its declaration sets ` +
              "`trust_completion = false`, so that call's result was never reported and it " +
              "is a call that was never completed. Settle it first (`salvor resolve " +
              `${this.runId} --store <the server's store> --output '<json the tool ` +
              `returned>'\`, \`POST /v1/runs/${this.runId}/resolve\` on the server, ` +
              "or `driver.resolve(...)`) and invoke again.",
            { code: "open_intent" },
          );
        }
        // Under the beats, for the same reason a model call is: the tool body
        // is the longest stretch of an invoke that presents the drive token
        // nowhere, and a body of minutes must not lose the run it is working
        // on to the next opener.
        const output = await this.keepAlive(() =>
          perform({ seq, idempotencyKey: opened.idempotencyKey }),
        );
        if (!trustCompletion) {
          throw new ToolNeedsResolution({
            run: this.runId,
            seq,
            thread: this.threadId,
            tool,
            output,
            key: opened.idempotencyKey,
          });
        }
        await this.lease(() => this.driver.clientToolCompletion(seq, output));
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
        { code: "call_unranked" },
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

  // -- the lease -------------------------------------------------------------

  /**
   * Hand the run's lease back, so the next process to invoke this thread takes
   * it at once instead of being refused for the rest of the TTL.
   *
   * Called when an invoke ends, on the success path and on the error paths
   * alike (see `index.ts`), and never on the one-driver refusals: a lease that
   * is not this invoke's is not this invoke's to end.
   *
   * A failure here is swallowed on purpose. Releasing is a courtesy to the
   * next driver, not part of what the invoke was asked to do: the log is
   * already written, the answer is already computed, and turning "I could not
   * say goodbye" into a thrown invoke would trade a run that costs the next
   * caller a TTL of waiting for one that costs this caller its result. The
   * lease lapses on its own either way, which is exactly what the lapse is
   * for.
   */
  async release(): Promise<boolean> {
    try {
      return await this.driver.release();
    } catch {
      return false;
    }
  }

  /**
   * Run `work` while telling the server, every so often, that this driver is
   * still here.
   *
   * A tool body or a live model call is the one stretch of an invoke that
   * makes no drive call at all: nothing presents the token between the intent
   * and the completion, so a body that outlasts the lease TTL would have its
   * run taken by the next opener while it was still working, and its
   * completion refused when it came back. The beats close that window.
   *
   * The interval comes from the server's own answer (`lapses_in_seconds`),
   * divided by three, so two beats can be lost in a row before the lease is
   * actually at risk. Until the first answer arrives the interval is
   * {@link PROBE_BEAT_MS}, because the TTL is not knowable any other way.
   *
   * A failed beat is not fatal and is deliberately not retried into an error:
   * it may be a blip (the next beat covers it), or the run may have been taken
   * or the server restarted, and both of those the next real drive call
   * discovers properly, through `lease()`, with the log in hand. Stopping the
   * beats is in a `finally`, so a body that throws leaves no timer behind.
   */
  private async keepAlive<T>(work: () => Promise<T>): Promise<T> {
    let stopped = false;
    let timer: ReturnType<typeof setTimeout> | undefined;

    const schedule = (): void => {
      timer = setTimeout(beat, this.beatInterval());
      // Never a reason for the process to stay alive: the beats exist for the
      // sake of work that is already running, and outlive nothing.
      (timer as { unref?: () => void }).unref?.();
    };
    const beat = async (): Promise<void> => {
      if (stopped) return;
      try {
        const lapsesInSeconds = await this.driver.heartbeat();
        if (lapsesInSeconds > 0) this.leaseTtlMs = lapsesInSeconds * 1000;
      } catch {
        /* a missed beat is not a failed step; the next drive call finds out */
      }
      if (!stopped) schedule();
    };

    schedule();
    try {
      return await work();
    } finally {
      stopped = true;
      if (timer) clearTimeout(timer);
    }
  }

  /** How long until the next beat: a third of the TTL, or the probe until one is known. */
  private beatInterval(): number {
    if (this.leaseTtlMs === undefined) return PROBE_BEAT_MS;
    return Math.max(MIN_BEAT_MS, Math.floor(this.leaseTtlMs / 3));
  }

  /**
   * Run one guarded step, taking the run up again only when this server has
   * simply forgotten it (a restart), and refusing by name at once when
   * something else is actively driving it.
   *
   * `step` reads `this.driver` itself rather than closing over one, because a
   * retry after `unknown_run` has to go through the driver the re-open handed
   * back, not the one whose token was just refused.
   */
  private async lease<T>(step: () => Promise<T>): Promise<T> {
    try {
      return await step();
    } catch (error) {
      if (isSupersededLease(error)) throw this.oneDriverError(error);
      if (!isForgottenRun(error)) throw error;
      await this.reopen(error);
      try {
        return await step();
      } catch (again) {
        if (isSupersededLease(again)) throw this.oneDriverError(again);
        throw again;
      }
    }
  }

  /**
   * The refusal for `invalid_drive_token`: another driver holds this run's
   * lease right now, live, which is exactly what one driver per thread rules
   * out. There is nothing to retry here, because a re-open would either hand
   * the run back (if that other driver has since gone quiet, in which case
   * the NEXT invoke succeeds on its own) or take it from whoever holds it now,
   * which is the same fight this refusal exists to avoid having.
   */
  private oneDriverError(cause: unknown): SalvorMiddlewareError {
    return new SalvorMiddlewareError(
      `thread \`${this.threadId}\` (run ${this.runId}) is no longer this invoke's to ` +
        "drive: another driver holds its lease now. One driver per thread at a time. " +
        "Invoke a given thread id from one process at a time, and give work that must " +
        "run alongside it a thread id of its own.",
      { code: "lease_lost", cause },
    );
  }

  /**
   * Open the run again, and rebuild what this tape knows of the log from what
   * comes back. The cursor is deliberately left where it is: the step that was
   * refused still belongs at the position it was given, and re-opening changes
   * who holds the lease, not where this invoke had got to.
   */
  private async reopen(cause: unknown): Promise<void> {
    let driver: ClientRunDriver;
    try {
      driver = await this.reopenRun();
    } catch (error) {
      const why = error instanceof SalvorApiError ? error.message : String(error);
      throw new SalvorMiddlewareError(
        `run ${this.runId} (thread \`${this.threadId}\`) refused this invoke's drive ` +
          `token (${describe(cause)}), and re-opening the run was refused too: ` +
          `${why}. The server refuses to re-open a run when it is not marked as ` +
          "client-driven in its log, or when the store no longer holds it.",
        { code: "reopen_refused", cause: error },
      );
    }
    this.driver = driver;
    this.recorded = new Map(driver.logEnvelopes.map((event) => [event.seq, event]));
    this.recordedLength = driver.logEnvelopes.length;
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
              `first (\`salvor resolve ${this.runId} --store <the server's store> ` +
              `--output '<json the call returned>'\`, or \`POST /v1/runs/${this.runId}/resolve\` ` +
              "on the server) and invoke again.",
            { code: "open_intent" },
          );
        }
        this.forked = this.cursor;
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
    const tail = await this.lease(() => this.driver.log(seq));
    const completion = tail[0];
    if (completion?.seq !== seq || completion.kind !== "ToolCallCompleted") {
      throw new SalvorMiddlewareError(
        `run ${this.runId} reports the tool call at seq ${seq - 1} settled, but ` +
          `seq ${seq} holds no completion to replay.`,
        { code: "unreadable_record" },
      );
    }
    return completion.payload.output;
  }
}

/**
 * Whether the server refused this step because another driver holds this
 * run's lease right now, live: `invalid_drive_token`, the token this tape
 * presented is no longer the current one. This is the one-driver case, and it
 * is never worth a re-open: whoever holds the lease is driving the run this
 * instant, and there is no order in which two live drivers can both be right
 * about what comes next.
 */
function isSupersededLease(error: unknown): boolean {
  return error instanceof SalvorApiError && error.code === "invalid_drive_token";
}

/**
 * Whether the server refused this step because it has forgotten the run
 * entirely: `unknown_run` on a run this tape itself opened. Leases live only
 * in the server's memory, so this is what a restart looks like from here, not
 * a live competitor; it is the one case worth taking the run up again for.
 */
function isForgottenRun(error: unknown): boolean {
  return error instanceof SalvorApiError && error.code === "unknown_run";
}

/** The refusal's own token, for an error message that quotes what was said. */
function describe(error: unknown): string {
  return error instanceof SalvorApiError ? error.code : String(error);
}
