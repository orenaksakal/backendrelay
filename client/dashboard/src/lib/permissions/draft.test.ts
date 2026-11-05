import { beforeEach, describe, expect, it } from 'vitest';
import {
  clearPermissionDraft,
  permissionDraftKey,
  readPermissionDraft,
  recoverDraft,
  writePermissionDraft,
} from './draft';

describe('permission drafts', () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  it('recovers a draft only when it differs from its saved baseline', () => {
    expect(recoverDraft('{bad', '{}')).toEqual({
      base: '{}',
      draft: '{}',
      recovered: false,
    });
    expect(
      recoverDraft(
        JSON.stringify({ base: '{}', draft: '{unfinished' }),
        '{"new":{}}',
      ),
    ).toEqual({ base: '{}', draft: '{unfinished', recovered: true });
    expect(
      recoverDraft(JSON.stringify({ base: 1, draft: '{}' }), '{}').recovered,
    ).toBe(false);
    expect(
      recoverDraft(JSON.stringify({ base: '{}', draft: '{}' }), '{}').recovered,
    ).toBe(false);
  });

  it('stores, reads and clears one draft per app', () => {
    expect(readPermissionDraft('app-1', '{}')).toEqual({
      base: '{}',
      draft: '{}',
      recovered: false,
    });

    writePermissionDraft('app-1', '{}', '{"todos":{}}');
    expect(readPermissionDraft('app-1', '{}')).toEqual({
      base: '{}',
      draft: '{"todos":{}}',
      recovered: true,
    });
    expect(window.localStorage.getItem(permissionDraftKey('app-1'))).not.toBe(
      null,
    );

    expect(readPermissionDraft('app-2', '{}').recovered).toBe(false);

    clearPermissionDraft('app-1');
    expect(readPermissionDraft('app-1', '{}').recovered).toBe(false);
  });
});
