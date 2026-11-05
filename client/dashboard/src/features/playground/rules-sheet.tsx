import { CodeEditor } from '@/components/code-editor/code-editor';
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

/**
 * C.12 "Use custom rules": edits the object sent as `rules-override` for the
 * next run. It never saves, so the app's permission rules stay untouched.
 */
export function PlaygroundRulesSheet({
  open,
  onOpenChange,
  value,
  onChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  value: string;
  onChange: (value: string) => void;
}) {
  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent size="md">
        <SheetHeader>
          <SheetTitle>Custom permission rules</SheetTitle>
          <SheetDescription>
            These rules replace the app&apos;s saved rules for this run only.
            Nothing is saved.
          </SheetDescription>
        </SheetHeader>
        <SheetBody className="flex flex-col gap-2">
          <CodeEditor
            value={value}
            mode="json"
            aria-label="Custom permission rules"
            height="440px"
            onChange={onChange}
          />
          <p className="text-muted-foreground text-xs">
            Sent as <code className="font-mono">rules-override</code> with every
            query and transaction from this page.
          </p>
        </SheetBody>
        <SheetFooter>
          <Button
            type="button"
            onClick={() => {
              onOpenChange(false);
            }}
          >
            Done
          </Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}
