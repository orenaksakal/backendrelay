import type { JsonSchemaDocument } from '@/components/code-editor/code-editor';

/**
 * C.10 JSON view: the JSON schema behind `json-with-schema` lint, completion
 * and hover. Ported from the old dashboard's `lib/permsJsonSchema.tsx`; known
 * namespaces get their own block, while pattern properties keep unknown
 * namespaces valid, and `$rateLimits` keeps its numeric shape.
 */

type JsonStringSchema = { type: 'string'; enum?: string[] };

type JsonNumberSchema = { type: 'number' };

type JsonIntegerSchema = { type: 'integer' };

type JsonArraySchema = {
  type: 'array';
  items: JsonSchema;
  minItems?: number;
};

type JsonObjectSchema = {
  type: 'object';
  properties?: Record<string, JsonSchema>;
  required?: string[];
  patternProperties?: Record<string, JsonSchema>;
  additionalProperties?: boolean | JsonSchema;
};

type JsonOneOfSchema = {
  oneOf: JsonSchema[];
};

type JsonSchema =
  | JsonStringSchema
  | JsonNumberSchema
  | JsonIntegerSchema
  | JsonArraySchema
  | JsonObjectSchema
  | JsonOneOfSchema;

const FIELD_NAME_PATTERN = '^(?!id$)[$a-zA-Z0-9_\\-]+$';
const NAMESPACE_PATTERN = '^(?!\\$rateLimits$)[$a-zA-Z0-9_\\-]+$';
const RESERVED_NAMESPACES = new Set(['$default', '$rateLimits', 'attrs']);

function makeRuleBlock({
  includeFields,
}: {
  includeFields: boolean;
}): JsonObjectSchema {
  const block: JsonObjectSchema = {
    type: 'object',
    properties: {
      allow: {
        type: 'object',
        properties: {
          create: { type: 'string' },
          update: { type: 'string' },
          delete: { type: 'string' },
          view: { type: 'string' },
          $default: { type: 'string' },
        },
        additionalProperties: false,
      },
      bind: {
        oneOf: [
          {
            type: 'array',
            items: { type: 'string' },
            minItems: 2,
          },
          {
            type: 'object',
            additionalProperties: { type: 'string' },
          },
        ],
      },
      fields: {
        type: 'object',
        patternProperties: { [FIELD_NAME_PATTERN]: { type: 'string' } },
        additionalProperties: false,
      },
    },
    additionalProperties: false,
  };
  if (!includeFields && block.properties !== undefined) {
    delete block.properties['fields'];
  }
  return block;
}

function rateLimitSchema(): JsonObjectSchema {
  const refill: JsonObjectSchema = {
    type: 'object',
    properties: {
      amount: { type: 'integer' },
      period: { type: 'string' },
      type: { type: 'string', enum: ['interval', 'greedy'] },
    },
    additionalProperties: false,
  };
  const limit: JsonObjectSchema = {
    type: 'object',
    properties: {
      capacity: { type: 'integer' },
      refill,
    },
    required: ['capacity'],
    additionalProperties: false,
  };
  return {
    type: 'object',
    properties: {
      limits: {
        type: 'array',
        items: limit,
        minItems: 1,
      },
    },
    required: ['limits'],
    additionalProperties: false,
  };
}

export function permissionsJsonSchema(
  namespaces: readonly string[] = [],
): JsonSchemaDocument {
  const properties: Record<string, JsonSchema> = {
    $default: makeRuleBlock({ includeFields: false }),
    attrs: makeRuleBlock({ includeFields: false }),
    $rateLimits: {
      type: 'object',
      additionalProperties: rateLimitSchema(),
    },
  };

  for (const namespace of namespaces) {
    if (!RESERVED_NAMESPACES.has(namespace)) {
      properties[namespace] = makeRuleBlock({ includeFields: true });
    }
  }

  const schema: JsonObjectSchema = {
    type: 'object',
    properties,
    patternProperties: {
      [NAMESPACE_PATTERN]: makeRuleBlock({ includeFields: true }),
    },
    additionalProperties: false,
  };
  return schema;
}
