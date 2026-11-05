import { act, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { CopyButton } from './copy-button';

describe('CopyButton', () => {
  const writeText = vi.fn<() => Promise<void>>(() => Promise.resolve());

  beforeEach(() => {
    vi.useFakeTimers();
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: { writeText },
    });
  });

  afterEach(() => {
    vi.useRealTimers();
    Reflect.deleteProperty(navigator, 'clipboard');
  });

  it('copies the value and announces "Copied" for 1.5s', async () => {
    render(<CopyButton value="3f2a9c1d" />);

    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: 'Copy' }));
      await Promise.resolve();
    });

    expect(writeText).toHaveBeenCalledWith('3f2a9c1d');
    expect(screen.getByRole('status')).toHaveTextContent('Copied');

    act(() => {
      vi.advanceTimersByTime(1499);
    });
    expect(screen.getByRole('status')).toHaveTextContent('Copied');

    act(() => {
      vi.advanceTimersByTime(1);
    });
    expect(screen.getByRole('status')).toBeEmptyDOMElement();
  });

  it('announces a failure when the clipboard is unavailable', async () => {
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: undefined,
    });

    render(<CopyButton value="3f2a9c1d" />);

    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: 'Copy' }));
      await Promise.resolve();
    });

    expect(screen.getByRole('status')).toHaveTextContent("Couldn't copy");
  });
});
