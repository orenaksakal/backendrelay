import { Plus, TriangleAlert } from 'lucide-react';
import { useEffect, useMemo, useRef, useState } from 'react';
import { SectionCard } from '@/components/layout/section-card';
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
import type { AppSchema } from '@/lib/instant';
import {
  actionRowKey,
  addNamespace,
  editorNamespaces,
  namespaceBlock,
  namespaceUnsupportedReason,
  ownExpression,
  setAllowExpression,
} from '@/lib/permissions/document';
import {
  PERMISSION_ACTIONS,
  deniesEveryone,
  namespaceAllowsPublicWrites,
  type PermissionAction,
} from '@/lib/permissions/effective';
import type { Rules } from '@/lib/permissions/model';
import {
  errorPath,
  rowErrors,
  type DraftValidation,
} from '@/lib/permissions/validation';
import { ActionRow } from './action-row';
import { AdvancedSection } from './advanced-section';
import { TemplatesMenu } from './templates-menu';

export interface PermissionsEditorProps {
  rules: Rules;
  schema: AppSchema | undefined;
  validation: DraftValidation;
  dirty: boolean;
  /** URL-selected namespace (`?ns=`), when the Overview opened one. */
  namespace?: string | null;
  /** The action the Overview cell pointed at, highlighted in the editor. */
  focusAction?: PermissionAction | null;
  onChange: (rules: Rules) => void;
  onNamespaceChange?: (namespace: string) => void;
  /** Present only for role A and above; Test-as needs the admin token. */
  onOpenTest?: () => void;
  onOpenJson: () => void;
}

const NAMESPACE_NAME = /^[A-Za-z0-9_-]+$/;
const DEFAULT_NAMESPACE = '$default';

function initialNamespace(
  namespaces: readonly string[],
  schemaNamespaces: readonly string[],
): string {
  return (
    schemaNamespaces.find(
      (name) => !name.startsWith('$') && namespaces.includes(name),
    ) ??
    namespaces.find((name) => name !== DEFAULT_NAMESPACE) ??
    DEFAULT_NAMESPACE
  );
}

/**
 * C.10 Editor view: namespace sidebar, one row per object action, the advanced
 * link/unlink, field and bind rows, and the effective-state chips. The draft is
 * the single source of truth; the JSON view writes the same document.
 */
