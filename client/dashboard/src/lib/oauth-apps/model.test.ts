import { describe, expect, it } from 'vitest';
import { mockOauthApp } from '@/test/fixtures/api/oauth-apps';
import {
  OAUTH_APP_LOGO_MAX_BYTES,
  canDeleteOAuthApps,
  canViewOAuthApps,
  cleanRedirectUrls,
  clientCount,
  clientNameProblem,
  logoProblem,
  oauthAppInfo,
  oauthAppInfoEqual,
  oauthAppInfoPatch,
  oauthAppInfoProblems,
  readFileAsDataUrl,
  redirectUrlProblem,
  redirectUrlsProblem,
} from './model';

describe('OAuth app roles', () => {
  it('needs the collaborator role to read and admin to delete', () => {
    expect(canViewOAuthApps('app-member')).toBe(false);
    expect(canViewOAuthApps('collaborator')).toBe(true);
    expect(canViewOAuthApps('owner')).toBe(true);
    expect(canDeleteOAuthApps('collaborator')).toBe(false);
    expect(canDeleteOAuthApps('admin')).toBe(true);
  });

  it('counts the clients', () => {
    expect(clientCount(mockOauthApp)).toBe(1);
    expect(clientCount({ clients: undefined })).toBe(0);
  });
});

describe('OAuth app info draft', () => {
  it('maps null fields to empty strings', () => {
    const info = oauthAppInfo({ ...mockOauthApp, appLogo: null });
    expect(info).toEqual({
      name: 'Acme Tools',
      logo: '',
      homePage: 'https://acme.test',
      privacyPolicy: 'https://acme.test/privacy',
      terms: 'https://acme.test/terms',
      supportEmail: 'support@acme.test',
    });
    expect(oauthAppInfoEqual(info, oauthAppInfo(mockOauthApp))).toBe(true);
  });

  it('sends only changed fields and nulls cleared ones', () => {
    const current = oauthAppInfo(mockOauthApp);
    const patch = oauthAppInfoPatch(
      {
        ...current,
        name: ' Acme Tools 2 ',
        homePage: 'https://acme.test/new',
        supportEmail: '',
        terms: current.terms,
      },
      current,
    );
    expect(patch).toEqual({
      app_name: 'Acme Tools 2',
      app_home_page: 'https://acme.test/new',
      support_email: null,
    });
    expect(oauthAppInfoPatch(current, current)).toEqual({});
  });

  it('validates the name, URLs and support email', () => {
    const current = oauthAppInfo(mockOauthApp);
    const problems = oauthAppInfoProblems({
      ...current,
      name: '   ',
      homePage: 'not a url',
      privacyPolicy: 'ftp://acme.test/privacy',
      supportEmail: 'support',
    });
    expect(problems).toEqual({
      name: 'Enter a name.',
      homePage: 'Enter an absolute http(s) URL.',
      privacyPolicy: 'Enter an absolute http(s) URL.',
      supportEmail: 'Enter a valid email address.',
    });
    expect(oauthAppInfoProblems(current)).toEqual({});
  });
});

describe('OAuth app logos', () => {
  it('accepts PNG and SVG up to 256 KB', () => {
    expect(logoProblem({ type: 'image/png', size: 1024 })).toBeUndefined();
    expect(logoProblem({ type: 'image/svg+xml', size: 1024 })).toBeUndefined();
    expect(
      logoProblem({ type: 'image/png', size: OAUTH_APP_LOGO_MAX_BYTES }),
    ).toBeUndefined();
    expect(
      logoProblem({ type: 'image/png', size: OAUTH_APP_LOGO_MAX_BYTES + 1 }),
    ).toBe('The logo must be 256 KB or smaller.');
    expect(logoProblem({ type: 'image/jpeg', size: 10 })).toBe(
      'Upload a PNG or SVG file.',
    );
  });

  it('reads an upload into a data URL', async () => {
    const file = new File(['<svg></svg>'], 'logo.svg', {
      type: 'image/svg+xml',
    });
    await expect(readFileAsDataUrl(file)).resolves.toMatch(
      /^data:image\/svg\+xml;base64,/,
    );
  });
});

describe('OAuth client names', () => {
  it('requires a non-empty name', () => {
    expect(clientNameProblem('Acme CLI')).toBeUndefined();
    expect(clientNameProblem('  ')).toBe('Enter a client name.');
  });
});

describe('redirect URLs', () => {
  it('accepts https and test-mode localhost', () => {
    expect(
      redirectUrlProblem('https://acme.test/callback', false),
    ).toBeUndefined();
    expect(
      redirectUrlProblem('http://localhost:3000/callback', true),
    ).toBeUndefined();
    expect(
      redirectUrlProblem('http://127.0.0.1:3000/callback', true),
    ).toBeUndefined();
  });

  it('explains the public-only rules', () => {
    expect(redirectUrlProblem('http://acme.test/cb', false)).toBe(
      'Use https for a public redirect URL.',
    );
    expect(redirectUrlProblem('http://localhost:3000/cb', false)).toBe(
      'Localhost redirects only work while the OAuth app is in test mode.',
    );
  });

  it('rejects credentials, fragments, traversal and relative URLs', () => {
    expect(redirectUrlProblem('https://u:p@acme.test/cb', false)).toBe(
      'Remove the username and password from the URL.',
    );
    expect(redirectUrlProblem('https://acme.test/cb#token', false)).toBe(
      'Remove the #fragment from the redirect URL.',
    );
    expect(redirectUrlProblem('https://acme.test/..%2fsecret', false)).toBe(
      'Remove the path traversal from the URL.',
    );
    expect(redirectUrlProblem('/callback', false)).toBe(
      'Enter an absolute URL, such as https://example.com/callback.',
    );
    expect(redirectUrlProblem('   ', false)).toBe('Enter a redirect URL.');
  });

  it('drops blank rows and reports the first problem', () => {
    expect(cleanRedirectUrls([' https://acme.test/a ', '', '  '])).toEqual([
      'https://acme.test/a',
    ]);
    expect(
      redirectUrlsProblem(
        ['https://acme.test/a', 'https://acme.test/a'],
        false,
      ),
    ).toBe('This redirect URL is already listed.');
    expect(redirectUrlsProblem(['', 'http://acme.test/cb'], false)).toBe(
      'Use https for a public redirect URL.',
    );
    expect(redirectUrlsProblem(['', '  '], false)).toBeUndefined();
  });
});
