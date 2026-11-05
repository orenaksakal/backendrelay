import { describe, expect, it } from 'vitest';
import { ApiError } from '@/lib/api/client';
import { authErrorMessage, oauthCallbackTarget } from './oauth';

describe('oauthCallbackTarget', () => {
  it('approves a pending CLI ticket before following redirect_path', () => {
    expect(oauthCallbackTarget('ticket 1', '/apps/app-1/overview')).toBe(
      '/cli/approve?ticket=ticket%201',
    );
  });

  it('follows a same-origin redirect_path', () => {
    expect(oauthCallbackTarget(undefined, '/apps/app-1/overview')).toBe(
      '/apps/app-1/overview',
    );
  });

  it('drops an absolute or missing redirect_path', () => {
    expect(oauthCallbackTarget(undefined, 'https://evil.test/apps')).toBe('/');
    expect(oauthCallbackTarget(undefined, undefined)).toBe('/');
  });
});

describe('authErrorMessage', () => {
  it('uses the server message when there is one', () => {
    expect(
      authErrorMessage(
        new ApiError({ status: 400, type: 'record-expired', message: 'Gone.' }),
        'fallback',
      ),
    ).toBe('Gone.');
  });

  it('falls back for transport errors and empty messages', () => {
    expect(authErrorMessage(new Error('boom'), 'fallback')).toBe('fallback');
    expect(
      authErrorMessage(
        new ApiError({ status: 500, type: 'unknown', message: '' }),
        'fallback',
      ),
    ).toBe('fallback');
  });
});