export function PermissionsEditor({
  rules,
  schema,
  validation,
  dirty,
  namespace,
  focusAction,
  onChange,
  onNamespaceChange,
  onOpenTest,
  onOpenJson,
}: PermissionsEditorProps) {
  const schemaNamespaces = useMemo(
    () => schema?.namespaces.map((entry) => entry.name) ?? [],
    [schema],
  );
  const namespaces = useMemo(
    () => editorNamespaces(rules, schemaNamespaces),
    [rules, schemaNamespaces],
  );
  const [selected, setSelected] = useState(() =>
    namespace !== null &&
    namespace !== undefined &&
    namespaces.includes(namespace)
      ? namespace
      : initialNamespace(namespaces, schemaNamespaces),
  );
  const pickedInitial = useRef(schemaNamespaces.length > 0);
  const [locked, setLocked] = useState<Map<string, string | null>>(
    () => new Map(),
  );
  const [addOpen, setAddOpen] = useState(false);
  const [newName, setNewName] = useState('');

  // The schema arrives after the first render; re-pick the default once so the
  // first schema namespace wins over the schema-less alphabetical first one.
  useEffect(() => {
    if (pickedInitial.current) return;
    if (schemaNamespaces.length === 0) return;
    pickedInitial.current = true;
    if (namespace !== null && namespace !== undefined) return;
    // eslint-disable-next-line react-hooks/set-state-in-effect -- the default namespace waits for the schema
    setSelected(initialNamespace(namespaces, schemaNamespaces));
  }, [namespaces, namespace, schemaNamespaces]);

  // A namespace can disappear when the JSON view resets or restores the draft.
  useEffect(() => {
    if (!namespaces.includes(selected)) {
      // eslint-disable-next-line react-hooks/set-state-in-effect -- a namespace can disappear when the draft resets
      setSelected(initialNamespace(namespaces, schemaNamespaces));
    }
  }, [namespaces, schemaNamespaces, selected]);

  // `?ns=` changes when the Overview opens a cell or the URL is restored.
  useEffect(() => {
    if (namespace === null || namespace === undefined) return;
    if (namespace !== selected && namespaces.includes(namespace)) {
      // eslint-disable-next-line react-hooks/set-state-in-effect -- the URL is the source of truth for the selected namespace
      setSelected(namespace);
    }
  }, [namespace, namespaces, selected]);

  const unsupported = namespaceUnsupportedReason(rules, selected);
  const readOnly = unsupported !== null;
  const errors = rowErrors(
    validation.state === 'invalid' ? validation.errors : [],
  );
  const rateLimits = namespaceBlock(rules, '$rateLimits');
  const rateLimitNames = rateLimits === null ? [] : Object.keys(rateLimits);
  const duplicate = namespaces.includes(newName) || newName === '$rateLimits';
  const nameProblem =
    newName === ''
      ? undefined
      : !NAMESPACE_NAME.test(newName)
        ? 'Use letters, numbers, underscores and hyphens.'
        : duplicate
          ? 'That namespace already exists.'
          : undefined;

  function toggleLock(action: PermissionAction, previous: string | null): void {
    const key = actionRowKey(selected, action);
    const current = ownExpression(rules, selected, action);
    const isLocked = current !== null && deniesEveryone(current);

    if (isLocked) {
      const restore = locked.get(key);
      onChange(
        setAllowExpression(
          rules,
          selected,
          action,
          restore === undefined || restore === null ? null : restore,
        ),
      );
      setLocked((previousMap) => {
        const next = new Map(previousMap);
        next.delete(key);
        return next;
      });
      return;
    }

    setLocked((previousMap) => {
      const next = new Map(previousMap);
      next.set(key, previous);
      return next;
    });
    onChange(setAllowExpression(rules, selected, action, 'false'));
  }

  return (
    <div className="flex flex-col gap-4 lg:flex-row lg:items-start">
      <nav
        aria-label="Permission namespaces"
        className="border-border bg-card flex shrink-0 flex-col gap-1 rounded-lg border p-2 lg:w-64"
      >
        {namespaces.map((name) => {
          const warns = namespaceAllowsPublicWrites(rules, name);
          return (
            <button
              key={name}
              type="button"
              aria-current={name === selected ? 'page' : undefined}
              className={`flex min-h-8 items-center gap-2 rounded-md px-2 text-left font-mono text-sm ${
                name === selected
                  ? 'bg-accent/12 text-accent'
                  : 'text-foreground hover:bg-muted'
              }`}
              onClick={() => {
                setSelected(name);
                onNamespaceChange?.(name);
              }}
            >
              <span className="min-w-0 flex-1 truncate">{name}</span>
              {warns ? (
                <>
                  <TriangleAlert
                    className="text-warning size-3.5 shrink-0"
                    aria-hidden="true"
                  />
                  <span className="sr-only">Allows public writes</span>
                </>
              ) : null}
            </button>
          );
        })}
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="mt-1 justify-start"
          onClick={() => {
            setNewName('');
            setAddOpen(true);
          }}
        >
          <Plus className="size-4" aria-hidden="true" />
          Add namespace
        </Button>
      </nav>

      <div className="min-w-0 flex-1">
        <SectionCard
          title={selected}
          description={
            selected === DEFAULT_NAMESPACE
              ? 'Applied to every namespace without its own rule.'
              : 'Rules for this namespace only.'
          }
          actions={
            <div className="flex items-center gap-2">
              {onOpenTest === undefined ? null : (
                <Button
                  type="button"
                  variant="secondary"
                  size="sm"
                  onClick={onOpenTest}
                >
                  Test as…
                </Button>
              )}
              <TemplatesMenu
                namespace={selected}
                rules={rules}
                disabled={readOnly}
                onChange={onChange}
              />
            </div>
          }
        >
          <div className="flex flex-col gap-3">
            {dirty && validation.state === 'checking' ? (
              <p role="status" className="text-muted-foreground text-xs">
                Checking rules…
              </p>
            ) : null}
            {dirty && validation.state === 'valid' ? (
              <p role="status" className="text-success text-xs">
                The server accepted this draft.
              </p>
            ) : null}
            {validation.state === 'invalid' && errors.unattached.length > 0 ? (
              <div
                role="alert"
                className="border-destructive/40 bg-destructive/5 rounded-md border p-3 text-sm"
              >
                <p className="text-destructive font-medium">
                  The server rejected this draft.
                </p>
                <ul className="mt-1 flex flex-col gap-1">
                  {errors.unattached.map((error, index) => (
                    <li key={`${errorPath(error)}-${String(index)}`}>
                      <span className="font-mono text-xs">
                        {errorPath(error)}
                      </span>
                      : {error.message}
                    </li>
                  ))}
                </ul>
              </div>
            ) : null}

            {unsupported !== null ? (
              <div
                role="alert"
                className="border-warning/40 bg-warning/5 rounded-md border p-3 text-sm"
              >
                <p className="font-medium">
                  This namespace uses settings the visual editor doesn&apos;t
                  support. Edit it in JSON.
                </p>
                <p className="text-muted-foreground mt-1 text-xs">
                  {unsupported}
                </p>
                <Button
                  type="button"
                  variant="secondary"
                  size="sm"
                  className="mt-2"
                  onClick={onOpenJson}
                >
                  Edit in JSON
                </Button>
              </div>
            ) : null}

            {PERMISSION_ACTIONS.map((action) => (
              <ActionRow
                key={action}
                namespace={selected}
                action={action}
                rules={rules}
                schema={schema}
                readOnly={readOnly}
                highlighted={focusAction === action && selected === namespace}
                serverError={
                  errors.byRow.get(actionRowKey(selected, action))?.[0]
                }
                onChange={onChange}
                onToggleLock={(previous) => {
                  toggleLock(action, previous);
                }}
              />
            ))}

            <AdvancedSection
              namespace={selected}
              rules={rules}
              schema={schema}
              readOnly={readOnly}
              errors={errors.byRow}
              onChange={onChange}
            />

            {selected === DEFAULT_NAMESPACE ? (
              <div className="border-border flex flex-col gap-2 border-t pt-3 md:flex-row md:items-start md:justify-between">
                <div>
                  <h3 className="text-section">Rate limits</h3>
                  <p className="text-muted-foreground text-xs">
                    Editable only in JSON.{' '}
                    {rateLimitNames.length === 0
                      ? 'No rate limits are configured.'
                      : `Configured: ${rateLimitNames.join(', ')}.`}
                  </p>
                </div>
                <Button
                  type="button"
                  variant="secondary"
                  size="sm"
                  onClick={onOpenJson}
                >
                  Edit in JSON
                </Button>
              </div>
            ) : null}
          </div>
        </SectionCard>
      </div>

      <Dialog
        open={addOpen}
        onOpenChange={(open) => {
          setAddOpen(open);
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Add namespace</DialogTitle>
            <DialogDescription>
              It starts with no rules. A user namespace then allows everyone
              until you add a rule.
            </DialogDescription>
          </DialogHeader>
          <div className="mt-3">
            <Input
              aria-label="Namespace name"
              placeholder="todos"
              value={newName}
              aria-invalid={nameProblem !== undefined}
              onChange={(event) => {
                setNewName(event.target.value.trim());
              }}
            />
            {nameProblem !== undefined ? (
              <p role="alert" className="text-destructive mt-1 text-xs">
                {nameProblem}
              </p>
            ) : null}
          </div>
          <DialogFooter>
            <Button
              type="button"
              variant="secondary"
              onClick={() => {
                setAddOpen(false);
              }}
            >
              Cancel
            </Button>
            <Button
              type="button"
              disabled={newName === '' || nameProblem !== undefined}
              onClick={() => {
                onChange(addNamespace(rules, newName));
                setSelected(newName);
                setAddOpen(false);
              }}
            >
              Add namespace
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
