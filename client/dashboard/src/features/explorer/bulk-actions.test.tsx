import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { BulkActions } from './bulk-actions';

describe('bulk actions', () => {
  it('shows the selection and its three actions', async () => {
    const user = userEvent.setup();
    const onExport = vi.fn();
    const onClear = vi.fn();
    render(
      <BulkActions
        count={2}
        onExport={onExport}
        onDelete={vi.fn()}
        onClear={onClear}
      />,
    );

    expect(screen.getByText('2 selected')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Export JSON' }));
    expect(onExport).toHaveBeenCalledTimes(1);

    await user.click(screen.getByRole('button', { name: 'Clear' }));
    expect(onClear).toHaveBeenCalledTimes(1);
  });

  it('confirms a small deletion with the Tier 2 dialog', async () => {
    const user = userEvent.setup();
    const onDelete = vi.fn(() => Promise.resolve(undefined));
    render(
      <BulkActions
        count={2}
        onExport={vi.fn()}
        onDelete={onDelete}
        onClear={vi.fn()}
      />,
    );

    await user.click(screen.getByRole('button', { name: 'Delete' }));
    expect(
      screen.getByRole('heading', { name: 'Delete 2 entities?' }),
    ).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Delete entities' }));
    expect(onDelete).toHaveBeenCalledTimes(1);
  });

  it('requires the count above ten entities before deleting', async () => {
    const user = userEvent.setup();
    const onDelete = vi.fn(() => Promise.resolve(undefined));
    render(
      <BulkActions
        count={11}
        onExport={vi.fn()}
        onDelete={onDelete}
        onClear={vi.fn()}
      />,
    );

    await user.click(screen.getByRole('button', { name: 'Delete' }));
    expect(
      screen.getByRole('heading', { name: 'Delete 11 entities?' }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole('button', { name: 'Delete entities' }),
    ).toBeDisabled();

    await user.type(screen.getByRole('textbox'), '11');
    await user.click(screen.getByRole('button', { name: 'Delete entities' }));
    expect(onDelete).toHaveBeenCalledTimes(1);
  });

  it('renders nothing with no selection', () => {
    render(
      <BulkActions
        count={0}
        onExport={vi.fn()}
        onDelete={vi.fn()}
        onClear={vi.fn()}
      />,
    );
    expect(
      screen.queryByRole('region', { name: 'Bulk actions' }),
    ).not.toBeInTheDocument();
  });
});
