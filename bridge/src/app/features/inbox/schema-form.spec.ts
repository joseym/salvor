import {
  type JsonSchemaObject,
  defaultControlValue,
  extractSchemaInput,
  hasProposedValues,
  isGeneratableSchema,
  schemaFields,
} from './schema-form';

const APPROVAL_SCHEMA: JsonSchemaObject = {
  title: 'Approve the refund',
  required: ['approved'],
  properties: {
    approved: { type: 'boolean', title: 'Approved' },
    note: { type: 'string', title: 'Note' },
  },
};

describe('isGeneratableSchema', () => {
  it('accepts an object schema with properties', () => {
    expect(isGeneratableSchema(APPROVAL_SCHEMA)).toBe(true);
  });

  it('rejects a schema with no properties, an array, or a non-object value — the raw-JSON fallback trigger', () => {
    expect(isGeneratableSchema({ type: 'string' })).toBe(false);
    expect(isGeneratableSchema(undefined)).toBe(false);
    expect(isGeneratableSchema(null)).toBe(false);
    expect(isGeneratableSchema([])).toBe(false);
    expect(isGeneratableSchema('not a schema')).toBe(false);
  });
});

describe('schemaFields', () => {
  it('never hand-writes a field: every property in the schema becomes exactly one field, in order', () => {
    const fields = schemaFields(APPROVAL_SCHEMA, 'abcd1234');
    expect(fields.map((f) => f.key)).toEqual(['approved', 'note']);
    expect(fields[0]).toMatchObject({ id: 'abcd1234-approved', kind: 'boolean', required: true, label: 'Approved' });
    expect(fields[1]).toMatchObject({ id: 'abcd1234-note', kind: 'text', required: false, label: 'Note' });
  });

  it('classifies field kinds: boolean, enum, number/integer, long text as textarea, else text', () => {
    const schema: JsonSchemaObject = {
      properties: {
        b: { type: 'boolean' },
        e: { type: 'string', enum: ['a', 'b'] },
        n: { type: 'number' },
        i: { type: 'integer' },
        long: { type: 'string', maxLength: 500 },
        short: { type: 'string', maxLength: 40 },
        plain: {},
      },
    };
    const kinds = Object.fromEntries(schemaFields(schema, 'ns').map((f) => [f.key, f.kind]));
    expect(kinds).toEqual({
      b: 'boolean',
      e: 'enum',
      n: 'number',
      i: 'number',
      long: 'textarea',
      short: 'text',
      plain: 'text',
    });
  });

  it('carries min/max/maxLength bounds and a human-readable bounds hint', () => {
    const schema: JsonSchemaObject = {
      properties: {
        pct: { type: 'number', minimum: 0, maximum: 100 },
        note: { type: 'string', maxLength: 240 },
      },
    };
    const [pct, note] = schemaFields(schema, 'ns');
    expect(pct!.bounds).toBe('0–100');
    expect(note!.bounds).toBe('≤ 240 characters');
  });

  it('marks a field PROPOSED (not a neutral default) only when the schema carries a default', () => {
    const schema: JsonSchemaObject = {
      properties: {
        withDefault: { type: 'string', default: 'guessed-by-run' },
        withoutDefault: { type: 'string' },
      },
    };
    const fields = schemaFields(schema, 'ns');
    expect(fields[0]!.proposed).toBe(true);
    expect(fields[0]!.defaultValue).toBe('guessed-by-run');
    expect(fields[1]!.proposed).toBe(false);
    expect(hasProposedValues(fields)).toBe(true);
    expect(hasProposedValues(schemaFields({ properties: { x: { type: 'string' } } }, 'ns'))).toBe(false);
  });

  it('defaultControlValue stringifies a schema default, or empty string when there is none', () => {
    const schema: JsonSchemaObject = { properties: { n: { type: 'number', default: 3 }, s: { type: 'string' } } };
    const [n, s] = schemaFields(schema, 'ns');
    expect(defaultControlValue(n!)).toBe('3');
    expect(defaultControlValue(s!)).toBe('');
  });
});

describe('extractSchemaInput', () => {
  it('coerces booleans and numbers, keeps strings as-is, and omits untouched/blank fields', () => {
    const fields = schemaFields(
      {
        properties: {
          approved: { type: 'boolean' },
          note: { type: 'string' },
          count: { type: 'number' },
          untouched: { type: 'string' },
        },
      },
      'ns',
    );
    const input = extractSchemaInput(fields, {
      approved: 'true',
      note: 'looks good',
      count: '3.5',
      untouched: '',
    });
    expect(input).toEqual({ approved: true, note: 'looks good', count: 3.5 });
    expect(input).not.toHaveProperty('untouched');
  });

  it('round-trips the canonical approval schema (approved: boolean, note: string)', () => {
    const fields = schemaFields(APPROVAL_SCHEMA, 'ns');
    expect(extractSchemaInput(fields, { approved: 'false' })).toEqual({ approved: false });
  });
});
