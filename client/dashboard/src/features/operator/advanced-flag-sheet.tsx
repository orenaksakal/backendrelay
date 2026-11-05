import { useMemo, useState } from 'react';
import { CodeEditor } from '@/components/code-editor/code-editor';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  Sheet,
  SheetBody,
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import type { AdvancedFlagChange, AdvancedFlagRow } from '@/lib/operator/model';

export interface AdvancedFlagSheetProps {
  flag: AdvancedFlagRow;
  /** The value the server currently stores, used to decide the reset action. */
  initialValue: unknown;
  onApply: (change: AdvancedFlagChange) => void;
  onClose: () => void;
}

/**
 * C.19 advanced flag editor: the flag's JSON value in the shared `CodeEditor`
 * sheet. Applying only stages the change; the page's `UnsavedChangesBar`
 * performs the PUT or DELETE (B.5.1).
 */
export function AdvancedFlagSheet({
  flag,
  initialValue,
  onApply,
  onClose,
}: AdvancedFlagSheetProps) {
  const [text, setText] = useState(() => initialText(initialValue));
  const parsed = useMemo(() => parseJsonValue(text), [text]);

  return (
    <Sheet
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
    >
      <SheetContent size="md">
        <SheetHeader>
          <SheetTitle>Edit {flag.name}</SheetTitle>
          <SheetDescription>
            {flag.description ?? 'This flag is not documented in this build.'}
          </SheetDescription>
        </SheetHeader>
        <SheetBody className="flex flex-col gap-3">
          <div className="flex items-center gap-2">
            <Badge>{flag.type ?? 'unknown type'}</Badge>
            <span className="text-muted-foreground text-xs">JSON value</span>
          </div>
          <CodeEditor
            value={text}
            mode="json"
            aria-label={`${flag.name} JSON value`}
            height="240px"
            onChange={setText}
          />
          <p className="text-muted-foreground text-xs">
            Changes stay in the browser until you save this page.
          </p>
        </SheetBody>
        <SheetFooter>
          {initialValue === undefined ? null : (
            <Button
              type="button"
              variant="secondary"
              className="mr-auto"
              onClick={() => {
                onApply({ action: 'reset' });
              }}
            >
              Reset to default
            </Button>
          )}
          <Button type="button" variant="secondary" onClick={onClose}>
            Cancel
          </Button>
          <Button
            type="button"
            disabled={parsed === undefined}
            onClick={() => {
              if (parsed !== undefined) {
                onApply({ action: 'set', value: parsed.value });
              }
            }}
          >
            Apply
          </Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}

function initialText(value: unknown): string {
  if (value === undefined) return '';
  return JSON.stringify(value, null, 2);
}

function parseJsonValue(text: string): { value: unknown } | undefined {
  if (text.trim() === '') return undefined;
  try {
    return { value: JSON.parse(text) as unknown };
  } catch {
    return undefined;
  }
}
