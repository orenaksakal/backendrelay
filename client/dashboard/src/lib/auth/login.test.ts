import { describe, expect, it } from 'vitest';
import { ApiError } from '@/lib/api/client';
import {
  MAGIC_CODE_LENGTH,
  normalizeMagicCode,
  oauthStartUrl,
  postSignInTarget,
  safeRedirectPath,
  sendCodeErrorMessage,
  verifyCodeErrorMessage,
} from './login';

const config = { apiBaseUrl: 'http://localhost:8888' };

function apiError(type: string, message: string, hint?: unknown): ApiError {
  return new ApiError({ status: 400, type, message, hint });
}

describe('safeRedirectPath', () => {
  it('keeps same-origin paths', () => {
    expect(safeRedirectPath('/apps/app-1/explorer?ns=todos')).toBe(
      '/apps/app-1/explorer?ns=todos',
    );
  });

  it('falls back to / for missing, absolute and protocol-relative targets', () => {
    expect(safeRedirectPath(undefined)).toBe('/');
    expect(safeRedirectPath('')).toBe('/');
    expect(safeRedirectPath('https://evil.test/apps')).toBe('/');
    expect(safeRedirectPath('//evil.test/apps')).toBe('/');
    expect(safeRedirectPath('/\\evil.test')).toBe('/');
  });
});

describe('postSignInTarget', () => {
  it('approves a pending CLI ticket before following the redirect', () => {
    expect(
      postSignInTarget({
        redirect: '/apps/app-1/overview',
        ticket: 'ticket-1',
      }),
    ).toBe('/cli/approve?ticket=ticket-1');
  });

  it('follows the redirect when there is no ticket', () => {
    expect(postSignInTarget({ redirect: '/apps/app-1/overview' })).toBe(
      '/apps/app-1/overview',
    );
    expect(postSignInTarget({})).toBe('/');
  });
});

describe('oauthStartUrl', () => {
  it('points at the management origin with the redirect and ticket', () => {
    const url = new URL(
      oauthStartUrl(config, {
        redirect: '/apps/app-1/overview',
        ticket: 'ticket-1',
      }),
    );

    expect(url.origin).toBe('http://localhost:8888');
    expect(url.pathname).toBe('/dash/oauth/start');
    expect(url.searchParams.get('redirect_path')).toBe('/apps/app-1/overview');
    expect(url.searchParams.get('ticket')).toBe('ticket-1');
  });

  it('drops an unsafe redirect before it reaches the server', () => {
    const url = new URL(
      oauthStartUrl(config, { redirect: 'https://evil.test/apps' }),
    );

    expect(url.searchParams.get('redirect_path')).toBe('/');
  });
});

describe('normalizeMagicCode', () => {
  it('keeps the first six digits and drops separators', () => {
    expect(MAGIC_CODE_LENGTH).toBe(6);
    expect(normalizeMagicCode('123 456')).toBe('123456');
    expect(normalizeMagicCode('12-34-56-78')).toBe('123456');
    expect(normalizeMagicCode('abc')).toBe('');
  });
});

describe('sign-in error messages', () => {
  it('maps the send-code types the plan names', () => {
    expect(sendCodeErrorMessage(apiError('signups-closed', 'closed'))).toBe(
      'Sign-ups are closed on this deployment. Ask the operator for access.',
    );
    expect(sendCodeErrorMessage(apiError('rate-limited', 'slow down'))).toBe(
      'Too many attempts. Try again in a few minutes.',
    );
    expect(
      sendCodeErrorMessage(
        apiError('validation-failed', 'invalid', {
          errors: [{ message: 'Enter a valid email.' }],
        }),
      ),
    ).toBe('Enter a valid email.');
    expect(
      sendCodeErrorMessage(apiError('email-send-failed', 'SMTP down')),
    ).toBe('SMTP down');
    expect(sendCodeErrorMessage(new Error('boom'))).toBe(
      "Couldn't send the code. Try again.",
    );
  });

  it('maps the verify-code types the plan names', () => {
    expect(verifyCodeErrorMessage(apiError('record-expired', 'gone'))).toBe(
      'That code expired. Send a new one.',
    );
    expect(
      verifyCodeErrorMessage(apiError('record-not-found', 'missing')),
    ).toBe("That code isn't valid.");
    expect(
      verifyCodeErrorMessage(apiError('param-malformed', 'bad code')),
    ).toBe("That code isn't valid.");
    expect(verifyCodeErrorMessage(apiError('rate-limited', 'slow down'))).toBe(
      'Too many attempts. Try again in a few minutes.',
    );
    expect(verifyCodeErrorMessage(apiError('unknown', 'Server exploded'))).toBe(
      'Server exploded',
    );
    expect(verifyCodeErrorMessage(new Error('boom'))).toBe(
      "Couldn't verify the code. Try again.",
    );
  });
});
