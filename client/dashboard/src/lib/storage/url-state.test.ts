import { describe, expect, it } from 'vitest';
import { storageSearchFor, storageSearchSchema } from './url-state';

describe('storage URL state', () => {
  it('defaults to the root with no search or sheet', () => {
    const search = storageSearchSchema.parse({});
    expect(search.prefix).toBeUndefined();
    expect(search.q).toBeUndefined();
    expect(search.file).toBeUndefined();
  });

  it('trims the search and the open sheet', () => {
    expect(storageSearchSchema.parse({ q: '  cat  ' }).q).toBe('cat');
    expect(storageSearchSchema.parse({ q: '   ' }).q).toBeUndefined();
    expect(storageSearchSchema.parse({ file: ' file-1 ' }).file).toBe('file-1');
  });

  it('writes a canonical prefix and drops empty values', () => {
    expect(
      storageSearchFor({ prefix: '/avatars/2026', q: ' cat ', file: 'file-1' }),
    ).toEqual({
      prefix: 'avatars/2026/',
      q: 'cat',
      file: 'file-1',
    });
    expect(storageSearchFor({ prefix: '/', q: '  ', file: null })).toEqual({
      prefix: undefined,
      q: undefined,
      file: undefined,
    });
  });
});
