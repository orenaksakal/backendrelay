import { describe, expect, it } from 'vitest';
import { playgroundSearchFor, playgroundSearchSchema } from './url-state';

describe('playground URL state', () => {
  it('defaults to a query as Admin', () => {
    expect(playgroundSearchSchema.parse({})).toEqual({
      mode: 'query',
      as: 'admin',
    });
  });

  it('keeps known values and falls back on hand-edited ones', () => {
    expect(
      playgroundSearchSchema.parse({ mode: 'transact', as: 'guest' }),
    ).toEqual({ mode: 'transact', as: 'guest' });
    expect(playgroundSearchSchema.parse({ mode: 'nope', as: 'root' })).toEqual({
      mode: 'query',
      as: 'admin',
    });
  });

  it('writes only the two route params', () => {
    expect(playgroundSearchFor({ mode: 'query', actor: 'token' })).toEqual({
      mode: 'query',
      as: 'token',
    });
  });
});
