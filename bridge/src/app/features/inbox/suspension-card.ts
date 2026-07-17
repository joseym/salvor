import { Component, computed, inject, input, output, signal } from '@angular/core';
import type { RunSummary } from '@salvor/client';

import { RunDetailService, SALVOR_CLIENT, errorMessage } from '../../core/api';
import { ViewService } from '../../core/view';
import { jsonHi } from './json-highlight';
import { type ReceiptVM, buildReceipt, shortId } from './inbox-model';
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
 * recorded `Suspended.input_schema` — see {@link schemaFields} — with a raw-JSON textarea fallback
 * for a schema this generator cannot turn into fields (no `properties`, or not an object at all).
 * Commits through `RunDetailService.resume`, the receipt pattern via {@link buildReceipt}.
 */
@Component({
  selector: 'bridge-suspension-card',
  imports: [RunRef],
  templateUrl: './suspension-card.html',
})
export class SuspensionCard {
  private readonly client = inject(SALVOR_CLIENT);
  private readonly runDetail = inject(RunDetailService);
  private readonly viewService = inject(ViewService);

  readonly row = input.required<RunSummary>();
  /** Whether this card's Evidence is the one open in the parent's panel (drives aria-pressed). */
  readonly evidencePressed = input(false);
  readonly announce = output<string>();
  readonly committed = output<void>();
  /** READ, never act: asks the parent to show this run's recorded evidence in the side panel. */
  readonly evidence = output<void>();

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

  /** Only the fields a person has actually EDITED — never pre-seeded from the schema's defaults in
   * the constructor, because a signal input's value is not guaranteed readable that early. An
   * untouched field reads its schema default lazily, via {@link valueOf}. */
  private readonly edited = signal<Record<string, string>>({});
  readonly rawJson = signal('');
  readonly submitError = signal<string | undefined>(undefined);
  readonly submitting = signal(false);
  readonly receipt = signal<ReceiptVM | undefined>(undefined);

  readonly endpoint = computed(() => `POST /v1/runs/${this.ns()}…/resume`);

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
        this.submitError.set(`Not valid JSON — ${errorMessage(ex)}`);
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
        `Resumed run ${this.ns()}. Appended at sequence ${r.seq ?? '—'}. Status now ${r.statusState}.`,
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
