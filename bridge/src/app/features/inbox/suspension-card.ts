import { Component, computed, effect, inject, input, output, signal } from '@angular/core';
import type { RunSummary } from '@salvor-run/client';

import { GraphRunService, GraphsService, RunDetailService, SALVOR_CLIENT, errorMessage } from '../../core/api';
import { ViewService } from '../../core/view';
import { jsonHi } from '../../shared/json-hi';
import {
  type NextNode,
  type ReceiptVM,
  buildReceipt,
  collectLog,
  lastAssistantText,
  lastToolResult,
  nextNodesAfter,
  shortId,
} from './inbox-model';
import { AbandonAction } from './abandon-action';
import { RunRef } from './run-ref';
import {
  type JsonSchemaObject,
  type SchemaField,
  defaultControlValue,
  extractSchemaInput,
  hasProposedValues,
  isGeneratableSchema,
  schemaFields,
} from './schema-form';

/**
 * SuspensionCard: `status.state === 'suspended'`. The whole form is GENERATED from the run's own
 * recorded `Suspended.input_schema` (see {@link schemaFields}) with a raw-JSON textarea fallback
 * for a schema this generator cannot turn into fields (no `properties`, or not an object at all).
 * Commits through `RunDetailService.resume`, the receipt pattern via {@link buildReceipt}.
 */
@Component({
  selector: 'bridge-suspension-card',
  imports: [RunRef, AbandonAction],
  templateUrl: './suspension-card.html',
})
export class SuspensionCard {
  private readonly client = inject(SALVOR_CLIENT);
  private readonly runDetail = inject(RunDetailService);
  private readonly viewService = inject(ViewService);
  private readonly graphRuns = inject(GraphRunService);
  private readonly graphsService = inject(GraphsService);

  readonly row = input.required<RunSummary>();
  /** Whether this card's Evidence is the one open in the parent's panel (drives aria-pressed). */
  readonly evidencePressed = input(false);
  readonly announce = output<string>();
  readonly committed = output<void>();
  /** READ, never act: asks the parent to show this run's recorded evidence in the side panel. */
  readonly evidence = output<void>();
  /** Forwarded from the embedded (secondary) abandon-action's receipt "Done": an abandon here
   * retires the run instead of resuming it, so the parent folds this whole card away, the same
   * treatment the Stalled card's abandon receipt gets (see stalled-card.ts's EXIT note). The card's
   * OWN resume receipt is untouched: it keeps its existing, permanent `.committed` treatment. */
  readonly retire = output<void>();

  readonly ns = computed(() => shortId(this.row().run));
  readonly reason = computed(() => this.row().status.reason ?? '');
  readonly schema = computed<JsonSchemaObject | undefined>(() => {
    const s = this.row().status.inputSchema;
    return isGeneratableSchema(s) ? s : undefined;
  });
  readonly title = computed(() => this.schema()?.title || 'Suspended, awaiting input');
  readonly fields = computed<SchemaField[]>(() => {
    const schema = this.schema();
    return schema ? schemaFields(schema, this.ns()) : [];
  });
  readonly showProvenance = computed(() => hasProposedValues(this.fields()));
  readonly usesRawJson = computed(() => this.schema() === undefined);

  /** Only the fields a person has actually EDITED, never pre-seeded from the schema's defaults in
   * the constructor, because a signal input's value is not guaranteed readable that early. An
   * untouched field reads its schema default lazily, via {@link valueOf}. */
  private readonly edited = signal<Record<string, string>>({});
  readonly rawJson = signal('');
  readonly submitError = signal<string | undefined>(undefined);
  readonly submitting = signal(false);
  readonly receipt = signal<ReceiptVM | undefined>(undefined);

  readonly endpoint = computed(() => `POST /v1/runs/${this.ns()}…/resume`);

  // ── GATE DECISION EVIDENCE ──
  // A gate asks true/false; these carry the run's recent substance to the decision point, read off
  // its own log (bounded excerpts, with an honest "from seq N" provenance), plus what a resume
  // continues into (the graph projection's next nodes). Undefined until loaded; absent honestly for
  // an agent run (no projection), which gets the last-events excerpt alone, per the brief.
  readonly lastText = signal<{ readonly text: string; readonly seq: number } | undefined>(undefined);
  readonly lastTool = signal<{ readonly tool: string; readonly output: unknown; readonly seq: number } | undefined>(undefined);
  readonly continuesInto = signal<readonly NextNode[] | undefined>(undefined);
  readonly evidenceLoaded = signal(false);
  readonly hasEvidence = computed(() => !!this.lastText() || !!this.lastTool());

