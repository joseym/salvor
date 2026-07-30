/**
 * Suspension forms are GENERATED from a run's own recorded `Suspended.input_schema`, never
 * hand-written per agent, since a hand-written form drifts from whatever schema an agent actually
 * declares. This module is the generator: a pure, unit-tested mapping from a JSON Schema object to
 * the field descriptors the suspension-card template renders, plus the inverse: reading the
 * submitted control values back into the typed input object `resume` expects.
 */

/** The shape of one JSON Schema property this generator understands. Anything else (a `$ref`, an
 * `anyOf`, a nested `object`/`array`) is exactly what the raw-JSON fallback exists for. */
export interface JsonSchemaProperty {
  readonly type?: string;
  readonly title?: string;
  readonly description?: string;
  readonly enum?: readonly string[];
  readonly default?: unknown;
  readonly minimum?: number;
  readonly maximum?: number;
  readonly maxLength?: number;
}

export interface JsonSchemaObject {
  readonly title?: string;
  readonly properties: Readonly<Record<string, JsonSchemaProperty>>;
  readonly required?: readonly string[];
}

/** True when `schema` has an object shape this generator can turn into real fields. A schema that
 * fails this check (no `properties`, or not an object at all) falls back to a raw-JSON textarea;
 * the form is never fabricated from a schema the generator cannot actually read. */
export function isGeneratableSchema(schema: unknown): schema is JsonSchemaObject {
  if (!schema || typeof schema !== 'object') return false;
  const props = (schema as Record<string, unknown>)['properties'];
  return !!props && typeof props === 'object' && !Array.isArray(props);
}

export type FieldKind = 'boolean' | 'enum' | 'number' | 'textarea' | 'text';

export interface SchemaField {
  readonly key: string;
  readonly id: string;
  readonly label: string;
  readonly required: boolean;
  readonly description?: string;
  readonly kind: FieldKind;
  readonly enumOptions?: readonly string[];
  readonly min?: number;
  readonly max?: number;
  readonly maxLength?: number;
  /** A schema default is not a neutral default: it is what the RUN proposed. Confirming it is
   * part of the approval, not a blank being accepted; the template marks these "proposed by the
   * run" rather than silently prefilling them as if they were the form's own idea. */
  readonly defaultValue?: unknown;
  readonly proposed: boolean;
  /** A constraint hint rendered below the control (a numeric range, or a max character count). */
  readonly bounds?: string;
}

function fieldKind(f: JsonSchemaProperty): FieldKind {
  if (f.type === 'boolean') return 'boolean';
  if (f.enum && f.enum.length > 0) return 'enum';
  if (f.type === 'number' || f.type === 'integer') return 'number';
  if ((f.maxLength ?? 0) >= 200) return 'textarea';
  return 'text';
}

function fieldBounds(f: JsonSchemaProperty): string | undefined {
  if (f.maximum != null) return `${f.minimum ?? 0}-${f.maximum}`;
  if (f.maxLength != null) return `≤ ${f.maxLength} characters`;
  return undefined;
}

/** Every field the schema's `properties` declares, in the schema's own key order: the same order
 * `Object.entries` walks, so field order in the rendered form always matches the schema's own. */
export function schemaFields(schema: JsonSchemaObject, ns: string): SchemaField[] {
  const required = schema.required ?? [];
  return Object.entries(schema.properties).map(([key, f]) => ({
    key,
    id: `${ns}-${key}`,
    label: f.title || key,
    required: required.includes(key),
    description: f.description,
    kind: fieldKind(f),
    enumOptions: f.enum,
    min: f.minimum,
    max: f.maximum,
    maxLength: f.maxLength,
    defaultValue: f.default,
    proposed: f.default != null,
    bounds: fieldBounds(f),
  }));
}

/** True when at least one field in the schema carries a run-proposed default: the provenance
 * notice ("Prefilled values were proposed by this run…") only renders when this is true. */
export function hasProposedValues(fields: readonly SchemaField[]): boolean {
  return fields.some((f) => f.proposed);
}

/** The default form-control string value for a field: the schema default, stringified, or ''. */
export function defaultControlValue(f: SchemaField): string {
  if (f.defaultValue == null) return '';
  return String(f.defaultValue);
}

/**
 * Reads submitted control values (every control keyed by its field's plain `key`, string-typed:
 * the shape Angular's reactive forms give a text/number/select/radio control) back into the typed
 * `input` object `resume` expects: booleans and numbers are coerced, everything else stays a
 * string. A field left blank (and not required) is OMITTED, not sent as `""`; the schema never
 * hears about a field nobody touched.
 */
export function extractSchemaInput(
  fields: readonly SchemaField[],
  values: Readonly<Record<string, string>>,
): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const f of fields) {
    const raw = values[f.key];
    if (raw === undefined || raw === '') continue;
    if (f.kind === 'boolean') out[f.key] = raw === 'true';
    else if (f.kind === 'number') out[f.key] = Number(raw);
    else out[f.key] = raw;
  }
  return out;
}
