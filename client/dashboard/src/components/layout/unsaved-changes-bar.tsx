import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';

export interface UnsavedChangesBarProps {
  /** Render nothing until the form is dirty. */
  dirty: boolean;
  onReset: () => void;
  onSave: () => void;
  saving?: boolean;
  /** Mutation error shown inline, so it is visible next to Save. */
  error?: string | null;
  className?: string;
}

/**
 * Floating bar at the bottom centre while a settings form differs from the
 * server (B.4, B.5.1). The router blocker handles navigation; this bar handles
 * the save and reset.
 */
export function UnsavedChangesBar({
  dirty,
  onReset,
  onSave,
  saving = false,
  error,
  className,
}: UnsavedChangesBarProps) {
  if (!dirty) {
    return null;
  }

  return (
    <div
      className={cn(
        'border-border bg-card fixed bottom-4 left-1/2 z-(--z-sticky) flex h-12 max-w-[calc(100vw-2rem)] -translate-x-1/2 items-center gap-3 rounded-lg border px-3 shadow-lg',
        className,
      )}
    >
      <p className="text-sm whitespace-nowrap">You have unsaved changes</p>
      {error ? (
        <p className="text-destructive max-w-64 truncate text-xs" role="alert">
          {error}
        </p>
      ) : null}
      <div className="ml-auto flex items-center gap-2">
        <Button
          type="button"
          variant="ghost"
          size="sm"
          disabled={saving}
          onClick={onReset}
        >
          Reset
        </Button>
        <Button type="button" size="sm" disabled={saving} onClick={onSave}>
          {saving ? 'Saving…' : 'Save'}
        </Button>
      </div>
    </div>
  );
}
