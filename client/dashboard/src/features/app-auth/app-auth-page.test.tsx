import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { mockGoogleWebClient } from '@/test/fixtures/api/app-auth';
import { mockSenderVerification, mockTodoApp } from '@/test/fixtures/api/apps';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const API = 'http://localhost:8888';
const ROUTE = '/apps/app-1/auth';

vi.mock('@/components/code-editor/code-editor', () => ({
  CodeEditor: (props: {
    value: string;
    onChange?: (value: string) => void;
    mode: string;
    'aria-label': string;
    readOnly?: boolean;
  }) => (
    <textarea
      aria-label={props['aria-label']}
      value={props.value}
      readOnly={props.readOnly}
      onChange={(event) => {
        props.onChange?.(event.target.value);
      }}
    />
  ),
}));

beforeEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
  setSession({ token: mockSignInToken, user: mockSignInUser });
});

afterEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
});

async function renderAuth(route = ROUTE) {
  render({ route });
  await waitFor(() => {
    const heading = screen.queryByRole('heading', { level: 1, name: 'Auth' });
    const sheet = screen.queryByRole('dialog');
    expect(heading ?? sheet).not.toBeNull();
  });
}

describe('app auth page', () => {
  it('shows provider status cards and the client list', async () => {
    await renderAuth();

    expect(
      await screen.findByText('Configured · 2 clients'),
    ).toBeInTheDocument();
    expect(screen.getByText('Configured · 1 client')).toBeInTheDocument();
    expect(screen.getAllByText('Not configured')).toHaveLength(4);
    expect(
      screen.getByRole('button', { name: 'Configure Google' }),
    ).toBeInTheDocument();

    const table = await screen.findByRole('table', { name: 'OAuth clients' });
    expect(within(table).getByText('google-web')).toBeInTheDocument();
    expect(within(table).getByText('google-ios')).toBeInTheDocument();
    expect(within(table).getByText('github-web')).toBeInTheDocument();
    expect(within(table).getAllByText('Google')).toHaveLength(2);
    expect(within(table).getByText('GitHub')).toBeInTheDocument();
  });

  it('creates the provider and the client from the add sheet', async () => {
    const user = userEvent.setup();
    let providerBody: unknown;
    let clientBody: unknown;
    server.use(
      http.post(
        `${API}/dash/apps/app-1/oauth_service_providers`,
        async ({ request }) => {
          providerBody = await request.json();
          return HttpResponse.json({
            provider: {
              id: 'provider-apple',
              provider_name: 'apple',
              created_at: '2026-01-01T00:00:00Z',
            },
          });
        },
      ),
      http.post(`${API}/dash/apps/app-1/oauth_clients`, async ({ request }) => {
        clientBody = await request.json();
        return HttpResponse.json({
          client: {
            id: 'client-apple',
            provider_id: 'provider-apple',
            client_name: 'apple',
            client_id: 'com.example.app',
            extra_audiences: [],
            discovery_endpoint:
              'https://appleid.apple.com/.well-known/openid-configuration',
            redirect_to: null,
            meta: {},
            use_shared_credentials: false,
            created_at: 1_761_400_000_000,
          },
        });
      }),
    );

    await renderAuth(`${ROUTE}?add=apple`);
    const sheet = await screen.findByRole('dialog');
    expect(
      within(sheet).getByRole('heading', { name: 'Add Apple client' }),
    ).toBeInTheDocument();

    await user.type(
      within(sheet).getByRole('textbox', { name: 'Services ID' }),
      'com.example.app',
    );
    await user.click(within(sheet).getByRole('button', { name: 'Add client' }));

    await waitFor(() => {
      expect(providerBody).toEqual({ provider_name: 'apple' });
    });
    await waitFor(() => {
      expect(clientBody).toEqual({
        provider_id: 'provider-apple',
        client_name: 'apple',
        client_id: 'com.example.app',
        discovery_endpoint:
          'https://appleid.apple.com/.well-known/openid-configuration',
      });
    });
    expect(await screen.findByText('Added Apple client')).toBeInTheDocument();
  });

  it('edits a Google client and deletes it after confirmation', async () => {
    const user = userEvent.setup();
    let updateBody: unknown;
    let deletedPath: string | undefined;
    server.use(
      http.post(
        `${API}/dash/apps/app-1/oauth_clients/:clientRecordId`,
        async ({ params, request }) => {
          updateBody = await request.json();
          return HttpResponse.json({
            client: {
              ...mockGoogleWebClient,
              id: String(params['clientRecordId']),
              client_id: 'new-client-id',
            },
          });
        },
      ),
      http.delete(
        `${API}/dash/apps/app-1/oauth_clients/:clientRecordId`,
        ({ params }) => {
          deletedPath = String(params['clientRecordId']);
          return HttpResponse.json({ client: mockGoogleWebClient });
        },
      ),
    );

    await renderAuth(`${ROUTE}?client=client-google-web`);
    const sheet = await screen.findByRole('dialog');
    const clientId = within(sheet).getByRole('textbox', { name: 'Client ID' });
    await waitFor(() => {
      expect(clientId).toHaveValue(
        'google-client-id.apps.googleusercontent.com',
      );
    });
    await user.clear(clientId);
    await user.type(clientId, 'new-client-id');
    await user.click(within(sheet).getByRole('button', { name: 'Save' }));

    await waitFor(() => {
      expect(updateBody).toEqual({
        client_id: 'new-client-id',
        extra_audiences: [],
        meta: { skipNonceChecks: true, appType: 'web' },
      });
    });
    expect(
      await screen.findByText('Google client updated'),
    ).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Delete google-web' }));
    const confirm = await screen.findByRole('dialog');
    await user.click(
      within(confirm).getByRole('button', { name: 'Delete client' }),
    );

    await waitFor(() => {
      expect(deletedPath).toBe('client-google-web');
    });
    expect(await screen.findByText('Client deleted')).toBeInTheDocument();
  });

  it('adds and deletes a redirect origin', async () => {
    const user = userEvent.setup();
    let originBody: unknown;
    let deletedPath: string | undefined;
    server.use(
      http.post(
        `${API}/dash/apps/app-1/authorized_redirect_origins`,
        async ({ request }) => {
          originBody = await request.json();
          return HttpResponse.json({
            origin: {
              id: 'origin-created',
              service: 'generic',
              params: ['docs.example.org'],
              created_at: '2026-01-06T00:00:00Z',
            },
          });
        },
      ),
      http.delete(
        `${API}/dash/apps/app-1/authorized_redirect_origins/:originId`,
        ({ params }) => {
          deletedPath = String(params['originId']);
          return HttpResponse.json({
            origin: {
              id: String(params['originId']),
              service: 'generic',
              params: ['example.com'],
            },
          });
        },
      ),
    );

    await renderAuth(`${ROUTE}?tab=origins`);
    const table = await screen.findByRole('table', {
      name: 'Redirect origins',
    });
    expect(within(table).getByText('example.com')).toBeInTheDocument();
    expect(within(table).getByText('todo-preview')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Add origin' }));
    const dialog = await screen.findByRole('dialog');
    await user.type(
      within(dialog).getByRole('textbox', { name: 'Origin domain' }),
      'docs.example.org',
    );
    await user.click(
      within(dialog).getByRole('button', { name: 'Add origin' }),
    );

    await waitFor(() => {
      expect(originBody).toEqual({
        service: 'generic',
        params: ['docs.example.org'],
      });
    });
    expect(
      await screen.findByText('Redirect origin added'),
    ).toBeInTheDocument();

    await user.click(
      screen.getByRole('button', { name: 'Delete origin example.com' }),
    );
    const confirm = await screen.findByRole('dialog');
    await user.click(
      within(confirm).getByRole('button', { name: 'Delete origin' }),
    );

    await waitFor(() => {
      expect(deletedPath).toBe('origin-website');
    });
    expect(
      await screen.findByText('Redirect origin deleted'),
    ).toBeInTheDocument();
  });

  it('edits and saves the magic code email', async () => {
    const user = userEvent.setup();
    let saved: unknown;
    server.use(
      http.post(
        `${API}/dash/apps/app-1/email_templates`,
        async ({ request }) => {
          saved = await request.json();
          return HttpResponse.json({ id: 'template-1' });
        },
      ),
    );

    await renderAuth(`${ROUTE}?tab=email`);
    const subject = await screen.findByRole('textbox', { name: 'Subject' });
    expect(subject).toHaveValue('Your code');

    fireEvent.change(subject, {
      target: { value: 'Use {code} to sign in' },
    });
    fireEvent.change(screen.getByLabelText('Email body'), {
      target: { value: 'Hello {app_title}, your code is {code}.' },
    });

    expect(
      await screen.findByText('You have unsaved changes'),
    ).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => {
      expect(saved).toEqual({
        'email-type': 'magic-code',
        subject: 'Use {code} to sign in',
        body: 'Hello {app_title}, your code is {code}.',
        'sender-name': 'magic-code',
        'sender-email': 'noreply@example.com',
      });
    });
    expect(await screen.findByText('Email template saved')).toBeInTheDocument();
  });

  it('previews the email body in a sandboxed iframe', async () => {
    const user = userEvent.setup();
    await renderAuth(`${ROUTE}?tab=email`);

    await user.click(await screen.findByRole('button', { name: 'Preview' }));

    const frame = await screen.findByTitle('Email preview');
    expect(frame).toHaveAttribute('sandbox', '');
    expect(frame.getAttribute('srcdoc')).toContain('123456');
  });

  it('offers the platform sender when no template is saved', async () => {
    server.use(
      http.get(`${API}/dash/apps/:appId`, ({ params }) =>
        HttpResponse.json({
          app: {
            ...mockTodoApp,
            id: String(params['appId']),
            magic_code_email_template: null,
            magic_code_expiry_minutes: null,
          },
        }),
      ),
    );

    await renderAuth(`${ROUTE}?tab=email`);

    expect(await screen.findByText('verify@example.com')).toBeInTheDocument();
    expect(
      screen.getByRole('button', { name: 'Use my own domain' }),
    ).toBeInTheDocument();
    expect(screen.getByRole('textbox', { name: 'Subject' })).toHaveValue(
      '{code} is your code for {app_title}',
    );
  });

  it('sends and verifies a sender code', async () => {
    const user = userEvent.setup();
    let codeSent = false;
    server.use(
      http.get(`${API}/dash/apps/app-1/sender-verification`, () =>
        HttpResponse.json({
          ...mockSenderVerification,
          instant: { 'verified?': false },
        }),
      ),
      http.post(
        `${API}/dash/apps/app-1/sender-verification/send-magic-code`,
        () => {
          codeSent = true;
          return HttpResponse.json({ sent: true });
        },
      ),
    );

    await renderAuth(`${ROUTE}?tab=email`);
    await user.click(await screen.findByRole('button', { name: 'Send code' }));

    await waitFor(() => {
      expect(codeSent).toBe(true);
    });
    expect(
      await screen.findByText('Verification code sent to noreply@example.com'),
    ).toBeInTheDocument();

    await user.type(
      screen.getByRole('textbox', { name: 'Verification code' }),
      '123456',
    );
    await user.click(screen.getByRole('button', { name: 'Verify' }));

    expect(
      await screen.findByText('Sender email verified'),
    ).toBeInTheDocument();
  });

  it('updates the magic code expiry', async () => {
    const user = userEvent.setup();
    let expiryBody: unknown;
    server.use(
      http.post(
        `${API}/dash/apps/app-1/set-magic-code-expiry`,
        async ({ request }) => {
          expiryBody = await request.json();
          return HttpResponse.json({
            app: { ...mockTodoApp, magic_code_expiry_minutes: 30 },
          });
        },
      ),
    );

    await renderAuth(`${ROUTE}?tab=email`);
    const input = await screen.findByRole('spinbutton', {
      name: 'Codes expire after',
    });
    await user.clear(input);
    await user.type(input, '30');
    await user.click(screen.getByRole('button', { name: 'Save code expiry' }));

    await waitFor(() => {
      expect(expiryBody).toEqual({ expiry: 30 });
    });
    expect(await screen.findByText('Code expiry updated')).toBeInTheDocument();
  });

  it('lists, adds and deletes test users', async () => {
    const user = userEvent.setup();
    let added: unknown;
    let deleted: unknown;
    server.use(
      http.post(`${API}/dash/apps/app-1/test_users`, async ({ request }) => {
        added = await request.json();
        return HttpResponse.json({
          'test-user': {
            id: 'test-user-created',
            app_id: 'app-1',
            email: 'new@example.com',
            code: '123456',
            created_at: '2026-01-10T00:00:00Z',
          },
        });
      }),
      http.delete(`${API}/dash/apps/app-1/test_users`, async ({ request }) => {
        deleted = await request.json();
        return HttpResponse.json({
          'test-user': {
            id: 'test-user-1',
            app_id: 'app-1',
            email: 'qa@example.com',
            code: '424242',
            created_at: '2026-01-08T00:00:00Z',
          },
        });
      }),
    );

    await renderAuth(`${ROUTE}?tab=test-users`);
    const table = await screen.findByRole('table', { name: 'Test users' });
    expect(within(table).getByText('qa@example.com')).toBeInTheDocument();
    expect(within(table).getByText('review@example.com')).toBeInTheDocument();

    await user.click(
      within(table).getByRole('button', {
        name: 'Reveal code for qa@example.com',
      }),
    );
    expect(within(table).getByText('424242')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Add test user' }));
    const dialog = await screen.findByRole('dialog');
    await user.type(
      within(dialog).getByRole('textbox', { name: 'Email' }),
      'new@example.com',
    );
    const code = within(dialog).getByRole('textbox', { name: 'Fixed code' });
    await user.clear(code);
    await user.type(code, '123456');
    await user.click(
      within(dialog).getByRole('button', { name: 'Add test user' }),
    );

    await waitFor(() => {
      expect(added).toEqual({ email: 'new@example.com', code: '123456' });
    });
    expect(
      await screen.findByText('new@example.com can sign in with 123456'),
    ).toBeInTheDocument();
    await waitFor(() => {
      expect(
        within(screen.getByRole('table', { name: 'Test users' })).getByText(
          'new@example.com',
        ),
      ).toBeInTheDocument();
    });

    await user.click(
      screen.getByRole('button', { name: 'Delete test user qa@example.com' }),
    );
    const confirm = await screen.findByRole('dialog');
    await user.click(
      within(confirm).getByRole('button', { name: 'Remove test user' }),
    );

    await waitFor(() => {
      expect(deleted).toEqual({ id: 'test-user-1' });
    });
    await waitFor(() => {
      expect(
        within(screen.getByRole('table', { name: 'Test users' })).queryByText(
          'qa@example.com',
        ),
      ).not.toBeInTheDocument();
    });
  });
});
