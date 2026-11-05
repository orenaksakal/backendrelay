import { ChevronLeft, ChevronRight } from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { formatCount } from '@/lib/format';
import { cn } from '@/lib/utils';

/** Page sizes offered by list screens (B.4). */
const PAGE_SIZES = [25, 50, 100, 200] as const;

export interface PaginationProps {
  /** 1-based page number. */
  page: number;
  pageSize: number;
  /** Omit or pass `null` when the server cannot count; the range shows `N+`. */
  total?: number | null;
  onPageChange: (page: number) => void;
  onPageSizeChange?: (pageSize: number) => void;
  pageSizes?: ReadonlyArray<number>;
  /** Overrides the inferred "there is a next page" state. */
  hasNext?: boolean;
  className?: string;
}

/**
 * "Showing 1–50 of 312", the page-size select and Prev/Next. Hidden entirely
 * when a known total fits on one page (B.4).
 */
export function Pagination({
  page,
  pageSize,
  total,
  onPageChange,
  onPageSizeChange,
  pageSizes = PAGE_SIZES,
  hasNext,
  className,
}: PaginationProps) {
  if (total != null && total <= pageSize) {
    return null;
  }

  const start = (page - 1) * pageSize + 1;
  const end =
    total != null ? Math.min(page * pageSize, total) : page * pageSize;
  const nextDisabled =
    hasNext === undefined ? total != null && end >= total : !hasNext;

  return (
    <nav
      aria-label="Pagination"
      className={cn(
        'flex flex-wrap items-center justify-between gap-3 py-2',
        className,
      )}
    >
      <p className="text-muted-foreground text-xs">
        Showing {formatCount(start)}–{formatCount(end)} of{' '}
        {total != null ? formatCount(total) : `${formatCount(end)}+`}
      </p>
      <div className="flex items-center gap-3">
        {onPageSizeChange ? (
          <div className="flex items-center gap-1.5">
            <span className="text-muted-foreground text-xs">Rows per page</span>
            <Select
              value={String(pageSize)}
              onValueChange={(value) => {
                const next = Number(value);
                if (Number.isFinite(next) && next > 0) {
                  onPageSizeChange(next);
                }
              }}
            >
              <SelectTrigger aria-label="Rows per page" className="h-7 w-20">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {pageSizes.map((size) => (
                  <SelectItem key={size} value={String(size)}>
                    {size}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
        ) : null}
        <div className="flex items-center gap-1">
          <Button
            type="button"
            variant="secondary"
            size="sm"
            aria-label="Previous page"
            disabled={page <= 1}
            onClick={() => {
              onPageChange(page - 1);
            }}
          >
            <ChevronLeft aria-hidden="true" />
          </Button>
          <Button
            type="button"
            variant="secondary"
            size="sm"
            aria-label="Next page"
            disabled={nextDisabled}
            onClick={() => {
              onPageChange(page + 1);
            }}
          >
            <ChevronRight aria-hidden="true" />
          </Button>
        </div>
      </div>
    </nav>
  );
}
