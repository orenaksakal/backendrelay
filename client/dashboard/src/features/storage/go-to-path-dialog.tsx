import { useEffect, useId, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';

export interface GoToPathDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** The open prefix, so the field starts where the user is looking. */
  initialPath: string;
  onGo: (path: string) => void;
}

/** C.9 "Go to path…": jump to a folder that has no row at the current level. */
export function GoToPathDialog({
  open,
  onOpenChange,
  initialPath,
  onGo,
}: GoToPathDialogProps) {
  const inputId = useId();
  const inputRef = useRef<HTMLInputElement>(null);
  const [value, setValue] = useState(initialPath);

  useEffect(() => {
    if (open) {
      // eslint-disable-next-line react-hooks/set-state-in-effect -- opening the dialog restores the current path
      setValue(initialPath);
      inputRef.current?.focus();
    }
  }, [open, initialPath]);

  function submit() {
    onGo(value);
    onOpenChange(false);
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <form
          className="flex flex-col gap-4"
          onSubmit={(event) => {
            event.preventDefault();
            submit();
          }}
        >
          <DialogHeader>
            <DialogTitle>Go to path</DialogTitle>
            <DialogDescription>
              Enter a folder path, for example avatars/2026. The root is
              available as /.
            </DialogDescription>
          </DialogHeader>

          <div className="flex flex-col gap-1.5">
            <Label htmlFor={inputId}>Path</Label>
            <Input
              id={inputId}
              ref={inputRef}
              value={value}
              placeholder="avatars/2026"
              onChange={(event) => {
                setValue(event.target.value);
              }}
            />
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="secondary"
              onClick={() => {
                onOpenChange(false);
              }}
            >
              Cancel
            </Button>
            <Button type="submit">Go</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
