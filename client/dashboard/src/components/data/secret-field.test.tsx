import { act, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { SecretField } from './secret-field';

const SECRET = 'a1b2c3d4e5f6';

describe('SecretField', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('hides the value again after 30s', () => {
    render(<SecretField value={SECRET} hideAfterMs={30_000} />);

    expect(screen.getByText('a1b2c3••••••••')).toBeInTheDocument();
    expect(screen.queryByText(SECRET)).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Show secret' }));
    expect(screen.getByText(SECRET)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Hide secret' })).toHaveAttribute(
      'aria-pressed',
      'true',
    );

    act(() => {
      vi.advanceTimersByTime(29_999);
    });
    expect(screen.getByText(SECRET)).toBeInTheDocument();

    act(() => {
      vi.advanceTimersByTime(1);
    });
    expect(screen.queryByText(SECRET)).not.toBeInTheDocument();
    expect(screen.getByText('a1b2c3••••••••')).toBeInTheDocument();
  });

  it('cancels the auto-hide timer when hidden by hand', () => {
    render(<SecretField value={SECRET} hideAfterMs={30_000} />);

    fireEvent.click(screen.getByRole('button', { name: 'Show secret' }));
    fireEvent.click(screen.getByRole('button', { name: 'Hide secret' }));
    expect(screen.getByText('a1b2c3••••••••')).toBeInTheDocument();

    // Reveal again; the second reveal gets a full window, not the first one's.
    fireEvent.click(screen.getByRole('button', { name: 'Show secret' }));
    act(() => {
      vi.advanceTimersByTime(29_000);
    });
    expect(screen.getByText(SECRET)).toBeInTheDocument();
  });
});
