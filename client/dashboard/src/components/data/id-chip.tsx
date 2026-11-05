import { copyAnnouncement, useCopyFeedback } from '@/lib/clipboard';
import { truncateId } from '@/lib/format';
import { cn } from '@/lib/utils';

export interface IdChipProps {
  /** The full identifier; it is truncated in the middle for display. */
  value: string;
  /** Characters kept at the start and end. */
  head?: number;
  tail?: number;
  /**
   * `-1` inside the Explorer grid: the grid is one tab stop, and ⌘C copies
   * the focused cell, so per-row copy buttons must not join the tab order.
   */
  tabIndex?: number;
  className?: string;
}

/**
 * A copyable identifier. Clicking copies the whole value; the native tooltip
 * and the accessible name carry it too, so truncation never hides it (B.4).
 */
export function IdChip({
  value,
  head = 4,
  tail = 4,
  tabIndex,
  className,
}: IdChipProps) {
  const { status, copy } = useCopyFeedback();
  const display = truncateId(value, head, tail);
  // Label-in-name wants the visible text in the accessible name; keep the full
  // identifier after it when truncation actually removed characters.
  const fullValue = display === value ? '' : ` ${value}`;

  return (
    <>
      <button
        type="button"
        title={value}
        tabIndex={tabIndex}
        aria-label={`Copy ID ${display}${fullValue}`}
        className={cn(
          'text-cell bg-muted text-muted-foreground hover:text-foreground inline-flex max-w-full items-center rounded-md px-1.5 py-0.5 font-mono transition-colors',
          className,
        )}
        onClick={(event) => {
          // Rows in tables open on click; copying must not also open them.
          event.stopPropagation();
          void copy(value);
        }}
      >
        <span aria-hidden="true">{display}</span>
      </button>
      <span role="status" className="sr-only">
        {copyAnnouncement(status)}
      </span>
    </>
  );
}
