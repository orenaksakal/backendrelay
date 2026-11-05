import { useMemo } from 'react';
import {
  diffPrefix,
  unifiedDiff,
  type DiffLineKind,
} from '@/lib/permissions/diff';
import { cn } from '@/lib/utils';

export interface PermissionsDiffProps {
  before: string;
  after: string;
  /** Accessible name and visible caption, for example "Current → draft". */
  label: string;
  className?: string;
}

function lineClass(kind: DiffLineKind): string {
  if (kind === 'add') return 'text-success';
  if (kind === 'remove') return 'text-destructive';
  return 'text-foreground';
}

/**
 * C.10 diff review: unified diff lines with `+`/`-` prefixes, so the meaning
 * survives without colour (B.1 status-colour rule).
 */
export function PermissionsDiff({
  before,
  after,
  label,
  className,
}: PermissionsDiffProps) {
  const hunks = useMemo(() => unifiedDiff(before, after), [before, after]);

  if (hunks.length === 0) {
    return (
      <p className={cn('text-muted-foreground text-sm', className)}>
        No changes.
      </p>
    );
  }

  return (
    <figure
      className={cn(
        'border-border bg-muted/40 overflow-hidden rounded-md border',
        className,
      )}
    >
      <figcaption className="border-border text-muted-foreground border-b px-3 py-1.5 text-xs">
        {label}
      </figcaption>
      <pre aria-label={label} className="text-cell overflow-auto p-3 font-mono">
        <code>
          {hunks.map((hunk, hunkIndex) => (
            <span key={`hunk-${String(hunkIndex)}`}>
              <span className="text-muted-foreground">
                {`@@ -${String(hunk.oldStart)},${String(hunk.oldCount)} +${String(hunk.newStart)},${String(hunk.newCount)} @@`}
                {'\n'}
              </span>
              {hunk.lines.map((line, lineIndex) => (
                <span
                  key={`line-${String(hunkIndex)}-${String(lineIndex)}`}
                  className={lineClass(line.kind)}
                >
                  {`${diffPrefix(line.kind)}${line.text}`}
                  {'\n'}
                </span>
              ))}
            </span>
          ))}
        </code>
      </pre>
    </figure>
  );
}
