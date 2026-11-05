import { ChevronRight, Plus, Trash2 } from 'lucide-react';
import { useState } from 'react';
import { IconButton } from '@/components/layout/icon-button';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import type { AppSchema } from '@/lib/instant';
import { celCompletionSource } from '@/lib/permissions/cel';
import {
  bindRowKey,
  fieldRowKey,
  fieldRows,
  readBinds,
  relationshipRowKey,
  relationshipRows,
  setBinds,
  setFieldExpression,
  setRelationshipExpression,
  type RelationshipKind,
} from '@/lib/permissions/document';
import type { Rules } from '@/lib/permissions/model';
import { celContextFor } from '@/lib/permissions/editor-context';
import { CelExpressionField } from './cel-expression-field';

export interface AdvancedSectionProps {
  namespace: string;
  rules: Rules;
  schema: AppSchema | undefined;
  readOnly: boolean;
  errors: Map<string, string[]>;
  onChange: (rules: Rules) => void;
}

const BIND_NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;

const RELATIONSHIP_TITLES: Record<RelationshipKind, string> = {
  link: 'Link rules',
  unlink: 'Unlink rules',
};

/**
 * C.10 Advanced: link/unlink expressions, field view expressions and bind
 * definitions. Binds keep the document's array or object shape; new rows use
 * the object form the plan specifies.
 */
export function AdvancedSection({
  namespace,
  rules,
  schema,
  readOnly,
  errors,
  onChange,
}: AdvancedSectionProps) {
  const [open, setOpen] = useState(false);
  const binds = readBinds(rules, namespace);
  const fields = fieldRows(rules, namespace);
  const namespaceSchema = schema?.namespaces.find(
    (candidate) => candidate.name === namespace,
  );
  const linkLabels = namespaceSchema?.links.map((link) => link.label) ?? [];
  const attributeNames =
    namespaceSchema?.attributes
      .map((attribute) => attribute.name)
      .filter((name) => name !== 'id') ?? [];

  return (
    <section className="border-border border-t pt-3">
      <button
        type="button"
        className="text-muted-foreground flex min-h-8 items-center gap-1 text-sm font-medium"
        aria-expanded={open}
        onClick={() => {
          setOpen((previous) => !previous);
        }}
      >
        <ChevronRight
          className={`size-4 transition-transform ${open ? 'rotate-90' : ''}`}
          aria-hidden="true"
        />
        Advanced: link / unlink · field rules · bind
      </button>

      {open ? (
        <div className="mt-3 flex flex-col gap-6">
          {(['link', 'unlink'] as const).map((kind) => {
            const rows = relationshipRows(rules, namespace, kind);
            const available = linkLabels.filter(
              (label) => !rows.some((row) => row.name === label),
            );
            return (
              <div key={kind} className="flex flex-col gap-2">
                <div>
                  <h3 className="text-section">{RELATIONSHIP_TITLES[kind]}</h3>
                  <p className="text-muted-foreground text-xs">
                    {kind === 'link'
                      ? 'Who may attach a linked entity.'
                      : 'Who may detach a linked entity.'}
                  </p>
                </div>
                {rows.length === 0 ? (
                  <p className="text-muted-foreground text-xs">
                    No {kind} rules.
                  </p>
                ) : (
                  rows.map((row) => (
                    <div
                      key={row.name}
                      className="flex flex-col gap-2 md:flex-row md:items-start"
                    >
                      <span className="text-cell w-40 shrink-0 font-mono md:pt-2">
                        {row.name}
                      </span>
                      <div className="min-w-0 flex-1">
                        <CelExpressionField
                          value={row.expression}
                          aria-label={`${namespace} ${kind} ${row.name} rule`}
                          completions={celCompletionSource(
                            celContextFor(schema, rules, namespace, kind),
                          )}
                          readOnly={readOnly}
                          serverError={
                            errors.get(
                              relationshipRowKey(namespace, kind, row.name),
                            )?.[0]
                          }
                          onChange={(value) => {
                            onChange(
                              setRelationshipExpression(
                                rules,
                                namespace,
                                kind,
                                row.name,
                                value.trim() === '' ? null : value,
                              ),
                            );
                          }}
                        />
                      </div>
                      <IconButton
                        aria-label={`Remove ${kind} rule for ${row.name}`}
                        icon={<Trash2 className="size-4" aria-hidden="true" />}
                        disabled={readOnly}
                        onClick={() => {
                          onChange(
                            setRelationshipExpression(
                              rules,
                              namespace,
                              kind,
                              row.name,
                              null,
                            ),
                          );
                        }}
                      />
                    </div>
                  ))
                )}
                {!readOnly && available.length > 0 ? (
                  <AddRow
                    label={`Add ${kind} rule`}
                    options={available}
                    onAdd={(label) => {
                      onChange(
                        setRelationshipExpression(
                          rules,
                          namespace,
                          kind,
                          label,
                          'true',
                        ),
                      );
                    }}
                  />
                ) : null}
              </div>
            );
          })}

          <div className="flex flex-col gap-2">
            <div>
              <h3 className="text-section">Field rules</h3>
              <p className="text-muted-foreground text-xs">
                Who may read each attribute. The object view rule still applies.
              </p>
            </div>
            {fields.length === 0 ? (
              <p className="text-muted-foreground text-xs">No field rules.</p>
            ) : (
              fields.map((row) => (
                <div
                  key={row.name}
                  className="flex flex-col gap-2 md:flex-row md:items-start"
                >
                  <span className="text-cell w-40 shrink-0 font-mono md:pt-2">
                    {row.name}
                  </span>
                  <div className="min-w-0 flex-1">
                    <CelExpressionField
                      value={row.expression}
                      aria-label={`${namespace} field ${row.name} rule`}
                      completions={celCompletionSource(
                        celContextFor(schema, rules, namespace, 'view'),
                      )}
                      readOnly={readOnly}
                      serverError={
                        errors.get(fieldRowKey(namespace, row.name))?.[0]
                      }
                      onChange={(value) => {
                        onChange(
                          setFieldExpression(
                            rules,
                            namespace,
                            row.name,
                            value.trim() === '' ? null : value,
                          ),
                        );
                      }}
                    />
                  </div>
                  <IconButton
                    aria-label={`Remove field rule for ${row.name}`}
                    icon={<Trash2 className="size-4" aria-hidden="true" />}
                    disabled={readOnly}
                    onClick={() => {
                      onChange(
                        setFieldExpression(rules, namespace, row.name, null),
                      );
                    }}
                  />
                </div>
              ))
            )}
            {!readOnly ? (
              <AddRow
                label="Add field rule"
                options={attributeNames.filter(
                  (name) => !fields.some((row) => row.name === name),
                )}
                onAdd={(name) => {
                  onChange(setFieldExpression(rules, namespace, name, 'true'));
                }}
              />
            ) : null}
          </div>

          <div className="flex flex-col gap-2">
            <div>
              <h3 className="text-section">Bind</h3>
              <p className="text-muted-foreground text-xs">
                Named CEL expressions other rules can reuse.
              </p>
            </div>
            {binds.rows.length === 0 ? (
              <p className="text-muted-foreground text-xs">No binds.</p>
            ) : (
              binds.rows.map((row) => (
                <div
                  key={row.name}
                  className="flex flex-col gap-2 md:flex-row md:items-start"
                >
                  <span className="text-cell w-40 shrink-0 font-mono md:pt-2">
                    {row.name}
                  </span>
                  <div className="min-w-0 flex-1">
                    <CelExpressionField
                      value={row.expression}
                      aria-label={`${namespace} bind ${row.name}`}
                      completions={celCompletionSource(
                        celContextFor(schema, rules, namespace, 'update'),
                      )}
                      readOnly={readOnly}
                      serverError={
                        errors.get(bindRowKey(namespace, row.name))?.[0]
                      }
                      onChange={(value) => {
                        onChange(
                          setBinds(
                            rules,
                            namespace,
                            binds.rows.map((candidate) =>
                              candidate.name === row.name
                                ? { ...candidate, expression: value }
                                : candidate,
                            ),
                            binds.shape,
                          ),
                        );
                      }}
                    />
                  </div>
                  <IconButton
                    aria-label={`Remove bind ${row.name}`}
                    icon={<Trash2 className="size-4" aria-hidden="true" />}
                    disabled={readOnly}
                    onClick={() => {
                      onChange(
                        setBinds(
                          rules,
                          namespace,
                          binds.rows.filter(
                            (candidate) => candidate.name !== row.name,
                          ),
                          binds.shape,
                        ),
                      );
                    }}
                  />
                </div>
              ))
            )}
            {!readOnly ? (
              <AddBind
                rules={rules}
                namespace={namespace}
                binds={binds}
                onChange={onChange}
              />
            ) : null}
          </div>
        </div>
      ) : null}
    </section>
  );
}

