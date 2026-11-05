import { describe, expect, it } from 'vitest';
import { consentScopeDescriptions } from './oauth-scopes';

function ids(scopes: string[]): string[] {
  return consentScopeDescriptions(scopes).map((scope) => scope.id);
}

describe('consentScopeDescriptions', () => {
  it('describes read-only app access without the write entries', () => {
    expect(ids(['apps-read'])).toEqual(['apps-read']);
  });

  it('treats apps-write as read, write and create', () => {
    expect(ids(['apps-write'])).toEqual(['apps-write', 'apps-create']);
  });

  it('describes data and storage scopes at the level granted', () => {
    expect(ids(['data-read', 'storage-write'])).toEqual([
      'data-read',
      'storage-read',
      'storage-write',
    ]);
  });

  it('ignores scopes it does not know', () => {
    expect(ids(['unknown'])).toEqual([]);
  });
});
