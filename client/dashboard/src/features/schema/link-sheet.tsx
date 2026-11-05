import { useEffect, useId, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  Sheet,
  SheetBody,
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import { Switch } from '@/components/ui/switch';
import type { AppInstantClient, AppNamespace } from '@/lib/instant';
import {
  addLinkOps,
  linkDraftProblem,
  type LinkDraft,
} from '@/lib/schema/attributes';
import { showErrorToast, toastManager } from '@/lib/toast';

export interface LinkSheetProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  namespace: AppNamespace;
  namespaces: ReadonlyArray<AppNamespace>;
  /** Link and attribute labels already used by the from-namespace. */
  existingLabels: ReadonlyArray<string>;
  client: AppInstantClient;
}

function defaultDraft(
  namespace: AppNamespace,
  namespaces: ReadonlyArray<AppNamespace>,
): LinkDraft {
  const target =
    namespaces.find((candidate) => candidate.name !== namespace.name) ??
    namespace;
  return {
    namespace: namespace.name,
    forwardLabel: '',
    targetNamespace: target.name,
    reverseLabel: namespace.name,
    forwardCardinality: 'many',
    reverseCardinality: 'many',
    cascade: false,
  };
}

/**
 * C.7 "Add link" sheet (480px). The from-namespace is fixed; the sheet names
 * both ends, the cardinality on each side and whether deleting a target
 * cascades to the linked entities. Saving is one `add-attr` ref step.
 */
export function LinkSheet({
  open,
  onOpenChange,
  namespace,
  namespaces,
  existingLabels,
  client,
}: LinkSheetProps) {
  const [draft, setDraft] = useState<LinkDraft>(() =>
    defaultDraft(namespace, namespaces),
  );
  const [error, setError] = useState<string>();
  const [saving, setSaving] = useState(false);
  const cascadeId = useId();
  const forwardRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (open) forwardRef.current?.focus();
  }, [open]);

  const selfLink = draft.targetNamespace === namespace.name;
  const cascadeAllowed = draft.forwardCardinality === 'one';

  function patch(next: Partial<LinkDraft>): void {
    setDraft((current) => ({ ...current, ...next }));
    setError(undefined);
  }

  async function save(): Promise<void> {
    if (saving) return;
    const problem = linkDraftProblem(draft, { namespaces, existingLabels });
    if (problem !== undefined) {
      setError(problem);
      return;
    }
    setSaving(true);
    setError(undefined);
    try {
      await client.core._reactor.pushOps(addLinkOps(draft));
      toastManager.add({
        type: 'success',
        title: `Linked ${namespace.name}.${draft.forwardLabel.trim()} to ${draft.targetNamespace}`,
      });
      onOpenChange(false);
    } catch (failure) {
      showErrorToast(failure);
    } finally {
      setSaving(false);
    }
  }

  return (
    <Sheet
      open={open}
      onOpenChange={(next) => {
        if (!next) setError(undefined);
        onOpenChange(next);
      }}
    >
      <SheetContent size="sm">
        <SheetHeader>
          <SheetTitle>Add link</SheetTitle>
          <SheetDescription>
            A link is one relationship that both namespaces can query from.
          </SheetDescription>
        </SheetHeader>

        <SheetBody>
          <div className="flex flex-col gap-4">
            <Field>
              <FieldLabel>From namespace</FieldLabel>
              <p className="text-cell font-mono">{namespace.name}</p>
            </Field>

            <Field>
              <FieldLabel>Forward label</FieldLabel>
              <Input
                value={draft.forwardLabel}
                autoComplete="off"
                spellCheck={false}
                placeholder="owner"
                aria-label="Forward label"
                aria-invalid={error === undefined ? undefined : true}
                ref={forwardRef}
                onChange={(event) => {
                  patch({ forwardLabel: event.target.value });
                }}
              />
              <FieldDescription>
                {`How ${namespace.name} names the target, for example todos.owner.`}
              </FieldDescription>
            </Field>

            <Field>
              <FieldLabel>Target namespace</FieldLabel>
              <Select
                value={draft.targetNamespace}
                onValueChange={(value) => {
                  if (typeof value === 'string' && value !== '') {
                    patch({ targetNamespace: value });
                  }
                }}
              >
                <SelectTrigger aria-label="Target namespace">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {namespaces.map((candidate) => (
                    <SelectItem key={candidate.name} value={candidate.name}>
                      {candidate.name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              {selfLink ? (
                <FieldDescription>
                  A self-link needs different forward and reverse labels.
                </FieldDescription>
              ) : null}
            </Field>

            <Field>
              <FieldLabel>Reverse label</FieldLabel>
              <Input
                value={draft.reverseLabel}
                autoComplete="off"
                spellCheck={false}
                placeholder={namespace.name}
                aria-label="Reverse label"
                onChange={(event) => {
                  patch({ reverseLabel: event.target.value });
                }}
              />
              <FieldDescription>
                {`How ${draft.targetNamespace} names the link back.`}
              </FieldDescription>
            </Field>

            <div className="grid gap-3 md:grid-cols-2">
              <Field>
                <FieldLabel>Forward cardinality</FieldLabel>
                <Select
                  value={draft.forwardCardinality}
                  onValueChange={(value) => {
                    if (value === 'one' || value === 'many') {
                      patch({
                        forwardCardinality: value,
                        cascade: value === 'one' ? draft.cascade : false,
                      });
                    }
                  }}
                >
                  <SelectTrigger aria-label="Forward cardinality">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="one">has one</SelectItem>
                    <SelectItem value="many">has many</SelectItem>
                  </SelectContent>
                </Select>
              </Field>

              <Field>
                <FieldLabel>Reverse cardinality</FieldLabel>
                <Select
                  value={draft.reverseCardinality}
                  onValueChange={(value) => {
                    if (value === 'one' || value === 'many') {
                      patch({ reverseCardinality: value });
                    }
                  }}
                >
                  <SelectTrigger aria-label="Reverse cardinality">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="one">has one</SelectItem>
                    <SelectItem value="many">has many</SelectItem>
                  </SelectContent>
                </Select>
              </Field>
            </div>

            <div className="flex items-start justify-between gap-4">
              <div className="flex min-w-0 flex-col gap-0.5">
                <p id={cascadeId} className="text-sm font-medium">
                  Cascade delete
                </p>
                <p className="text-muted-foreground text-xs">
                  {`When a linked ${draft.targetNamespace} entity is deleted, delete the ${namespace.name} entities that point at it.`}
                </p>
                {!cascadeAllowed ? (
                  <p className="text-muted-foreground text-xs">
                    Only a has-one forward link can cascade.
                  </p>
                ) : null}
              </div>
              <Switch
                aria-labelledby={cascadeId}
                checked={draft.cascade}
                disabled={!cascadeAllowed}
                onCheckedChange={(checked) => {
                  patch({ cascade: checked });
                }}
              />
            </div>

            {error === undefined ? null : (
              <p role="alert" className="text-destructive text-xs font-medium">
                {error}
              </p>
            )}
          </div>
        </SheetBody>

        <SheetFooter>
          <Button
            type="button"
            variant="secondary"
            disabled={saving}
            onClick={() => {
              onOpenChange(false);
            }}
          >
            Cancel
          </Button>
          <Button
            type="button"
            disabled={saving}
            onClick={() => {
              void save();
            }}
          >
            {saving ? 'Creating…' : 'Add link'}
          </Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}
