import { describe, expect, it } from 'vitest';
import {
  addProviderSearch,
  appAuthSearchSchema,
  clientSheetSearch,
  closedSheetSearch,
} from './url-state';

describe('auth URL state', () => {
  it('defaults to the providers tab and falls back on bad input', () => {
    expect(appAuthSearchSchema.parse({})).toEqual({ tab: 'providers' });
    expect(appAuthSearchSchema.parse({ tab: 'nope' })).toEqual({
      tab: 'providers',
    });
    expect(appAuthSearchSchema.parse({ tab: 'origins' })).toEqual({
      tab: 'origins',
    });
  });

  it('keeps client and add drill-ins mutually exclusive', () => {
    const search = clientSheetSearch({ tab: 'origins' }, 'client-1');
    expect(search).toEqual({ tab: 'origins', client: 'client-1' });

    const add = addProviderSearch(search, 'google');
    expect(add).toEqual({ tab: 'origins', add: 'google' });

    expect(closedSheetSearch(add)).toEqual({ tab: 'origins' });
  });
});
