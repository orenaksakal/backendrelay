import { Kbd, KbdGroup } from '@/components/ui/kbd';
import {
  Sheet,
  SheetBody,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import { groupShortcuts, shortcutCaps, type Shortcut } from '@/lib/shortcuts';

export interface ShortcutsSheetProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  shortcuts?: readonly Shortcut[];
}

/**
 * The `?` sheet: every entry in the shortcut registry, grouped by where it
 * applies (B.7). It is also the reference for the shortcuts shown in tooltips.
 */
export function ShortcutsSheet({
  open,
  onOpenChange,
  shortcuts,
}: ShortcutsSheetProps) {
  const groups = groupShortcuts(shortcuts);

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent size="sm">
        <SheetHeader>
          <SheetTitle>Keyboard shortcuts</SheetTitle>
          <SheetDescription>
            Shortcuts are ignored while you type in a field or an editor.
          </SheetDescription>
        </SheetHeader>
        <SheetBody className="flex flex-col gap-5">
          {groups.map((group) => (
            <section
              key={group.scope}
              aria-labelledby={`shortcut-group-${group.scope}`}
              className="flex flex-col gap-1"
            >
              <h3
                id={`shortcut-group-${group.scope}`}
                className="text-muted-foreground px-1 text-[11px] font-medium tracking-[0.04em] uppercase"
              >
                {group.label}
              </h3>
              <dl className="flex flex-col">
                {group.shortcuts.map((shortcut) => (
                  <div
                    key={shortcut.id}
                    className="flex min-h-8 items-center justify-between gap-3 px-1 py-1"
                  >
                    <dt className="text-sm">{shortcut.label}</dt>
                    <dd className="shrink-0">
                      <KbdGroup>
                        {shortcutCaps(shortcut.keys).map((cap, index) => (
                          <Kbd key={`${shortcut.id}-${String(index)}`}>
                            {cap}
                          </Kbd>
                        ))}
                      </KbdGroup>
                    </dd>
                  </div>
                ))}
              </dl>
            </section>
          ))}
        </SheetBody>
      </SheetContent>
    </Sheet>
  );
}
