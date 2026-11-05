// @vitest-environment jsdom
import React from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from '@testing-library/react';
import Auth from './Auth';
import { sendMagicCode, verifyMagicCode } from '@/lib/auth';

vi.mock('@/lib/auth', () => ({
  sendMagicCode: vi.fn(),
  verifyMagicCode: vi.fn(),
}));
vi.mock('@/lib/config', () => ({
  dashboardApiURI: 'https://private.test/__instant_api',
  isDev: false,
  isSelfHosted: true,
}));
vi.mock('@/lib/navigation', () => ({ useRouter: () => ({ asPath: '/dash' }) }));
vi.mock('posthog-js/react', () => ({
  usePostHog: () => ({ identify: vi.fn(), capture: vi.fn() }),
}));

afterEach(() => {
  cleanup();
  vi.resetAllMocks();
});

describe('original dashboard sign-in', () => {
  it('keeps BackendRelay branding and the upstream form and shared controls', () => {
    const { container } = render(React.createElement(Auth));
    expect(screen.getByText("Let's log you in")).toBeTruthy();
    expect(
      screen.getByRole('link', { name: 'BackendRelay home' }),
    ).toBeTruthy();
    expect(
      screen.getByRole('button', { name: 'Send Code' }).className,
    ).toContain('bg-[#606AF4]');
    expect(
      screen
        .getByRole('textbox', { name: 'Email address' })
        .getAttribute('autocomplete'),
    ).toBe('email');
    expect(container.querySelector('.dash-input')).toBeNull();
    expect(
      screen
        .getByRole('link', { name: /Continue with Google/ })
        .getAttribute('href'),
    ).toMatch(/^https:\/\/private\.test\/__instant_api\/dash\/oauth\/start/);
  });

  it('retains the existing email-code verification flow', async () => {
    vi.mocked(sendMagicCode).mockResolvedValue(undefined);
    vi.mocked(verifyMagicCode).mockResolvedValue({
      token: 'test-session',
      user: { email: 'owner@example.test', id: 'owner', created_at: '' },
    } as any);
    const onVerified = vi.fn();
    render(React.createElement(Auth, { onVerified }));
    fireEvent.change(screen.getByRole('textbox', { name: 'Email address' }), {
      target: { value: 'owner@example.test' },
    });
    fireEvent.click(screen.getByRole('button', { name: 'Send Code' }));
    const input = await screen.findByRole('textbox', {
      name: 'Verification code',
    });
    expect(input.getAttribute('autocomplete')).toBe('one-time-code');
    fireEvent.change(input, { target: { value: '123456' } });
    fireEvent.click(screen.getByRole('button', { name: 'Verify Code' }));
    await waitFor(() =>
      expect(onVerified).toHaveBeenCalledWith({
        token: 'test-session',
        ticket: undefined,
      }),
    );
    expect(sendMagicCode).toHaveBeenCalledWith({ email: 'owner@example.test' });
    expect(verifyMagicCode).toHaveBeenCalledWith({
      email: 'owner@example.test',
      code: '123456',
    });
  });

  it('shows existing authentication errors in the original input without dropping accessibility', async () => {
    vi.mocked(sendMagicCode).mockRejectedValue({
      body: { type: 'param-malformed' },
    });
    render(React.createElement(Auth, { emailOnly: true }));
    fireEvent.change(screen.getByRole('textbox', { name: 'Email address' }), {
      target: { value: 'owner@example.test' },
    });
    fireEvent.click(screen.getByRole('button', { name: 'Send Code' }));
    expect(await screen.findByRole('alert')).toBeTruthy();
    expect(
      screen
        .getByRole('textbox', { name: 'Email address' })
        .getAttribute('aria-describedby'),
    ).toBe('sign-in-email-error');
    expect(
      screen.queryByRole('link', { name: /Continue with Google/ }),
    ).toBeNull();
  });
});
