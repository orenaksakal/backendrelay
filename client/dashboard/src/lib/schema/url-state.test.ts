import { describe, expect, it } from 'vitest';
import {
  SCHEMA_TABS,
  isSchemaTab,
  schemaSearchFor,
  schemaSearchSchema,
  schemaTabSearch,
} from './url-state';

describe('schema url state', () => {
  it('defaults to the attributes tab and rejects unknown tabs', () => {
    expect(schemaSearchSchema.parse({})).toEqual({ tab: 'attributes' });
    expect(schemaSearchSchema.parse({ tab: 'links' })).toEqual({
      tab: 'links',
    });
    expect(schemaSearchSchema.parse({ tab: 'nope' })).toEqual({
      tab: 'attributes',
    });
    for (const tab of SCHEMA_TABS) {
      expect(isSchemaTab(tab)).toBe(true);
    }
    expect(isSchemaTab('settings')).toBe(false);
  });

  it('keeps a namespace and drops blank ones', () => {
    expect(schemaSearchSchema.parse({ ns: 'todos' })).toEqual({
      ns: 'todos',
      tab: 'attributes',
    });
    expect(schemaSearchSchema.parse({ ns: '   ' })).toEqual({
      tab: 'attributes',
    });
  });

  it('round-trips the view state', () => {
    const search = schemaSearchFor('todos', 'jobs');
    expect(search).toEqual({ ns: 'todos', tab: 'jobs' });
    expect(schemaTabSearch(search, 'deleted')).toEqual({
      ns: 'todos',
      tab: 'deleted',
    });
    expect(schemaSearchFor(null, 'code')).toEqual({ tab: 'code' });
  });
});
