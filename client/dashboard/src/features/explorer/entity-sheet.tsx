import { ChevronDown, Link2, Plus, Save, Trash2, X } from 'lucide-react';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { CodeEditor } from '@/components/code-editor/code-editor';
import { CopyButton } from '@/components/data/copy-button';
import { IdChip } from '@/components/data/id-chip';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { ErrorState } from '@/components/feedback/error-state';
import { TypedConfirmDialog } from '@/components/feedback/typed-confirm-dialog';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { Field, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
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
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { linkedEntities } from '@/lib/explorer/entities';
import {
  clearDraft,
  NEW_ENTITY_DRAFT_ID,
  readDraft,
  writeDraft,
  type ExplorerDraft,
} from '@/lib/explorer/drafts';
import { useExplorerEntity } from '@/lib/explorer/rows';
import {
  collectEntityFields,
  deleteEntities,
  initialFieldValues,
  linkUpdateValue,
  newEntityId,
  planEntityWrite,
  runEntityWrite,
  toDateTimeLocal,
  type FieldValue,
  type FieldValues,
} from '@/lib/explorer/writes';
import type { AppInstantClient, AppLink, AppNamespace } from '@/lib/instant';
import { showErrorToast, toastManager } from '@/lib/toast';
import { LinkPickerSheet } from './link-picker-sheet';

const ENTITY_TABS = [
  { id: 'fields', label: 'Fields' },
  { id: 'links', label: 'Links' },
  { id: 'json', label: 'JSON' },
] as const;

type EntityTab = (typeof ENTITY_TABS)[number]['id'];

function isEntityTab(value: unknown): value is EntityTab {
  return ENTITY_TABS.some((entry) => entry.id === value);
}

interface LinkDraft {
  add: string[];
  remove: string[];
}

export interface EntitySheetProps {
  appId: string;
  namespace: AppNamespace;
  namespaces: ReadonlyArray<AppNamespace>;
  client: AppInstantClient;
  /** `null` opens the sheet for a new entity. */
  entityId: string | null;
  onOpenChange: (open: boolean) => void;
  /** `another` keeps the sheet open on a blank entity (C.6 split menu). */
  onSaved: (entityId: string, mode: 'close' | 'another') => void;
}

/**
 * C.6 entity sheet (`?entity=<id>` or `?new=1`): typed field controls, links,
 * read-only JSON, autosaved drafts, and a Save/Delete footer. Every write goes
 * through the app's Instant client, so webhooks and rules behave like an app
 * write.
 */
export function EntitySheet({
  appId,
  namespace,
  namespaces,
  client,
  entityId,
  onOpenChange,
  onSaved,
}: EntitySheetProps) {
  const isNew = entityId === null;
  const [generatedId] = useState(() => newEntityId());
  const targetId = entityId ?? generatedId;
  const draftId = isNew ? NEW_ENTITY_DRAFT_ID : targetId;
  const attributes = namespace.attributes;
  const linkLabels = useMemo(
    () => namespace.links.map((link) => link.label),
    [namespace.links],
  );

  const entityQuery = useExplorerEntity(
    client,
    isNew ? null : namespace.name,
    isNew ? null : targetId,
    linkLabels,
  );
  const entity = isNew ? null : entityQuery.entity;
  const ready = isNew || !entityQuery.isLoading;

  const [tab, setTab] = useState<EntityTab>('fields');
  const [fields, setFields] = useState<FieldValues>(() =>
    initialFieldValues(attributes, null),
  );
  const [linkDrafts, setLinkDrafts] = useState<Record<string, LinkDraft>>({});
  const [dirty, setDirty] = useState(false);
  const [showErrors, setShowErrors] = useState(false);
  const [saving, setSaving] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [discardOpen, setDiscardOpen] = useState(false);
  const [pickerLink, setPickerLink] = useState<AppLink | null>(null);
  const [storedDraft, setStoredDraft] = useState<ExplorerDraft | null>(null);
  const initializedFor = useRef('');
  const draftLoadedFor = useRef('');

  const orderedAttributes = useMemo(
    () =>
      attributes
        .filter((attribute) => attribute.name !== 'id')
        .sort(
          (left, right) => Number(right.isRequired) - Number(left.isRequired),
        ),
    [attributes],
  );

  // The live row is the source for the first render of the form; after that the
  // operator's edits win, so a live update never overwrites a draft.
  useEffect(() => {
    if (!ready) return;
    if (initializedFor.current === targetId) return;
    initializedFor.current = targetId;
    setFields(initialFieldValues(attributes, entity));
    setLinkDrafts({});
    setDirty(false);
    setShowErrors(false);
  }, [attributes, entity, ready, targetId]);

  useEffect(() => {
    if (!ready) return;
    if (draftLoadedFor.current === draftId) return;
    draftLoadedFor.current = draftId;
    setStoredDraft(readDraft(appId, namespace.name, draftId));
  }, [appId, draftId, namespace.name, ready]);

  // Autosave: sessionStorage per entity, only once the form has real edits.
  useEffect(() => {
    if (!dirty) return;
    const values: ExplorerDraft['values'] = {};
    const unset: string[] = [];
    const json: Record<string, string> = {};
    for (const attribute of attributes) {
      const field = fields[attribute.name];
      if (field === undefined) continue;
      if (field.unset) unset.push(attribute.name);
      if (field.value !== undefined) values[attribute.name] = field.value;
      if (attribute.type === 'json' && field.json !== undefined) {
        json[attribute.name] = field.json;
      }
    }
    writeDraft(appId, namespace.name, draftId, {
      values,
      unset,
      json,
      links: linkDrafts,
    });
  }, [appId, attributes, dirty, draftId, fields, linkDrafts, namespace.name]);

  const collected = useMemo(
    () => collectEntityFields(attributes, fields),
    [attributes, fields],
  );
  const hasLinks = namespace.links.some(
    (link) => linkedEntities(entity?.[link.label]).length > 0,
  );

  function updateField(name: string, patch: Partial<FieldValue>): void {
    setFields((current) => ({
      ...current,
      [name]: {
        ...(current[name] ?? { unset: false }),
        unset: false,
        ...patch,
      },
    }));
    setDirty(true);
  }

  function toggleUnset(name: string): void {
    setFields((current) => {
      const field = current[name];
      if (field === undefined) return current;
      return { ...current, [name]: { ...field, unset: !field.unset } };
    });
    setDirty(true);
  }

  function linkDraft(linkId: string): LinkDraft {
    return linkDrafts[linkId] ?? { add: [], remove: [] };
  }

  function updateLinkDraft(linkId: string, next: LinkDraft): void {
    setLinkDrafts((current) => ({ ...current, [linkId]: next }));
    setDirty(true);
  }

  function removeLink(link: AppLink, id: string): void {
    const current = linkDraft(link.id);
    if (current.add.includes(id)) {
      updateLinkDraft(link.id, {
        ...current,
        add: current.add.filter((candidate) => candidate !== id),
      });
      return;
    }
    updateLinkDraft(link.id, {
      ...current,
      remove: [...new Set([...current.remove, id])],
    });
  }

  function restoreLink(link: AppLink, id: string): void {
    const current = linkDraft(link.id);
    updateLinkDraft(link.id, {
      ...current,
      remove: current.remove.filter((candidate) => candidate !== id),
    });
  }

  const save = useCallback(
    async (mode: 'close' | 'another'): Promise<void> => {
      if (saving) return;
      const next = collectEntityFields(attributes, fields);
      if (Object.keys(next.errors).length > 0) {
        setShowErrors(true);
        return;
      }
      const addLinks: Record<string, string | string[]> = {};
      const removeLinks: Record<string, string | string[]> = {};
      for (const link of namespace.links) {
        const staged = linkDrafts[link.id];
        if (staged === undefined) continue;
        if (staged.add.length > 0) {
          addLinks[link.label] = linkUpdateValue(link.cardinality, staged.add);
        }
        if (staged.remove.length > 0) {
          removeLinks[link.label] = linkUpdateValue(
            link.cardinality,
            staged.remove,
          );
        }
      }

      setSaving(true);
      try {
        const plan = planEntityWrite(client, {
          namespace: namespace.name,
          entityId: targetId,
          values: next.values,
          unset: next.unset,
          addLinks,
          removeLinks,
        });
        await runEntityWrite(client, plan);
        clearDraft(appId, namespace.name, draftId);
        setDirty(false);
        toastManager.add({
          type: 'success',
          title: isNew ? 'Entity created' : 'Entity saved',
        });
        onSaved(targetId, mode);
      } catch (failure) {
        showErrorToast(failure);
      } finally {
        setSaving(false);
      }
    },
    [
      appId,
      attributes,
      client,
      draftId,
      fields,
      isNew,
      linkDrafts,
      namespace.links,
      namespace.name,
      onSaved,
      saving,
      targetId,
    ],
  );

  async function removeEntity(): Promise<void> {
    if (deleting || isNew) return;
    setDeleting(true);
    try {
      await deleteEntities(client, namespace.name, [targetId]);
      clearDraft(appId, namespace.name, draftId);
      setDirty(false);
      toastManager.add({ type: 'success', title: 'Entity deleted' });
      onOpenChange(false);
    } catch (failure) {
      showErrorToast(failure);
    } finally {
      setDeleting(false);
    }
  }

  function requestClose(): void {
    if (dirty) {
      setDiscardOpen(true);
      return;
    }
    clearDraft(appId, namespace.name, draftId);
    onOpenChange(false);
  }

  function restoreDraft(): void {
    if (storedDraft === null) return;
    const next = { ...fields };
    for (const attribute of orderedAttributes) {
      const field = next[attribute.name] ?? { unset: false };
      if (storedDraft.unset.includes(attribute.name)) {
        next[attribute.name] = { ...field, unset: true };
      } else {
        const saved = storedDraft.values[attribute.name];
        if (saved !== undefined) {
          next[attribute.name] = {
            ...field,
            unset: false,
            value: saved,
          };
        }
      }
      const savedJson = storedDraft.json[attribute.name];
      if (attribute.type === 'json' && savedJson !== undefined) {
        next[attribute.name] = {
          ...(next[attribute.name] ?? { unset: false }),
          unset: false,
          json: savedJson,
        };
      }
    }
    setFields(next);
    setLinkDrafts(storedDraft.links);
    setDirty(true);
    setStoredDraft(null);
  }

  function discardStoredDraft(): void {
    clearDraft(appId, namespace.name, draftId);
    setStoredDraft(null);
  }

  const jsonPayload = useMemo(() => {
    const source = entity ?? { id: targetId, ...collected.values };
    return JSON.stringify(source, null, 2);
  }, [collected.values, entity, targetId]);

  const pickerTarget =
    pickerLink === null
      ? undefined
      : namespaces.find(
          (candidate) => candidate.name === pickerLink.targetNamespace,
        );

  if (entityQuery.error !== null) {
    return (
      <Sheet
        open
        onOpenChange={(next) => {
          if (!next) requestClose();
        }}
      >
        <SheetContent size="md">
          <SheetHeader>
            <SheetTitle>Entity</SheetTitle>
            <SheetDescription>Could not load this entity.</SheetDescription>
          </SheetHeader>
          <SheetBody>
            <ErrorState
              thing="entity"
              message={entityQuery.error}
              onRetry={() => {
                onOpenChange(false);
              }}
            />
          </SheetBody>
        </SheetContent>
      </Sheet>
    );
  }

  return (
    <>
      <Sheet
        open
        onOpenChange={(next) => {
          if (!next) requestClose();
        }}
      >
        <SheetContent
          size="md"
          onKeyDown={(event) => {
            if (!(event.metaKey || event.ctrlKey)) return;
            if (event.key.toLowerCase() !== 's') return;
            event.preventDefault();
            void save('close');
          }}
        >
          <SheetHeader>
            <SheetTitle>{`${namespace.name} entity`}</SheetTitle>
            <SheetDescription>
              {isNew
                ? 'New entity. Fields marked required must be filled in.'
                : 'Edit this entity. Every change is saved through the app.'}
            </SheetDescription>
            {isNew ? null : (
              <span className="mt-1 flex flex-wrap items-center gap-2">
                <IdChip value={targetId} />
                <CopyButton value={targetId} aria-label="Copy entity ID" />
              </span>
            )}
          </SheetHeader>

          <Tabs
            value={tab}
            className="flex min-h-0 flex-1 flex-col"
            onValueChange={(value) => {
              if (isEntityTab(value)) setTab(value);
            }}
          >
            <TabsList aria-label="Entity" className="mx-4">
              {ENTITY_TABS.map((entry) => (
                <TabsTrigger key={entry.id} value={entry.id}>
                  {entry.label}
                </TabsTrigger>
              ))}
            </TabsList>

            <SheetBody className="flex flex-col gap-4">
              {storedDraft === null ? null : (
                <div
                  role="status"
                  className="border-border bg-muted flex flex-wrap items-center gap-2 rounded-md border px-3 py-2 text-sm"
                >
                  <p className="min-w-0 flex-1">
                    You have unsaved changes from earlier
                  </p>
                  <Button
                    type="button"
                    size="sm"
                    variant="secondary"
                    onClick={restoreDraft}
                  >
                    Restore
                  </Button>
                  <Button
                    type="button"
                    size="sm"
                    variant="ghost"
                    onClick={discardStoredDraft}
                  >
                    Discard
                  </Button>
                </div>
              )}

              <TabsContent value="fields" className="flex flex-col gap-4">
                {orderedAttributes.map((attribute) => {
                  const field = fields[attribute.name] ?? { unset: false };
                  const error = showErrors
                    ? collected.errors[attribute.name]
                    : undefined;
                  return (
                    <Field key={attribute.name}>
                      <div className="flex items-center justify-between gap-2">
                        <FieldLabel className="font-mono">
                          {attribute.name}
                          {attribute.isRequired ? (
                            <span
                              aria-hidden="true"
                              className="text-destructive ml-1"
                            >
                              *
                            </span>
                          ) : null}
                        </FieldLabel>
                        <div className="flex items-center gap-2">
                          <Badge tone="neutral">{attribute.type}</Badge>
                          <Button
                            type="button"
                            variant="link"
                            size="sm"
                            aria-label={
                              field.unset
                                ? `Restore ${attribute.name}`
                                : `Clear ${attribute.name}`
                            }
                            onClick={() => {
                              toggleUnset(attribute.name);
                            }}
                          >
                            {field.unset ? 'Restore value' : 'Clear (unset)'}
                          </Button>
                        </div>
                      </div>

                      {field.unset ? (
                        <p className="text-muted-foreground text-sm italic">
                          unset
                        </p>
                      ) : attribute.type === 'json' ? (
                        <CodeEditor
                          mode="json"
                          aria-label={attribute.name}
                          height="160px"
                          value={field.json ?? ''}
                          onChange={(value) => {
                            updateField(attribute.name, { json: value });
                          }}
                        />
                      ) : attribute.type === 'boolean' ? (
                        <span className="flex items-center gap-2">
                          <Switch
                            aria-label={attribute.name}
                            checked={field.value === true}
                            onCheckedChange={(checked) => {
                              updateField(attribute.name, {
                                value: checked,
                              });
                            }}
                          />
                          <span
                            className={
                              field.value === undefined
                                ? 'text-muted-foreground text-sm italic'
                                : 'text-muted-foreground text-sm'
                            }
                          >
                            {field.value === undefined
                              ? 'unset'
                              : field.value === true
                                ? 'true'
                                : 'false'}
                          </span>
                        </span>
                      ) : attribute.type === 'date' ? (
                        <Input
                          type="datetime-local"
                          aria-label={attribute.name}
                          value={
                            typeof field.value === 'string'
                              ? field.value
                              : toDateTimeLocal(field.value)
                          }
                          onChange={(event) => {
                            updateField(attribute.name, {
                              value: event.target.value,
                            });
                          }}
                        />
                      ) : attribute.type === 'number' ? (
                        <Input
                          type="number"
                          aria-label={attribute.name}
                          value={
                            typeof field.value === 'number' ||
                            typeof field.value === 'string'
                              ? field.value
                              : ''
                          }
                          onChange={(event) => {
                            updateField(attribute.name, {
                              value: event.target.value,
                            });
                          }}
                        />
                      ) : (
                        <Input
                          aria-label={attribute.name}
                          value={
                            typeof field.value === 'string' ? field.value : ''
                          }
                          onChange={(event) => {
                            updateField(attribute.name, {
                              value: event.target.value,
                            });
                          }}
                        />
                      )}

                      {error === undefined ? null : (
                        <p
                          role="alert"
                          className="text-destructive text-xs font-medium"
                        >
                          {error}
                        </p>
                      )}
                    </Field>
                  );
                })}
              </TabsContent>

              <TabsContent value="links" className="flex flex-col gap-4">
                {namespace.links.length === 0 ? (
                  <p className="text-muted-foreground text-sm">
                    This namespace has no links.
                  </p>
                ) : (
                  namespace.links.map((link) => {
                    const staged = linkDraft(link.id);
                    const current = linkedEntities(entity?.[link.label]);
                    const visible = current.filter(
                      (target) => !staged.remove.includes(target.id),
                    );
                    return (
                      <div
                        key={link.id}
                        className="flex flex-col gap-2 rounded-md border p-3"
                      >
                        <div className="flex flex-wrap items-center justify-between gap-2">
                          <div className="flex items-center gap-2">
                            <Link2 aria-hidden="true" className="size-4" />
                            <span className="font-mono text-sm">
                              {link.label}
                            </span>
                            <Badge tone="accent">
                              {link.cardinality === 'one' ? 'one' : 'many'}
                            </Badge>
                          </div>
                          <Button
                            type="button"
                            size="sm"
                            variant="secondary"
                            onClick={() => {
                              setPickerLink(link);
                            }}
                          >
                            <Plus aria-hidden="true" />
                            Add link
                          </Button>
                        </div>

                        <ul className="flex flex-col gap-1">
                          {visible.map((target) => (
                            <li
                              key={target.id}
                              className="flex items-center gap-2 text-sm"
                            >
                              <IdChip value={target.id} tabIndex={-1} />
                              <span className="min-w-0 flex-1 truncate">
                                {target.label}
                              </span>
                              <Button
                                type="button"
                                variant="ghost"
                                size="sm"
                                aria-label={`Remove link to ${target.label}`}
                                onClick={() => {
                                  removeLink(link, target.id);
                                }}
                              >
                                <X aria-hidden="true" />
                                Remove
                              </Button>
                            </li>
                          ))}
                          {staged.add.map((id) => (
                            <li
                              key={`added-${id}`}
                              className="flex items-center gap-2 text-sm"
                            >
                              <IdChip value={id} tabIndex={-1} />
                              <Badge tone="neutral">new</Badge>
                              <Button
                                type="button"
                                variant="ghost"
                                size="sm"
                                aria-label={`Remove new link ${id}`}
                                onClick={() => {
                                  removeLink(link, id);
                                }}
                              >
                                <X aria-hidden="true" />
                                Remove
                              </Button>
                            </li>
                          ))}
                          {staged.remove.map((id) => (
                            <li
                              key={`removed-${id}`}
                              className="flex items-center gap-2 text-sm"
                            >
                              <code className="text-muted-foreground text-xs line-through">
                                {id}
                              </code>
                              <Button
                                type="button"
                                variant="link"
                                size="sm"
                                aria-label={`Restore link ${id}`}
                                onClick={() => {
                                  restoreLink(link, id);
                                }}
                              >
                                Undo
                              </Button>
                            </li>
                          ))}
                          {visible.length === 0 &&
                          staged.add.length === 0 &&
                          staged.remove.length === 0 ? (
                            <li className="text-muted-foreground text-sm italic">
                              unset
                            </li>
                          ) : null}
                        </ul>
                      </div>
                    );
                  })
                )}
              </TabsContent>

              <TabsContent value="json" className="flex flex-col gap-2">
                <div className="flex items-center justify-between gap-2">
                  <p className="text-muted-foreground text-sm">
                    Read-only entity JSON.
                  </p>
                  <CopyButton
                    value={jsonPayload}
                    label="Copy JSON"
                    aria-label="Copy entity JSON"
                  />
                </div>
                <CodeEditor
                  mode="json"
                  aria-label="Entity JSON"
                  value={jsonPayload}
                  readOnly
                  height="360px"
                />
              </TabsContent>
            </SheetBody>
          </Tabs>

          <SheetFooter className="justify-between">
            <div className="flex min-w-0 flex-col gap-1">
              {Object.keys(collected.errors).length > 0 && showErrors ? (
                <p role="alert" className="text-destructive text-xs">
                  Fix the highlighted fields before saving.
                </p>
              ) : null}
            </div>
            <div className="flex items-center gap-2">
              <Button
                type="button"
                variant="secondary"
                disabled={saving || deleting}
                onClick={requestClose}
              >
                Cancel
              </Button>
              <Button
                type="button"
                disabled={saving || deleting}
                onClick={() => {
                  void save('close');
                }}
              >
                <Save aria-hidden="true" />
                {saving ? 'Saving…' : 'Save'}
              </Button>
              <DropdownMenu>
                <DropdownMenuTrigger
                  render={<Button type="button" variant="secondary" />}
                  aria-label="More save actions"
                  disabled={saving || deleting}
                >
                  <ChevronDown aria-hidden="true" />
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end" className="w-56">
                  <DropdownMenuItem
                    onClick={() => {
                      void save('another');
                    }}
                  >
                    Save and add another
                  </DropdownMenuItem>
                  <DropdownMenuSeparator />
                  <DropdownMenuItem
                    className="text-destructive"
                    disabled={isNew}
                    onClick={() => {
                      setConfirmDelete(true);
                    }}
                  >
                    <Trash2 aria-hidden="true" />
                    Delete entity
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
            </div>
          </SheetFooter>
        </SheetContent>
      </Sheet>

      {pickerLink === null ? null : (
        <LinkPickerSheet
          open
          onOpenChange={(next) => {
            if (!next) setPickerLink(null);
          }}
          client={client}
          link={pickerLink}
          target={pickerTarget}
          exclude={[
            ...linkedEntities(entity?.[pickerLink.label])
              .filter(
                (target) =>
                  !linkDraft(pickerLink.id).remove.includes(target.id),
              )
              .map((target) => target.id),
            ...linkDraft(pickerLink.id).add,
          ]}
          onConfirm={(ids) => {
            const staged = linkDraft(pickerLink.id);
            updateLinkDraft(pickerLink.id, {
              add: [...new Set([...staged.add, ...ids])],
              remove: staged.remove.filter((id) => !ids.includes(id)),
            });
            setPickerLink(null);
          }}
        />
      )}

      {confirmDelete && !isNew ? (
        hasLinks ? (
          <TypedConfirmDialog
            open
            onOpenChange={setConfirmDelete}
            title="Delete this entity?"
            description="It has links. Deleting it also removes those links from the other side."
            consequences={[
              'The entity and its attribute values are removed.',
              'Linked entities stay, but the links to this one disappear.',
            ]}
            confirmText={targetId}
            confirmLabel="Delete entity"
            confirming={deleting}
            onConfirm={() => {
              void removeEntity();
            }}
          />
        ) : (
          <ConfirmDialog
            open
            onOpenChange={setConfirmDelete}
            title="Delete this entity?"
            description={`${namespace.name} entity ${targetId} is removed from the app.`}
            confirmLabel="Delete entity"
            confirming={deleting}
            onConfirm={() => {
              void removeEntity();
            }}
          />
        )
      ) : null}

      <ConfirmDialog
        open={discardOpen}
        onOpenChange={setDiscardOpen}
        title="Discard unsaved changes?"
        description="The changes to this entity have not been saved."
        confirmLabel="Discard"
        cancelLabel="Keep editing"
        onConfirm={() => {
          clearDraft(appId, namespace.name, draftId);
          setDirty(false);
          setDiscardOpen(false);
          onOpenChange(false);
        }}
      />
    </>
  );
}
