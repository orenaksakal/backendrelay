import { Upload, X } from 'lucide-react';
import { Button } from '@/components/ui/button';

/**
 * C.9 upload progress. The panel is page content pinned to the bottom right,
 * on the same in-page layer as the sticky table header: it floats above the
 * page but must stay below any sheet or dialog, whose footers own that corner
 * (DASH.fix10). Each file keeps its own state and Cancel, and every state
 * change is announced once through the polite status line.
 */

export type UploadStatus =
  | 'queued'
  | 'uploading'
  | 'done'
  | 'failed'
  | 'cancelled';

export interface UploadTaskView {
  id: string;
  name: string;
  status: UploadStatus;
  /** 0 until the file is on the server, then 100. */
  progress: number;
  error?: string;
}

export interface UploadPanelProps {
  tasks: ReadonlyArray<UploadTaskView>;
  /** The sentence the polite live region should read next. */
  announcement: string;
  onCancel: (id: string) => void;
  onDismiss: () => void;
}

const STATUS_LABELS: Record<UploadStatus, string> = {
  queued: 'Waiting',
  uploading: 'Uploading…',
  done: 'Uploaded',
  failed: 'Failed',
  cancelled: 'Cancelled',
};

export function UploadPanel({
  tasks,
  announcement,
  onCancel,
  onDismiss,
}: UploadPanelProps) {
  if (tasks.length === 0) return null;
  const active = tasks.some(
    (task) => task.status === 'queued' || task.status === 'uploading',
  );

  return (
    <section
      aria-label="Uploads"
      className="border-border bg-card fixed right-4 bottom-4 z-(--z-sticky) flex w-full max-w-[360px] flex-col gap-2 rounded-lg border p-3 shadow-lg"
    >
      <div className="flex items-center gap-2">
        <Upload aria-hidden="true" className="text-muted-foreground size-4" />
        <p className="min-w-0 flex-1 text-sm font-medium">
          {active
            ? tasks.length === 1
              ? 'Uploading 1 file'
              : `Uploading ${String(tasks.length)} files`
            : 'Uploads'}
        </p>
        {active ? null : (
          <Button
            type="button"
            variant="ghost"
            size="sm"
            aria-label="Dismiss uploads"
            className="size-7 p-0"
            onClick={onDismiss}
          >
            <X aria-hidden="true" />
          </Button>
        )}
      </div>

      <p role="status" aria-live="polite" className="sr-only">
        {announcement}
      </p>

      <ul className="flex flex-col gap-2">
        {tasks.map((task) => (
          <li key={task.id} className="flex flex-col gap-1">
            <div className="flex items-center gap-2">
              <span className="min-w-0 flex-1 truncate text-xs">
                {task.name}
              </span>
              <span
                className={
                  task.status === 'failed' || task.status === 'cancelled'
                    ? 'text-warning text-xs'
                    : 'text-muted-foreground text-xs'
                }
              >
                {STATUS_LABELS[task.status]}
              </span>
              {task.status === 'queued' || task.status === 'uploading' ? (
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  className="h-6 px-1 text-xs"
                  onClick={() => {
                    onCancel(task.id);
                  }}
                >
                  Cancel
                </Button>
              ) : null}
            </div>
            <progress
              aria-label={`${task.name} upload progress`}
              max={100}
              value={task.progress}
              className="h-1 w-full overflow-hidden rounded-full"
            />
            {task.error === undefined ? null : (
              <p className="text-destructive text-xs">{task.error}</p>
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}
