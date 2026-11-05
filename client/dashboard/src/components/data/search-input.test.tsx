import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useState } from 'react';
import { describe, expect, it, vi } from 'vitest';
import { SearchInput } from './search-input';

describe('SearchInput', () => {
  it('debounces typing, then Esc clears before it blurs', async () => {
    const onValueChange = vi.fn();
    // Keys that reached the enclosing surface; Escape must never be one.
    const bubbledKeys: string[] = [];

    function Harness() {
      const [value, setValue] = useState('');
      return (
        // eslint-disable-next-line jsx-a11y/no-static-element-interactions -- test harness observes bubbled keys
        <div
          onKeyDown={(event) => {
            bubbledKeys.push(event.key);
          }}
        >
          <SearchInput
            value={value}
            onValueChange={(next) => {
              setValue(next);
              onValueChange(next);
            }}
            aria-label="Search apps"
          />
        </div>
      );
    }

    const user = userEvent.setup();
    render(<Harness />);
    const input = screen.getByRole('searchbox', { name: 'Search apps' });

    await user.type(input, 'todos');
    await waitFor(() => {
      expect(onValueChange).toHaveBeenLastCalledWith('todos');
    });

    // First Esc clears the text and keeps focus; the enclosing surface stays open.
    await user.keyboard('{Escape}');
    expect(input).toHaveValue('');
    expect(input).toHaveFocus();
    expect(bubbledKeys).not.toContain('Escape');

    // With nothing left to clear, Esc leaves the field.
    await user.keyboard('{Escape}');
    expect(input).not.toHaveFocus();
    expect(bubbledKeys).not.toContain('Escape');
  });
});
