import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { TypedConfirmDialog } from './typed-confirm-dialog';

function setup() {
  const onConfirm = vi.fn();
  const onOpenChange = vi.fn();

  render(
    <TypedConfirmDialog
      open
      onOpenChange={onOpenChange}
      title="Delete app"
      description="This removes the app and everything in it."
      consequences={['Every entity is deleted.', 'The API stops responding.']}
      confirmText="todo-app"
      confirmLabel="Delete app"
      onConfirm={onConfirm}
    />,
  );

  return { onConfirm, onOpenChange, user: userEvent.setup() };
}

describe('TypedConfirmDialog', () => {
  it('enables the destructive button only for the exact text', async () => {
    const { user, onConfirm } = setup();
    const button = screen.getByRole('button', { name: 'Delete app' });
    const input = screen.getByLabelText(/type todo-app to confirm/i);

    expect(button).toBeDisabled();

    await user.type(input, 'todo');
    expect(button).toBeDisabled();

    // Matching is case-sensitive, so capitalising the title must not unlock it.
    await user.clear(input);
    await user.type(input, 'Todo-app');
    expect(button).toBeDisabled();

    await user.clear(input);
    await user.type(input, 'todo-app');
    expect(button).toBeEnabled();

    await user.click(button);
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it('keeps the button disabled while the action is running', () => {
    render(
      <TypedConfirmDialog
        open
        onOpenChange={() => undefined}
        title="Delete app"
        consequences={['Every entity is deleted.']}
        confirmText="restore"
        confirmLabel="Restore backup"
        confirming
        onConfirm={() => undefined}
      />,
    );

    expect(screen.getByRole('button', { name: 'Working…' })).toBeDisabled();
  });
});