  constructor() {
    // Earn the cost when the Inbox is actually looked at. Every view is mounted from boot, so
    // loading evidence in a constructor/ngOnInit would stream a log per suspended run on every page
    // load, for a view nobody may open: the same "gate the fold on the active view" rule Spend
    // states. Once loaded it never re-fetches (evidenceLoaded latches).
    effect(() => {
      if (this.viewService.view() !== 'inbox' || this.evidenceLoaded()) return;
      void this.loadEvidence();
    });
  }

  /** Read the run's recent substance off its own log, and (for a graph run parked at a gate) what
   * resuming continues into. Evidence is a courtesy at the decision point; every failure is
   * swallowed so the form still works without it (an agent run simply gets the excerpt, no next
   * nodes). Bounded: {@link collectLog} stops at the recorded head, never holding the live stream. */
  private async loadEvidence(): Promise<void> {
    try {
      const row = this.row();
      const events = await collectLog(this.client, row.run, 0, row.eventCount);
      this.lastText.set(lastAssistantText(events));
      this.lastTool.set(lastToolResult(events));
      // A graph run's log opens with GraphRunStarted; only then is there a projection to ask what
      // the resume continues into. An agent run gets the excerpt alone.
      if (events[0]?.kind === 'GraphRunStarted') {
        const proj = await this.graphRuns.loadProjection(row.run);
        if (proj.currentNode && proj.graphHash) {
          const rec = await this.graphsService.get(proj.graphHash);
          this.continuesInto.set(nextNodesAfter(rec.document, proj.currentNode));
        }
      }
    } catch {
      /* evidence is a courtesy: the form works without it */
    } finally {
      this.evidenceLoaded.set(true);
    }
  }

  /** A bounded excerpt for the evidence card: the full text stays available on expand. */
  excerpt(text: string, max = 240): string {
    const t = text.trim();
    return t.length > max ? t.slice(0, max).trimEnd() + '…' : t;
  }
  isLong(text: string, max = 240): boolean {
    return text.trim().length > max;
  }
  nextNodeLabel(n: NextNode): string {
    return n.kind ? `${n.name} (${n.kind})` : n.name;
  }

  /** The control's current value: what was typed/chosen, or the schema's own proposed default. */
  valueOf(f: SchemaField): string {
    return this.edited()[f.key] ?? defaultControlValue(f);
  }

  setValue(key: string, value: string): void {
    this.edited.update((v) => ({ ...v, [key]: value }));
  }

  bounds(f: SchemaField): string | null {
    return f.bounds ?? null;
  }

  async submit(): Promise<void> {
    if (this.submitting()) return;
    let resumeInput: unknown;
    if (this.usesRawJson()) {
      const text = this.rawJson().trim();
      if (!text) {
        this.submitError.set('Paste the resume input as JSON, per the recorded reason above.');
        return;
      }
      try {
        resumeInput = JSON.parse(text);
      } catch (ex) {
        this.submitError.set(`Not valid JSON: ${errorMessage(ex)}`);
        return;
      }
    } else {
      const values: Record<string, string> = {};
      for (const f of this.fields()) values[f.key] = this.valueOf(f);
      resumeInput = extractSchemaInput(this.fields(), values);
    }
    this.submitError.set(undefined);
    this.submitting.set(true);
    const runId = this.row().run;
    const beforeCount = this.row().eventCount;
    try {
      await this.runDetail.resume(runId, resumeInput);
      const r = await buildReceipt(
        this.client,
        runId,
        beforeCount,
        'Resumed',
        { input: resumeInput },
        this.endpoint(),
      );
      this.receipt.set(r);
      this.announce.emit(
        `Resumed run ${this.ns()}. Appended at sequence ${r.seq ?? '-'}. Status now ${r.statusState}.`,
      );
      this.committed.emit();
    } catch (ex) {
      this.submitError.set(errorMessage(ex));
    } finally {
      this.submitting.set(false);
    }
  }

  jsonHiCompact(value: unknown): string {
    return jsonHi(value);
  }

  openTimeline(): void {
    this.viewService.openRun(this.row().run);
  }
}
