import { History } from 'lucide-react';
import { RelativeTime } from '@/components/data/relative-time';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { historyMenuRuns, type PlaygroundRun } from '@/lib/playground/history';
import { sourceSummary } from '@/lib/playground/model';

/**
 * C.12 History menu: saved runs first, then the recent 50. Choosing an entry
 * restores its editor document; "Save as…" names the latest run.
 */
export function PlaygroundHistoryMenu({
  runs,
  onRestore,
  onSaveAs,
}: {
  runs: ReadonlyArray<PlaygroundRun>;
  onRestore: (run: PlaygroundRun) => void;
  onSaveAs: () => void;
}) {
  const entries = historyMenuRuns(runs);

  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        render={
          <Button type="button" variant="secondary" aria-label="Run history" />
        }
      >
        <History aria-hidden="true" />
        History
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-80">
        <DropdownMenuItem disabled={runs.length === 0} onClick={onSaveAs}>
          Save as…
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        {entries.length === 0 ? (
          <DropdownMenuItem disabled>No runs yet</DropdownMenuItem>
        ) : (
          <DropdownMenuGroup>
            <DropdownMenuLabel>Runs</DropdownMenuLabel>
            {entries.map((run) => (
              <DropdownMenuItem
                key={run.id}
                className="flex h-auto flex-col items-start gap-0.5 py-1.5"
                onClick={() => {
                  onRestore(run);
                }}
              >
                <span className="flex w-full items-center gap-2">
                  <span className="min-w-0 flex-1 truncate text-sm">
                    {run.name ?? sourceSummary(run.source)}
                  </span>
                  <span className="text-muted-foreground shrink-0 text-xs">
                    {run.mode === 'query' ? 'Query' : 'Transact'}
                  </span>
                </span>
                <span className="text-muted-foreground flex w-full items-center gap-1 text-xs">
                  <span className="min-w-0 flex-1 truncate">
                    {run.actorLabel}
                  </span>
                  <RelativeTime value={run.createdAt} />
                </span>
              </DropdownMenuItem>
            ))}
          </DropdownMenuGroup>
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
