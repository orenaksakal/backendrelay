import { describe, expect, it } from 'vitest';
import { permissionsJsonSchema } from './schema';

interface RuleBlock {
  properties?: Record<string, unknown>;
  additionalProperties?: boolean;
}

interface SchemaShape {
  properties: Record<string, RuleBlock>;
  patternProperties: Record<string, RuleBlock>;
  additionalProperties: boolean;
}

function shape(namespaces: readonly string[]): SchemaShape {
  return permissionsJsonSchema(namespaces) as unknown as SchemaShape;
}

describe('permissions JSON schema', () => {
  it('describes the reserved blocks and known namespaces', () => {
    const schema = shape(['todos', 'goals']);

    expect(Object.keys(schema.properties)).toEqual(
      expect.arrayContaining([
        '$default',
        'attrs',
        '$rateLimits',
        'todos',
        'goals',
      ]),
    );
    expect(schema.additionalProperties).toBe(false);
    expect(
      schema.patternProperties['^(?!\\$rateLimits$)[$a-zA-Z0-9_\\-]+$'],
    ).toBeDefined();
  });

  it('gives user namespaces field rules and keeps $default without them', () => {
    const schema = shape(['todos']);

    expect(schema.properties['todos']?.properties?.['fields']).toBeDefined();
    expect(
      schema.properties['$default']?.properties?.['fields'],
    ).toBeUndefined();
    expect(schema.properties['todos']?.properties?.['allow']).toBeDefined();
  });

  it('does not duplicate reserved names passed as namespaces', () => {
    const schema = shape(['$default', '$rateLimits', 'attrs', 'todos']);
    const userBlock = schema.properties['todos'];

    expect(userBlock?.properties?.['allow']).toBeDefined();
    expect(schema.properties['$rateLimits']?.properties).toBeUndefined();
    expect(schema.properties['attrs']?.properties?.['fields']).toBeUndefined();
  });
});
