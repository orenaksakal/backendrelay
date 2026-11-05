import { describe, expect, it } from 'vitest';
import {
  PERMISSION_VIEWS,
  permissionsSearchSchema,
  permissionsTestSearch,
  permissionsViewSearch,
} from './url-state';

describe('permissions URL state', () => {
  it('defaults to the Editor view', () => {
    expect(permissionsSearchSchema.parse({})).toEqual({ view: 'editor' });
  });

  it('accepts every documented view', () => {
    for (const view of PERMISSION_VIEWS) {
      expect(permissionsSearchSchema.parse({ view })).toEqual({ view });
    }
  });

  it('reads the namespace and Test-as parameters', () => {
    expect(permissionsSearchSchema.parse({ ns: 'todos', test: '1' })).toEqual({
      view: 'editor',
      ns: 'todos',
      test: '1',
    });
    expect(permissionsSearchSchema.parse({ test: 1 })).toEqual({
      view: 'editor',
      test: 1,
    });
    const closed = permissionsSearchSchema.parse({ test: '0' });
    expect(closed.view).toBe('editor');
    expect(closed.test).toBeUndefined();
  });

  it('opens and closes the Test-as parameter without losing the view', () => {
    const opened = permissionsTestSearch({ view: 'json', ns: 'goals' }, true);
    expect(opened).toEqual({ view: 'json', ns: 'goals', test: '1' });
    expect(permissionsTestSearch(opened, false)).toEqual({
      view: 'json',
      ns: 'goals',
    });
  });

  it('falls back to the Editor when a hand-edited URL is unknown', () => {
    expect(permissionsSearchSchema.parse({ view: 'json5' })).toEqual({
      view: 'editor',
    });
    expect(permissionsViewSearch('history')).toEqual({ view: 'history' });
  });
});
