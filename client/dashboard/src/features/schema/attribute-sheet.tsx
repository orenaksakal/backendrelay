import { useEffect, useId, useMemo, useRef, useState } from 'react';
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
import { Button } from '@/components/ui/button';
import type { IndexingJob } from '@/lib/api/endpoints/apps';
import type {
  AppAttribute,
  AppInstantClient,
  AppNamespace,
  AttributeType,
  InstantAttr,
} from '@/lib/instant';
import {
  ATTRIBUTE_TYPE_OPTIONS,
  addAttributeOps,
  attributeNameProblem,
  draftFromAttribute,
  pendingConstraintJobs,
  renameAttributeOps,
  type AttributeDraft,
} from '@/lib/schema/attributes';
import { createIndexingJob } from '@/lib/schema/jobs';
import { showErrorToast, toastManager } from '@/lib/toast';

export interface AttributeSheetProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  appId: string;
  namespace: AppNamespace;
  /** `null` opens the sheet for a new attribute. */
  attribute: AppAttribute | null;
  /** The raw catalog entry, for the forward identity id on rename. */
  rawAttribute: InstantAttr | undefined;
  /** Attribute names in this namespace, for the duplicate check. */
  existingNames: ReadonlyArray<string>;
  client: AppInstantClient;
  /** Every constraint job the save started, so the table can show progress. */
  onJobsStarted: (jobs: ReadonlyArray<IndexingJob>) => void;
}

function emptyDraft(): AttributeDraft {
  return {
    name: '',
    type: 'string',
    required: false,
    unique: false,
    indexed: false,
  };
}

/**
 * C.7 "Add attribute" / "Edit" sheet (480px): name, type and the three
 * constraints. A new attribute lands in one `add-attr`; an edit renames with
 * `update-attr` and asks the server for a validating indexing job for each
 * changed constraint, so existing data is never rewritten silently.
 */
export function AttributeSheet({
  open,
  onOpenChange,
  appId,
  namespace,
  attribute,
  rawAttribute,
  existingNames,
  client,
  onJobsStarted,
}: AttributeSheetProps) {
  const isNew = attribute === null;
  const [draft, setDraft] = useState<AttributeDraft>(() =>
    attribute === null ? emptyDraft() : draftFromAttribute(attribute),
  );
  const [error, setError] = useState<string>();
  const [saving, setSaving] = useState(false);
  const requiredId = useId();
  const uniqueId = useId();
  const indexedId = useId();
  const nameRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (open) nameRef.current?.focus();
  }, [open]);

  const previous = useMemo(
    () => (attribute === null ? null : draftFromAttribute(attribute)),
    [attribute],
  );
  const changed =
    previous === null ||
    previous.name !== draft.name.trim() ||
    previous.type !== draft.type ||
    previous.required !== draft.required ||
    previous.unique !== draft.unique ||
    previous.indexed !== draft.indexed;

  function patch(next: Partial<AttributeDraft>): void {
    setDraft((current) => ({ ...current, ...next }));
    setError(undefined);
  }

  async function save(): Promise<void> {
    if (saving) return;
    const problem = attributeNameProblem(
      draft.name,
      existingNames,
      attribute?.name,
    );
    if (problem !== undefined) {
      setError(problem);
      return;
    }
    setSaving(true);
    setError(undefined);
    const name = draft.name.trim();
    try {
      if (attribute === null) {
        await client.core._reactor.pushOps(
          addAttributeOps(namespace.name, { ...draft, name }),
        );
        toastManager.add({ type: 'success', title: `Added ${name}` });
        onOpenChange(false);
        return;
      }

      const ops = renameAttributeOps(attribute, name, rawAttribute);
      if (ops.length > 0) await client.core._reactor.pushOps(ops);

      const jobs = pendingConstraintJobs(previous, { ...draft, name });
      const started: IndexingJob[] = [];
      for (const job of jobs) {
        started.push(await createIndexingJob(appId, attribute.id, job));
      }
      if (started.length > 0) onJobsStarted(started);
      toastManager.add({
        type: 'success',
        title:
          started.length > 0
            ? `${attribute.namespace}.${name} saved; ${started.length === 1 ? 'a check is' : 'checks are'} running`
            : `${attribute.namespace}.${name} saved`,
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
          <SheetTitle>
            {attribute === null ? 'Add attribute' : `Edit ${attribute.name}`}
          </SheetTitle>
          <SheetDescription>
            {`${namespace.name} attributes are enforced by the server for every write.`}
          </SheetDescription>
        </SheetHeader>

        <SheetBody>
          <div className="flex flex-col gap-4">
            <Field>
              <FieldLabel>Name</FieldLabel>
              <Input
                value={draft.name}
                autoComplete="off"
                spellCheck={false}
                placeholder="title"
                aria-label="Attribute name"
                aria-invalid={error === undefined ? undefined : true}
                ref={nameRef}
                onChange={(event) => {
                  patch({ name: event.target.value });
                }}
                onKeyDown={(event) => {
                  if (event.key !== 'Enter') return;
                  event.preventDefault();
                  void save();
                }}
              />
              {error === undefined ? null : (
                <p
                  role="alert"
                  className="text-destructive text-xs font-medium"
                >
                  {error}
                </p>
              )}
            </Field>

            <Field>
              <FieldLabel>Type</FieldLabel>
              <Select
                value={draft.type}
                onValueChange={(value) => {
                  patch({ type: value as AttributeType });
                }}
              >
                <SelectTrigger aria-label="Attribute type">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {ATTRIBUTE_TYPE_OPTIONS.map((option) => (
                    <SelectItem key={option.value} value={option.value}>
                      {option.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <FieldDescription>
                {ATTRIBUTE_TYPE_OPTIONS.find(
                  (option) => option.value === draft.type,
                )?.description ?? ''}
              </FieldDescription>
            </Field>

            <div className="flex flex-col gap-3">
              <ConstraintRow
                id={requiredId}
                label="Required"
                description="Every entity must set this attribute before it can be saved."
                checked={draft.required}
                onCheckedChange={(checked) => {
                  patch({ required: checked });
                }}
              />
              <ConstraintRow
                id={uniqueId}
                label="Unique"
                description="No two entities may share this value."
                checked={draft.unique}
                onCheckedChange={(checked) => {
                  patch({ unique: checked });
                }}
              />
              <ConstraintRow
                id={indexedId}
                label="Indexed"
                description="Makes filters and lookups on this attribute fast."
                checked={draft.indexed}
                onCheckedChange={(checked) => {
                  patch({ indexed: checked });
                }}
              />
            </div>

            {!isNew && changed ? (
              <p className="text-muted-foreground text-xs">
                Changing a constraint checks the existing entities first; the
                attribute keeps its current rules until the check passes.
              </p>
            ) : null}
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
            disabled={saving || !changed}
            onClick={() => {
              void save();
            }}
          >
            {saving ? 'Saving…' : isNew ? 'Add attribute' : 'Save attribute'}
          </Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}

interface ConstraintRowProps {
  id: string;
  label: string;
  description: string;
  checked: boolean;
  onCheckedChange: (checked: boolean) => void;
}

function ConstraintRow({
  id,
  label,
  description,
  checked,
  onCheckedChange,
}: ConstraintRowProps) {
  return (
    <div className="flex items-start justify-between gap-4">
      <div className="flex min-w-0 flex-col gap-0.5">
        <p id={id} className="text-sm font-medium">
          {label}
        </p>
        <p className="text-muted-foreground text-xs">{description}</p>
      </div>
      <Switch
        aria-labelledby={id}
        checked={checked}
        onCheckedChange={onCheckedChange}
      />
    </div>
  );
}