function AddRow({
  label,
  options,
  onAdd,
}: {
  label: string;
  options: readonly string[];
  onAdd: (value: string) => void;
}) {
  const [selected, setSelected] = useState('');
  if (options.length === 0) return null;
  return (
    <div className="flex flex-wrap items-center gap-2">
      <Select
        value={selected}
        onValueChange={(value) => {
          if (typeof value === 'string') setSelected(value);
        }}
      >
        <SelectTrigger aria-label={label} className="w-48">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {options.map((option) => (
            <SelectItem key={option} value={option}>
              {option}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      <Button
        type="button"
        variant="secondary"
        size="sm"
        aria-label={label}
        disabled={selected === ''}
        onClick={() => {
          onAdd(selected);
          setSelected('');
        }}
      >
        <Plus className="size-4" aria-hidden="true" />
        Add
      </Button>
    </div>
  );
}

function AddBind({
  rules,
  namespace,
  binds,
  onChange,
}: {
  rules: Rules;
  namespace: string;
  binds: ReturnType<typeof readBinds>;
  onChange: (rules: Rules) => void;
}) {
  const [name, setName] = useState('');
  const duplicate = binds.rows.some((row) => row.name === name);
  const valid = BIND_NAME.test(name) && !duplicate;
  return (
    <div className="flex flex-wrap items-center gap-2">
      <Input
        aria-label="New bind name"
        placeholder="bind name"
        value={name}
        className="w-48"
        onChange={(event) => {
          setName(event.target.value);
        }}
      />
      <Button
        type="button"
        variant="secondary"
        size="sm"
        disabled={!valid}
        onClick={() => {
          onChange(
            setBinds(
              rules,
              namespace,
              [...binds.rows, { name, expression: 'true' }],
              binds.shape,
            ),
          );
          setName('');
        }}
      >
        <Plus className="size-4" aria-hidden="true" />
        Add bind
      </Button>
      {name !== '' && !valid ? (
        <p className="text-destructive basis-full text-xs">
          {duplicate
            ? 'That bind name already exists.'
            : 'Use letters, numbers and underscores; start with a letter or underscore.'}
        </p>
      ) : null}
    </div>
  );
}
